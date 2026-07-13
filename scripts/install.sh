#!/usr/bin/env bash
#
# install.sh — install the `mem` memory service as a managed background
# service via systemd or supervisor.
#
# Builds (or installs a prebuilt) `mem` binary, creates a dedicated
# system user + data dir + env file, writes a systemd unit OR a
# supervisor program, then enables and starts it.
#
# Quick start (from the repo root, as root):
#     sudo ./scripts/install.sh
#
# It auto-detects the init system: systemd if `systemctl` is present,
# else supervisor if `supervisorctl` is present. Override with
# `--init-system`. Re-running is idempotent (config.env is never
# overwritten). Uninstall with `./scripts/install.sh --uninstall`.
#
set -euo pipefail

# ── Defaults (override via flags) ───────────────────────────────────
INIT_SYSTEM="auto"          # auto | systemd | supervisor
PREFIX="/usr/local"         # binary → $PREFIX/bin/mem
DATA_DIR="/var/lib/mem"     # Lance datasets + config.env + model cache
LOG_DIR="/var/log/mem"      # supervisor logs (systemd uses journald)
SVC_USER="mem"              # service runs as this (system) user
BIND_ADDR="127.0.0.1:3000"  # HTTP bind
TENANT="local"
PROVIDER="fake"             # fake | embedanything | openai
BINARY=""                   # prebuilt binary path; empty → build/use target/release
DO_BUILD="auto"             # auto | never  (auto = build if no binary found)
DO_START=1                  # 0 → install but don't start
UNINSTALL=0
SERVICE_NAME="mem"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd -P)"

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    sed -n '/^# install\.sh —/,/Uninstall with/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    cat <<EOF

Options:
  --init-system <auto|systemd|supervisor>   service manager (default: auto)
  --prefix <dir>          install prefix; binary → <prefix>/bin/mem (default: ${PREFIX})
  --data-dir <dir>        DB + config.env + model cache (default: ${DATA_DIR})
  --user <name>           run-as system user (default: ${SVC_USER})
  --bind <host:port>      HTTP bind address (default: ${BIND_ADDR})
  --tenant <name>         default tenant (default: ${TENANT})
  --provider <fake|embedanything|openai>    embedding provider (default: ${PROVIDER})
  --binary <path>         use a prebuilt mem binary instead of building
  --no-build              never run cargo; require a prebuilt binary
  --no-start              install + enable but do not start the service now
  --uninstall             stop + remove the service, binary and unit (keeps ${DATA_DIR})
  -h, --help              show this help

Examples:
  sudo ./scripts/install.sh                          # build + systemd, fake embeddings
  sudo ./scripts/install.sh --provider embedanything # local Qwen3 (download model first)
  sudo ./scripts/install.sh --init-system supervisor --bind 0.0.0.0:3000
  sudo ./scripts/install.sh --binary /tmp/mem --no-build
  sudo ./scripts/install.sh --uninstall
EOF
}

# ── Arg parsing ─────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --init-system) INIT_SYSTEM="$2"; shift 2 ;;
        --prefix)      PREFIX="$2"; shift 2 ;;
        --data-dir)    DATA_DIR="$2"; shift 2 ;;
        --log-dir)     LOG_DIR="$2"; shift 2 ;;
        --user)        SVC_USER="$2"; shift 2 ;;
        --bind)        BIND_ADDR="$2"; shift 2 ;;
        --tenant)      TENANT="$2"; shift 2 ;;
        --provider)    PROVIDER="$2"; shift 2 ;;
        --binary)      BINARY="$2"; shift 2 ;;
        --no-build)    DO_BUILD="never"; shift ;;
        --no-start)    DO_START=0; shift ;;
        --uninstall)   UNINSTALL=1; shift ;;
        -h|--help)     usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

BIN_DST="${PREFIX}/bin/${SERVICE_NAME}"
CONFIG_ENV="${DATA_DIR}/config.env"
SYSTEMD_UNIT="/etc/systemd/system/${SERVICE_NAME}.service"
SUPERVISOR_CONF="/etc/supervisor/conf.d/${SERVICE_NAME}.conf"

