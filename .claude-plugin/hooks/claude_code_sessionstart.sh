#!/usr/bin/env bash
# Wake-up envelope is rendered by `mem wake-up --format hook-session-start`
# itself; this wrapper only exists so the plugin manifest keeps a stable
# command path. Errors silently fall back to `{}` (skip injection) because
# `mem wake-up` already does that internally on empty body.
set -euo pipefail

BUDGET="${MEM_WAKEUP_TOKEN_BUDGET:-800}"

# Scope the wake-up to the current repo so the boot context is about THIS
# project, not whatever was globally freshest. Derive the dir name and
# pass it as repo/project scope filters — but ONLY if this mem build
# supports `--scope` (older installed binaries would error on the unknown
# flag and inject nothing, breaking wake-up entirely). Detect via --help.
SCOPE_ARGS=()
PROJ_DIR="${CLAUDE_PROJECT_DIR:-$PWD}"
BASE=$(basename "$PROJ_DIR" 2>/dev/null || echo "")
if [ -n "$BASE" ] && mem wake-up --help 2>/dev/null | grep -q -- '--scope'; then
    SCOPE_ARGS=(--scope "repo:$BASE" --scope "project:$BASE")
fi

exec mem wake-up --tenant local --token-budget "$BUDGET" --format hook-session-start \
    ${SCOPE_ARGS[@]+"${SCOPE_ARGS[@]}"}