[[ "$(id -u)" -eq 0 ]] || die "must run as root (use sudo). System install touches ${PREFIX}, ${DATA_DIR}, the unit file."

# ── Resolve init system ─────────────────────────────────────────────
resolve_init_system() {
    if [[ "$INIT_SYSTEM" == "auto" ]]; then
        if command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]]; then
            INIT_SYSTEM="systemd"
        elif command -v supervisorctl >/dev/null 2>&1; then
            INIT_SYSTEM="supervisor"
        else
            die "could not auto-detect an init system; install systemd or supervisor, or pass --init-system"
        fi
    fi
    case "$INIT_SYSTEM" in
        systemd)    command -v systemctl   >/dev/null 2>&1 || die "systemd selected but systemctl not found" ;;
        supervisor) command -v supervisorctl >/dev/null 2>&1 || die "supervisor selected but supervisorctl not found" ;;
        *) die "invalid --init-system: ${INIT_SYSTEM} (expected systemd or supervisor)" ;;
    esac
    log "init system: ${INIT_SYSTEM}"
}

# ── Uninstall ───────────────────────────────────────────────────────
do_uninstall() {
    resolve_init_system
    if [[ "$INIT_SYSTEM" == "systemd" ]]; then
        systemctl stop    "${SERVICE_NAME}" 2>/dev/null || true
        systemctl disable "${SERVICE_NAME}" 2>/dev/null || true
        rm -f "${SYSTEMD_UNIT}"
        systemctl daemon-reload
    else
        supervisorctl stop "${SERVICE_NAME}" 2>/dev/null || true
        rm -f "${SUPERVISOR_CONF}"
        supervisorctl reread 2>/dev/null || true
        supervisorctl update 2>/dev/null || true
    fi
    rm -f "${BIN_DST}"
    log "removed service + binary. Data preserved at ${DATA_DIR} (remove manually if you want a clean wipe)."
    exit 0
}
[[ "$UNINSTALL" -eq 1 ]] && do_uninstall

resolve_init_system

# ── 1. Obtain the binary ────────────────────────────────────────────
install_binary() {
    local src="$BINARY"
    if [[ -z "$src" ]]; then
        # No explicit binary: prefer an existing release build, else build.
        if [[ -x "${REPO_ROOT}/target/release/${SERVICE_NAME}" ]]; then
            src="${REPO_ROOT}/target/release/${SERVICE_NAME}"
            log "using existing release binary: ${src}"
        elif [[ "$DO_BUILD" == "never" ]]; then
            die "--no-build set but no binary at ${REPO_ROOT}/target/release/${SERVICE_NAME}; pass --binary"
        else
            command -v cargo >/dev/null 2>&1 || die "cargo not found — install Rust, or build elsewhere and pass --binary"
            log "building release binary (cargo build --release)…"
            ( cd "$REPO_ROOT" && cargo build --release --bin "${SERVICE_NAME}" )
            src="${REPO_ROOT}/target/release/${SERVICE_NAME}"
        fi
    fi
    [[ -x "$src" ]] || die "binary not found or not executable: ${src}"
    install -d -m 0755 "${PREFIX}/bin"
    install -m 0755 "$src" "$BIN_DST"
    log "installed binary → ${BIN_DST}"
}

# ── 2. Service user + data dirs ─────────────────────────────────────
setup_user_and_dirs() {
    if ! id -u "$SVC_USER" >/dev/null 2>&1; then
        log "creating system user: ${SVC_USER} (home=${DATA_DIR}, nologin)"
        useradd --system --home-dir "$DATA_DIR" --shell /usr/sbin/nologin "$SVC_USER" \
            2>/dev/null || useradd --system --home-dir "$DATA_DIR" --shell /bin/false "$SVC_USER"
    fi
    install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$DATA_DIR"
    install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$DATA_DIR/hf-cache"
    install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$LOG_DIR"
}

# Dataset dir under DATA_DIR. Fresh installs get the honest `mem.lance`
# name; a pre-route-B install whose config.env was lost keeps pointing at
# its existing `mem.duckdb` dir (the name is historical — the contents
# are Lance datasets either way) instead of silently starting empty.
db_path() {
    if [[ -e "${DATA_DIR}/mem.duckdb" ]]; then
        echo "${DATA_DIR}/mem.duckdb"
    else
        echo "${DATA_DIR}/mem.lance"
    fi
}

# ── 3. config.env (idempotent — never clobber an edited one) ─────────
write_config_env() {
    if [[ -f "$CONFIG_ENV" ]]; then
        log "config.env already exists, leaving it untouched: ${CONFIG_ENV}"
        return
    fi
    cat > "$CONFIG_ENV" <<EOF
# mem service config — generated by scripts/install.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)
# Edit and restart the service to apply (systemctl restart ${SERVICE_NAME}
# / supervisorctl restart ${SERVICE_NAME}). Re-running install.sh will NOT
# overwrite this file.

# Values are intentionally unquoted: this file is read both by systemd
# EnvironmentFile= (which can mishandle surrounding quotes) and by a
# shell 'source' under supervisor. None of the values contain spaces.
#
# ── Storage (Lance dataset dir, single-writer — never point two services here) ──
MEM_DB_PATH=$(db_path)
MEM_TENANT=${TENANT}

# ── HTTP bind ──
BIND_ADDR=${BIND_ADDR}

# ── Embedding provider ──
# fake          = offline, deterministic hash vectors (no model needed)
# embedanything = local CPU Qwen3-Embedding-0.6B (run scripts/download_model.py
#                 first; HF_HOME below points the cache into the data dir)
# openai        = needs OPENAI_API_KEY; sends content off this machine
EMBEDDING_PROVIDER=${PROVIDER}
HF_HOME=${DATA_DIR}/hf-cache
# OPENAI_API_KEY=
# MEM_PRIVACY_WARN_SUPPRESS=1

# ── Optional worker tuning (in-binary defaults shown) ──
# MEM_VACUUM_DISABLED=1
# MEM_AUTO_PROMOTE_DISABLED=1
# MEM_DEDUP_ENABLED=1
# MEM_INGEST_NEARDUP_ENABLED=1     # O2 write-time near-dup review flagging
# MEM_RECALL_PER_SOURCE_CAP=3      # O3 recall diversity cap (0 disables)
EOF
    chown "$SVC_USER:$SVC_USER" "$CONFIG_ENV"
    chmod 0640 "$CONFIG_ENV"
    log "wrote ${CONFIG_ENV} (provider=${PROVIDER}, bind=${BIND_ADDR})"
}

# ── 4a. systemd unit ────────────────────────────────────────────────
write_systemd_unit() {
    cat > "$SYSTEMD_UNIT" <<EOF
[Unit]
Description=mem — local-first memory service for multi-agent workflows
Documentation=https://github.com/BenLocal/mem
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${SVC_USER}
Group=${SVC_USER}
WorkingDirectory=${DATA_DIR}
EnvironmentFile=${CONFIG_ENV}
ExecStart=${BIN_DST} serve
# mem serve has crashed before (a Lance vacuum edge case); always recover.
Restart=on-failure
RestartSec=3
TimeoutStopSec=15
# Hardening — the service only needs its own data dir writable.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=${DATA_DIR} ${LOG_DIR}

[Install]
WantedBy=multi-user.target
EOF
    log "wrote ${SYSTEMD_UNIT}"
    systemctl daemon-reload
    systemctl enable "${SERVICE_NAME}" >/dev/null
    if [[ "$DO_START" -eq 1 ]]; then
        systemctl restart "${SERVICE_NAME}"
        log "started ${SERVICE_NAME} (systemctl status ${SERVICE_NAME})"
    else
        log "enabled ${SERVICE_NAME} (not started; --no-start). Start with: systemctl start ${SERVICE_NAME}"
    fi
}

# ── 4b. supervisor program ──────────────────────────────────────────
write_supervisor_conf() {
    [[ -d "$(dirname "$SUPERVISOR_CONF")" ]] || die "supervisor conf dir $(dirname "$SUPERVISOR_CONF") missing; is supervisor installed/configured?"
    # supervisor has no EnvironmentFile — source config.env in a sh -c wrapper.
    cat > "$SUPERVISOR_CONF" <<EOF
[program:${SERVICE_NAME}]
command=/bin/sh -c 'set -a; . ${CONFIG_ENV}; set +a; exec ${BIN_DST} serve'
directory=${DATA_DIR}
user=${SVC_USER}
autostart=true
autorestart=true
startsecs=3
stopwaitsecs=15
stopsignal=TERM
stdout_logfile=${LOG_DIR}/${SERVICE_NAME}.out.log
stderr_logfile=${LOG_DIR}/${SERVICE_NAME}.err.log
stdout_logfile_maxbytes=10MB
stderr_logfile_maxbytes=10MB
EOF
    log "wrote ${SUPERVISOR_CONF}"
    supervisorctl reread
    supervisorctl update
    if [[ "$DO_START" -eq 1 ]]; then
        supervisorctl restart "${SERVICE_NAME}" 2>/dev/null || supervisorctl start "${SERVICE_NAME}"
        log "started ${SERVICE_NAME} (supervisorctl status ${SERVICE_NAME})"
    else
        log "registered ${SERVICE_NAME} (not started; --no-start). Start with: supervisorctl start ${SERVICE_NAME}"
    fi
}

# ── 5. Health check ─────────────────────────────────────────────────
health_check() {
    [[ "$DO_START" -eq 1 ]] || return 0
    local url="http://${BIND_ADDR}/health"
    command -v curl >/dev/null 2>&1 || { warn "curl not found, skipping health check"; return 0; }
    log "waiting for ${url} …"
    for _ in $(seq 1 20); do
        if curl -fsS --max-time 2 "$url" >/dev/null 2>&1; then
            log "health OK — ${url} is up"
            return 0
        fi
        sleep 1
    done
    warn "service did not pass health check within 20s; inspect logs:"
    if [[ "$INIT_SYSTEM" == "systemd" ]]; then
        warn "  journalctl -u ${SERVICE_NAME} -n 50 --no-pager"
    else
        warn "  tail -n 50 ${LOG_DIR}/${SERVICE_NAME}.err.log"
    fi
}

# ── Run ─────────────────────────────────────────────────────────────
install_binary
setup_user_and_dirs
write_config_env
if [[ "$INIT_SYSTEM" == "systemd" ]]; then
    write_systemd_unit
else
    write_supervisor_conf
fi
health_check

cat <<EOF

$(log "mem installed.")
  binary   : ${BIN_DST}
  data dir : ${DATA_DIR}   (DB: $(grep -m1 '^MEM_DB_PATH=' "$CONFIG_ENV" | cut -d= -f2- | grep . || db_path))
  config   : ${CONFIG_ENV}
  bind     : ${BIND_ADDR}    provider: ${PROVIDER}
  manage   : $( [[ "$INIT_SYSTEM" == systemd ]] \
                  && echo "systemctl {status,restart,stop} ${SERVICE_NAME} ; journalctl -u ${SERVICE_NAME} -f" \
                  || echo "supervisorctl {status,restart,stop,tail -f} ${SERVICE_NAME}" )
  verify   : curl http://${BIND_ADDR}/health
EOF
[[ "$PROVIDER" == "embedanything" ]] && \
    warn "provider=embedanything needs the Qwen3 model: run 'HF_HOME=${DATA_DIR}/hf-cache ${SCRIPT_DIR}/download_model.py' (as ${SVC_USER}) then restart."
exit 0
