# mem

Local-first Rust `axum` memory service for multi-agent engineering workflows. The MVP supports memory ingest, pending review, detail lookup, graph diagnostics, compressed search, feedback updates, and episode-driven workflow extraction, backed by on-disk Lance datasets (read natively via the lancedb Rust API — vector ANN via lance `nearest_to`, BM25 via an in-RAM Tantivy index, hybrid fused with RRF in Rust). Beyond CRUD, memories have a **lifecycle**: retrieval reinforcement, time decay, hard expiry, opt-in governance sweeps (idle-archive, ingest quality gate), and an opt-in **self-evolution** worker that merges and generalizes related capsules — every governance / evolution path defaults **OFF**, previews via dry-run, and is verbatim-safe (never rewrites or physically deletes a fact).

## Run Locally

```bash
cargo run
```

The server binds to `127.0.0.1:3000` by default. Set `MEM_DB_PATH` to point at a specific Lance dataset directory if you do not want to use the default local dev path.

## Codex / MCP (shared memory)

`mem` ships its own MCP stdio server in the same binary — no Node, no npm.

```bash
# In one terminal: run the HTTP service.
mem serve

# In another (or wired into Codex / Cursor): run the MCP stdio server.
mem mcp
```

The MCP server forwards 20 tools to the HTTP service over `MEM_BASE_URL` (default `http://127.0.0.1:3000`). Configuration env vars:

| Variable | Default | Meaning |
|----------|---------|---------|
| `MEM_BASE_URL` | `http://127.0.0.1:3000` | `mem serve` HTTP root |
| `MEM_TENANT` | `local` | Default tenant when a tool omits it |
| `MEM_MCP_EXPOSE_EMBEDDINGS` | unset | Set to `1` to enable admin `embeddings_*` tools |

## Storage backends

`mem` has three interchangeable storage backends behind one `Backend` trait, all compiled in and selected at runtime via `MEM_BACKEND`. The default (Lance) needs no setup; Postgres and ClickHouse are for teams that already run them and want a single shared store. `mem sync` (below) migrates data between any two of them.

| Backend | Select with | Storage | Notes |
|---------|-------------|---------|-------|
| **Lance** (default) | `MEM_BACKEND=lance` (or unset) | On-disk Lance datasets at `MEM_DB_PATH`, read natively via the lancedb Rust API — vector ANN via lance `nearest_to`, BM25 via an in-RAM Tantivy index, hybrid fused with RRF in Rust | Zero external services. The standard single-node deployment. |
| **Postgres** | `MEM_BACKEND=postgres` + `MEM_POSTGRES_URL=…` | A Postgres database with the [`pgvector`](https://github.com/pgvector/pgvector) extension | ANN via pgvector, BM25 via `tsvector`/GIN, hybrid recall fused with RRF. |
| **ClickHouse** | `MEM_BACKEND=clickhouse` + `MEM_CLICKHOUSE_URL=…` (creds in the URL) | A ClickHouse server (columnar / OLAP) | ANN via `cosineDistance` over `Array(Float32)`, lexical via substring/token candidates, hybrid fused with RRF in Rust; the update-heavy lifecycle (decay / status / supersede) is modeled as versioned re-inserts into `ReplacingMergeTree`. See [`docs/clickhouse-backend.md`](docs/clickhouse-backend.md). |

All three backends are compiled into every build (default dependencies — no cargo feature flags), so selecting one is purely a runtime choice:

```bash
# Run with the Postgres backend — no special build flags.
cargo build
MEM_BACKEND=postgres \
MEM_POSTGRES_URL='postgres://user:pass@localhost:5432/mem' \
  ./target/debug/mem serve     # or target/release/mem after a release build
```

- The database must have `pgvector` available (`CREATE EXTENSION vector;` — the image [`pgvector/pgvector:pg16`](https://hub.docker.com/r/pgvector/pgvector) ships it). Schema migrations (`migrations/postgres/0001`–`0004`) are applied idempotently on connect; no manual setup needed.
- Embedding dimension is provider-dependent (default 1024). The pgvector embedding tables are lazy-created on first upsert with the running provider's dim — changing the embedding provider/dim means recreating those tables.
- Selecting `MEM_BACKEND=postgres` (or `clickhouse`) without its `MEM_*_URL` set is a startup error; with `MEM_BACKEND` unset (or `lance`), Lance is always used.

### Migrating between backends (`mem sync`)

`mem sync` copies all data **verbatim** from one backend to another — any → any across Lance / Postgres / ClickHouse — so you can migrate a local Lance store into Postgres or ClickHouse (or roll back):

```bash
# Dry-run first (reads + counts, writes nothing):
mem sync --from lance:/root/.mem/mem.lance \
         --to clickhouse:http://mem:mem@localhost:8123 \
         --tenant local --dry-run --verbose

# Then for real:
mem sync --from lance:/root/.mem/mem.lance \
         --to clickhouse:http://mem:mem@localhost:8123 \
         --tenant local
```

- **`--from` / `--to`** are `<kind>:<locator>` specs: `lance:<dir>`, `postgres:<url>`, `clickhouse:<url>`.
- **`--tenant`** is required and repeatable — there is no tenant-enumeration read, so name each tenant explicitly.
- **`--domains`** copies a subset (default: all five, in dependency order): `entities,capsules,episodes,transcripts,graph`.
- Rows are copied **verbatim** (original ids / timestamps / lifecycle state preserved), not re-ingested, and the copy is **idempotent** — re-running skips rows already present in the target, so it resumes after an interruption.
- **Embeddings are rebuilt on the target:** sync copies rows + enqueues embedding jobs; the target's `mem serve` embedding worker fills the vectors (source and target may use different embedding providers/dims).

**Known v1 limitations** (all from using only existing read methods): tenants must be named explicitly; entity-table rows are best-effort — `resolve_or_create` remints `entity_id`, so migrated entities won't link to the copied edges (the graph edges themselves are copied verbatim); only active graph edges are reconstructed; operational tables (embedding jobs, mine cursors, raw sessions) are skipped. Opening a Lance **source** holds an advisory single-writer lock, so stop any `mem serve` running on the source dir during migration.

## Cross-compile server (Linux binary)

The config file is **`Cross.toml`** at the repo root (the `cross` CLI always reads that exact filename; set the `CROSS_CONFIG` environment variable to use a different path).

```bash
cargo install cross --locked
cross build --release
```

Binary: `target/x86_64-unknown-linux-gnu/release/mem`. Static **musl** build:

```bash
cross build --release --target x86_64-unknown-linux-musl
```

Output: `target/x86_64-unknown-linux-musl/release/mem` (suitable for glibc-free environments such as Alpine).

Docker must be installed and running locally (`cross` provides the linking environment through a container). If a target fails to compile (for example a native dependency's build script), upgrade `cross` first, or switch that `target` to a newer `image` tag in `Cross.toml`.

CI (`.github/workflows/ci.yml`) runs **`cross build --release`** on PRs / pushes, targeting **`x86_64-unknown-linux-gnu`** and **`x86_64-unknown-linux-musl`** (matching `Cross.toml` / the Docker builder). On a `v*.*.*` tag, the Release workflow uploads both **`mem-<tag>-x86_64-unknown-linux-gnu`** and **`mem-<tag>-x86_64-unknown-linux-musl`** to the GitHub Release.

## Service install (systemd / supervisor)

[`scripts/install.sh`](scripts/install.sh) installs `mem serve` as a managed
background service. It auto-detects the init system (systemd if present, else
supervisor), builds (or takes a prebuilt) binary, creates a dedicated `mem`
system user + `/var/lib/mem` data dir + `config.env`, writes the unit, then
enables, starts, and health-checks it.

```bash
sudo ./scripts/install.sh                          # build + systemd, fake embeddings
sudo ./scripts/install.sh --init-system supervisor --bind 0.0.0.0:3000
sudo ./scripts/install.sh --binary /path/to/mem --no-build   # use a cross-built binary
sudo ./scripts/install.sh --provider embedanything # local Qwen3 (download model first)
sudo ./scripts/install.sh --uninstall              # stop + remove (keeps the data dir)
```

Re-running is idempotent (`config.env` is never overwritten); see
`./scripts/install.sh --help` for all flags. `Restart=on-failure` /
`autorestart=true` keeps the single-writer service up; never point two
instances at the same `MEM_DB_PATH`.

### Manual setup

Prefer wiring it up by hand? The script just emits the following — copy a
binary to `/usr/local/bin/mem`, create a `mem` user + `/var/lib/mem`, then
drop in the env + unit. Ready-to-copy templates (with step-by-step header
comments) live in [`deploy/`](deploy/): [`mem.config.env.example`](deploy/mem.config.env.example),
[`mem.service`](deploy/mem.service), [`mem.supervisor.conf`](deploy/mem.supervisor.conf).

`/var/lib/mem/config.env` (values **unquoted** so both systemd
`EnvironmentFile=` and a shell `source` read them identically):

```ini
MEM_DB_PATH=/var/lib/mem/mem.lance
MEM_TENANT=local
BIND_ADDR=127.0.0.1:3000
EMBEDDING_PROVIDER=fake        # fake | embedanything | openai
HF_HOME=/var/lib/mem/hf-cache  # model cache for embedanything
# OPENAI_API_KEY=
```

**systemd** — `/etc/systemd/system/mem.service`:

```ini
[Unit]
Description=mem — local-first memory service for multi-agent workflows
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=mem
Group=mem
WorkingDirectory=/var/lib/mem
EnvironmentFile=/var/lib/mem/config.env
ExecStart=/usr/local/bin/mem serve
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/mem /var/log/mem

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now mem
sudo systemctl status mem ; journalctl -u mem -f
```

**supervisor** — `/etc/supervisor/conf.d/mem.conf` (no `EnvironmentFile`, so
`source` the env file in a wrapper):

```ini
[program:mem]
command=/bin/sh -c 'set -a; . /var/lib/mem/config.env; set +a; exec /usr/local/bin/mem serve'
directory=/var/lib/mem
user=mem
autostart=true
autorestart=true
startsecs=3
stopwaitsecs=15
stopsignal=TERM
stdout_logfile=/var/log/mem/mem.out.log
stderr_logfile=/var/log/mem/mem.err.log
```

```bash
sudo supervisorctl reread && sudo supervisorctl update
sudo supervisorctl status mem ; sudo supervisorctl tail -f mem
```

Verify either with `curl http://127.0.0.1:3000/health`.

## Docker (mem HTTP only)

Build and run locally (the build stage uses the same **cross-rs** `x86_64-unknown-linux-gnu` image as `Cross.toml`):

```bash
docker build -t mem:local .
docker run --rm -p 3000:3000 -v mem_data:/data mem:local
```

Example compose (build context is repo root): [deploy/docker-compose.yml](deploy/docker-compose.yml).

Default in the image: `BIND_ADDR=0.0.0.0:3000`, `MEM_DB_PATH=/data/mem.lance`. Point MCP clients at the same host with `MEM_BASE_URL` (for example `http://127.0.0.1:3000`).

## Release (GHCR + binaries)

1. Push a semver tag: `git tag v0.1.0 && git push origin v0.1.0` (triggers both **CI** and **Release**; the Docker image build uses the GitHub Actions cache to speed up repeated builds).
2. Workflow [`.github/workflows/release.yml`](.github/workflows/release.yml) pushes **`ghcr.io/<lowercase-owner>/mem:<tag>`** and **`:latest`**, and attaches **`mem-<tag>-x86_64-unknown-linux-gnu`**, **`mem-<tag>-x86_64-unknown-linux-musl`**, and **`mem-<tag>-SHA256SUMS`** (a `sha256sum` checksum file) to the GitHub Release. The MCP server is built into the binary, so no separate release is needed.

Point every client at the same `MEM_BASE_URL` and `tenant` so multiple Codex or Cursor processes share one store.

## API Smoke Checklist

```bash
curl localhost:3000/health
curl -X POST localhost:3000/memories \
  -H 'content-type: application/json' \
  -d '{
    "memory_type": "implementation",
    "content": "invalidate cache when schema changes",
    "scope": "repo",
    "write_mode": "auto",
    "tenant": "local"
  }'
curl localhost:3000/memories/mem_123
curl 'localhost:3000/reviews/pending?tenant=local'
curl -X POST localhost:3000/memories/search \
  -H 'content-type: application/json' \
  -d '{
    "query": "how should I debug invoice retry failures",
    "intent": "debugging",
    "scope_filters": ["repo:billing"],
    "token_budget": 300,
    "caller_agent": "codex-worker",
    "expand_graph": true,
    "tenant": "local"
  }'
curl -X POST localhost:3000/memories/feedback \
  -H 'content-type: application/json' \
  -d '{
    "tenant": "local",
    "memory_id": "mem_123",
    "feedback_kind": "useful"
  }'
curl -X POST localhost:3000/episodes \
  -H 'content-type: application/json' \
  -d '{
    "goal": "debug invoice retries",
    "steps": ["inspect logs", "trace job", "verify fix"],
    "outcome": "success"
  }'
curl localhost:3000/graph/neighbors/module:mem:invoice
```

Expected response shapes:
- `GET /health` returns plain text `ok`
- `POST /memories` returns `{ "memory_id": "...", "status": "..." }`
- `GET /memories/{id}` returns the full memory plus `version_chain`, `graph_links`, and `feedback_summary`
- `GET /reviews/pending` returns a JSON array of pending memories
- `POST /memories/search` returns `directives`, `relevant_facts`, `reusable_patterns`, and optional `suggested_workflow`
- `POST /memories/feedback` returns the updated memory record
- `POST /episodes` returns `{ "episode_id": "...", "status": "created", ... }`
- `GET /graph/neighbors/:node_id` returns a JSON array of graph edges

## Verification

```bash
cargo test -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## Design Principles

- **Verbatim discipline**: `memories.content` is the **fact source** — never rewritten or truncated at storage. `memories.summary` is **index/hint only** — never used as the basis for answers or quotes. When a caller provides an explicit `summary` field, the ingest pipeline rejects requests where `summary` equals `content` — preventing agents from copying refined text into the content field. When no caller summary is supplied, the server derives one from `content[:80]` for indexing only.
- **Lifecycle-aware**: memories have status (`Provisional`, `Active`, `PendingConfirmation`), confidence scores, decay, and feedback loops — not just CRUD operations.
- **Graph-temporal**: edges carry `valid_from`/`valid_to` timestamps for point-in-time queries and supersede chains.
- **Safe by default**: every destructive or generative background sweep (idle-archive, ingest quality gate, near-dup flagging, self-evolution) ships **default OFF**, exposes a `dry_run` HTTP preview that works regardless of the switch and writes nothing, and is **verbatim-safe** — a memory only ever leaves the active pool via a reversible status flip (`Archived`) or a supersede chain (`version_chain` keeps the old row), never a physical delete or a content rewrite.

## Lifecycle, Governance & Self-Evolution

On top of the per-memory lifecycle, `mem` runs a set of background workers that keep the pool healthy as it grows. Each is independently switchable; the destructive / generative ones are **opt-in** and have an HTTP dry-run preview so you can inspect candidates before flipping the switch.

### Retrieval reinforcement & decay

Every memory written into a search response is stamped `last_used_at` (off the read path, batched by the always-on `last_used_worker`). The decay clock runs from `last_used_at` when a memory has ever been used, else `updated_at`, so retrieved memories decay slower than untouched ones. A separate sweep-proof `last_recalled_at` column records the first real recall — it is what the idle-archive sweep below trusts (the decay sweep cannot forge it).

### Hard expiry (auto-forget)

A memory may carry an optional `expires_at` (20-digit ms timestamp). Once past, it is treated as expired: filtered out of search candidates (`is_expired` in the retrieve pipeline) and excluded from the evolution map. Set it at ingest time for facts with a known shelf life (a temporary credential, a sprint-scoped decision). Always on, no env needed — a memory with no `expires_at` never expires.

### Governance sweeps (opt-in, default OFF)

- **Idle-archive** (`idle_archive_worker`) — archives `Active` memories that are dead weight on *every* axis at once: never recalled (`last_recalled_at IS NULL`), aged past `age_days`, never positively reinforced, decayed past a floor, **and** structurally low-value. The structural clause keeps substantive lessons safe however idle they look. Archival reuses the feedback path, so the row is kept verbatim — only search drops it. Preview: `POST /reviews/idle_archive {"tenant":"local","dry_run":true}`.
- **Ingest quality gate** — rejects structurally low-value `experience` capsules at write time (too short, or a bare commit subject with no evidence / code_refs) instead of letting them accumulate as noise. Only `experience` is gated; every other type passes untouched.
- **Auto-promote** (`auto_promote_worker`, **default ON**) — promotes long-idle `PendingConfirmation` capsules to `Active`. Excludes `Preference` / `Workflow` and, since the self-evolution work, anything stamped `source_agent=evolution_worker` (evolution proposals must stay review-gated). Preview: `POST /reviews/auto_promote {"dry_run":true}`.

### Self-evolution (`evolution_worker`, opt-in, default OFF)

A daily sweep that treats the active pool as points on a semantic "living map" and structurally consolidates it — **LLM-free**, anti-jitter, verbatim-safe:

1. **Map** — clusters active memories over their *existing* embeddings (union-find on cosine; never calls embed itself).
2. **Anti-jitter gate** — each candidate operation accumulates evidence in a durable `evolution_candidates` table and only executes after its signal held for `K` **consecutive** sweeps (EvoMap-inspired temporal smoothing); evidence survives restarts.
3. **Operators**:
   - **① merge** — a cluster of near-identical memories collapses to the longest-content canonical; the rest flip to `Archived` (reversible) with `merged_into` lineage edges.
   - **② generalize** — a stable cluster of ≥4 episodic memories sharing ≥2 themes (`topics ∪ tags`) produces **one** `PendingConfirmation` proposal capsule built by the `review` `SynthesisBackend` (structured raw material, *no generated prose* — a human or the interactive agent writes the actual principle via review-edit-accept); the source memories stay `Active`.
4. **Preview** — `POST /reviews/evolution {"tenant":"local","dry_run":true}` returns the proposals + per-candidate evidence/cycle counts and writes **nothing** (not even candidate rows), regardless of the switch.

```bash
# Preview what the evolution worker would merge / generalize — zero writes.
curl -X POST localhost:3000/reviews/evolution \
  -H 'content-type: application/json' \
  -d '{"tenant":"local","dry_run":true}' | jq
```

### Governance & evolution env vars

All default to the safe value; the worker for any opt-in feature is simply not spawned when its `*_ENABLED` is unset.

| Variable | Default | Meaning |
|----------|---------|---------|
| `MEM_IDLE_ARCHIVE_ENABLED` | unset (OFF) | Spawn the idle-archive worker. Real sweeps are a no-op while off; the dry-run preview works regardless. |
| `MEM_INGEST_QUALITY_GATE_ENABLED` | unset (OFF) | Reject structurally low-value `experience` capsules at ingest. |
| `MEM_INGEST_MIN_CONTENT_LEN` | 40 | Minimum trimmed content length for a gated `experience` capsule. |
| `MEM_AUTO_PROMOTE_DISABLED` | unset (worker ON) | Opt **out** of the `PendingConfirmation → Active` sweep (this one is default ON). |
| `MEM_EVOLUTION_ENABLED` | unset (OFF) | Master switch for the self-evolution worker. |
| `MEM_EVOLUTION_K_CYCLES` | 3 | Consecutive sweeps a candidate must hold before it executes (anti-jitter gate). |
| `MEM_EVOLUTION_INTERVAL_SECS` | 86400 | Sweep cadence — one sweep is one cycle. Earliest real execution ≈ `K × interval` after start. |
| `MEM_EVOLUTION_SYNTHESIS` | `off` | `off` \| `review`. `review` defers generalize content to the pending-review queue (worker stays LLM-free); `local` / `api` are designed but unimplemented and rejected at parse. |
| `MEM_EVOLUTION_CLUSTER_THRESHOLD` / `MEM_EVOLUTION_MERGE_THRESHOLD` | 0.80 / 0.88 | Map-cluster vs ① merge cosine thresholds. |
| `MEM_EVOLUTION_GENERALIZE_MIN_N` | 4 | Minimum episodic members for a ② generalize proposal. |
| `MEM_EVOLUTION_EVIDENCE_DECAY` / `MEM_EVOLUTION_HYSTERESIS` | 0.7 / 0.5 | Evidence retention `β` and the cancel-below floor. |
| `MEM_EVOLUTION_SCAN_LIMIT` | 2000 | Per-sweep cap on candidate memories pulled. |

`POST /reviews/idle_archive`, `POST /reviews/auto_promote`, and `POST /reviews/evolution` all accept `{"tenant": "...", "dry_run": bool}` and default `dry_run` to `true` — the canonical flow is preview → review → enable the worker.

## Claude Code Integration

### Installation

1. **Install hooks**:

```bash
mkdir -p ~/.mem/hooks
cp hooks/claude_code_*.sh ~/.mem/hooks/
```

2. **Register hooks** in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": "~/.mem/hooks/claude_code_stop.sh",
    "PreCompact": "~/.mem/hooks/claude_code_precompact.sh",
    "SessionStart": "~/.mem/hooks/claude_code_sessionstart.sh"
  }
}
```

3. **(Optional) Create identity file**:

```bash
cat > ~/.mem/identity.txt <<EOF
I am a [role] working on [domain].
I prefer [preferences].
EOF
```

### Usage

Hooks run automatically:
- **Stop**: Every 15 exchanges, mines memories in background
- **PreCompact**: Before context compression, final mine
- **SessionStart**: Injects recent memories at session start

Manual commands:

```bash
# Mine a transcript (extract memories + archive blocks)
mem mine ~/.claude/projects/.../session.jsonl

# Bulk archive-only import of the whole transcript store (see Transcript Archive)
mem import claude-code

# Get wake-up context
mem wake-up --token-budget 800
```

## Transcript Archive (conversation_messages)

A second pipeline, fully isolated from `memories`, archives every Claude Code transcript block verbatim and exposes semantic search + ordered replay over those blocks. It exists alongside `memories` so the existing ranking / lifecycle / verbatim-guard surface is **untouched**: separate table (`conversation_messages`), separate embedding queue (`transcript_embedding_jobs`), and a separate Lance-native embedding table (`conversation_message_embeddings`). `mem mine` is now **dual-sink** — one transcript scan writes both extracted memories (existing path) and every block (text / tool_use / tool_result / thinking) to the archive.

**Bulk import (`mem import`)** — `mem mine` handles **one** transcript at a time (and also extracts memories). To back-fill the archive from an agent's entire transcript store in one pass — **archive-only**, no memory extraction, no `<mem-save>` parsing — use `mem import`:

```bash
# Archive every Claude Code transcript under ~/.claude/projects/**/*.jsonl.
mem import claude-code

# Scope to one project directory, or a single .jsonl file.
mem import claude-code --path ~/.claude/projects/-home-me-myrepo
mem import claude-code --path ~/.claude/projects/foo/abc.jsonl

# Parse + count only, without POSTing anything (sanity check).
mem import claude-code --dry-run --verbose
```

It POSTs to the service at `--base-url` (default `http://127.0.0.1:3000`), so `mem serve` must be running. **Idempotent**: the batch endpoint dedups server-side by `(transcript_path, line_number, block_index)`, so re-running over an already-imported store re-sends without double-inserting — safe to run repeatedly to pick up new blocks. The importer is **per-agent extensible** (`mem import <agent>`); `claude-code` is the first source.

```bash
# Ingest a single block (internal — `mem mine` POSTs these for you).
curl -X POST localhost:3000/transcripts/messages \
  -H 'content-type: application/json' \
  -d '{
    "tenant": "local",
    "caller_agent": "claude-code",
    "transcript_path": "/home/me/.claude/projects/foo/abc.jsonl",
    "line_number": 1,
    "block_index": 0,
    "role": "user",
    "block_type": "text",
    "content": "how do I debug invoice retry failures?",
    "embed_eligible": true,
    "created_at": "2026-04-30T10:00:00Z"
  }'

# Time-ordered replay of one session (verbatim transcript).
curl 'localhost:3000/transcripts?tenant=local&session_id=sess_abc'
```

**Search** (BM25 + HNSW hybrid; returns merged conversation windows):
```bash
curl -X POST localhost:3000/transcripts/search \
  -H 'content-type: application/json' \
  -d '{
    "query": "vector index",
    "tenant": "local",
    "limit": 5,
    "context_window": 2,
    "anchor_session_id": null,
    "include_tool_blocks_in_context": false
  }' | jq
```

Response shape: `{ "windows": [{ "session_id": "...", "blocks": [...], "primary_ids": [...], "score": 47 }] }`. Each window is a conversation snippet around one or more primary hits; `is_primary: true` flags the actual matches inside the `blocks` array.

**New request fields** (all optional; transcripts pipeline only):
- `anchor_session_id` — boost blocks from this session above topical matches; useful when continuing a known conversation.
- `context_window` — ±N blocks of context around each primary (default 2, cap 10).
- `include_tool_blocks_in_context` — include `tool_use` / `tool_result` blocks as context (default false; primary blocks always returned regardless of type).

**MCP does not expose transcript search by design** — agents go through `memory_search`, then use the resulting `session_id` to pull the surrounding transcript via the HTTP endpoints above.

| Variable | Default | Meaning |
|----------|---------|---------|
| `MEM_TRANSCRIPT_EMBED_DISABLED` | unset | Set to `1` to stop the transcript embedding worker (e.g. when using OpenAI to avoid double provider spend). Blocks still archive verbatim. |
| `MEM_TRANSCRIPT_OVERSAMPLE` | 4 | Candidate fan-out factor for transcript search (`k = limit * factor`). Read live in `TranscriptService::search`; invalid values (non-numeric or 0) silently fall back to default. |

## Benches: Four Tools, Different Questions

Mem ships four benches that answer different questions. Pick the right one:

| Bench | Question | When to use | Where |
|---|---|---|---|
| **Recall Quality Bench** | Internal: do mem's own ranking signals each carry weight? Should we invest in cross-encoder rerank? | When tuning mem's stack | `tests/recall_bench.rs`, 8-rung ablation (`--ignored`), deterministic geometry embeddings |
| **Gold-set Regression Gate** | Internal: did this ranking edit make recall worse? | Every CI run (non-ignored, hermetic, ~8s) | `tests/golden_recall.rs` + versioned `tests/golden_recall/baseline.json` |
| **LongMemEval Parity** | External: session-level memory recall on the benchmark Zep / agentmemory report against | Cross-system comparison | `tests/mempalace_bench.rs`, real `longmemeval_s_cleaned.json` drop-in |
| **LoCoMo Parity** | External: same question on the track's second common benchmark (mem0 / MemOS / Zep all quote LoCoMo) | Cross-system comparison | `tests/locomo_bench.rs`, real `locomo10.json` drop-in |

The internal benches use deterministic fake embeddings (CI-cheap,
ablation-only); the two parity benches use the production embedding model
(Qwen3-Embedding-0.6B) so the numbers are real. Don't compare absolute
values across internal and external benches.

### Published retrieval numbers (2026-07-02)

Both numbers are **session-level memory recall of evidence sessions**
through mem's real hybrid pipeline (jieba BM25 + Qwen3-0.6B ANN + RRF +
ranking stack). They are *retrieval recall* — deliberately NOT the
LLM-judged end-to-end QA accuracy that Zep (LongMemEval 63.8%) or
mem0/Zep (LoCoMo) headline, which measures a QA model on top of
retrieval and is a different, harder axis.

| Benchmark | any@5 | recall@5 | recall@10 | mrr | Sample |
|---|---|---|---|---|---|
| **LongMemEval-S** (real `longmemeval_s_cleaned.json`) | **0.860** | 0.792 | 0.897 | 0.757 | n=50, type-stratified over the 6 question types |
| **LoCoMo** (real `locomo10.json`) | **0.700** | 0.619 | 0.688 | 0.495 | n=50, category-stratified across all 10 conversations, adversarial excluded |

`any@5` = ≥1 evidence session in the top-5 (the axis comparable to
agentmemory's self-reported recall@5); `recall@5` = fraction of ALL
evidence sessions retrieved. Per-type breakdowns print with each run —
current weak spots are LongMemEval `single-session-preference`
(any@5 0.625) and LoCoMo `open-domain` (any@5 0.417), the latter being
the motivating case for graph-as-a-retrieval-channel work
(oss-memory-diff G2). Both rows use the Qwen3 query-side
instruction template (asymmetric retrieval — documents embed raw,
queries instructed), which lifted LoCoMo any@5 0.660 → 0.700 and
multi-hop any@5 0.462 → 0.615, and re-validated LongMemEval unchanged
(any@5 0.860). The optional G2 graph channel (`LOCOMO_GRAPH=1`: H1
`related_to` links + `expand_graph`) trades top-5 precision for depth
on this corpus — measured +8pt open-domain any@5 and +4pt any@10
against -15pt multi-hop any@5 — so the headline posture keeps it off;
production callers opt in per request via `expand_graph`. Reproduce
with the commands below.

## Recall Quality Bench (transcripts)

A 10-rung ablation harness for the transcript recall pipeline. Quantifies
each ranking signal's marginal NDCG@k contribution and gives an oracle
upper bound for binary cross-encoder rerankers.

### Synthetic (CI / regression smoke)

Runs on a deterministic in-tree fixture (`SyntheticConfig::default()`,
seed=42, 30 sessions × 8 blocks × 24 queries):

```bash
cargo test --test recall_bench synthetic_recall_bench -- --nocapture
```

Prints the 10-rung table to stdout; writes `target/bench-out/recall-synthetic.json`.

### Real (local decision pull)

Set `MEM_BENCH_FIXTURE_PATH` to a JSON dump of your own transcripts
(see `tests/bench/longmemeval_dataset.rs` for the expected schema):

```bash
MEM_BENCH_FIXTURE_PATH=/path/to/recall-real.json \
  cargo test --test recall_bench real_recall_bench -- --ignored --nocapture
```

### Reading the output

The bench answers two questions, each with a different lens:

1. **"Does each existing signal carry weight?"** — read the `all-minus-X` rows.
   The Δ column shows how much NDCG@10 drops when a single signal is removed
   from the full stack. A large negative Δ means the signal is load-bearing;
   ~0.000 means the signal is inert on this fixture.
2. **"Is a real cross-encoder worth pursuing?"** — compare `+oracle-rerank`
   (binary-reranker upper bound) to `+freshness (full)` (current production
   stack). Big gap → spike a real cross-encoder. Small gap → don't bother.

Watch for these synthetic-fixture artifacts (do not generalize to production):

- **HNSW under-performs absolutely.** The CI run uses `FakeEmbeddingProvider`
  which has near-zero semantic signal. `hnsw-only` will look bad regardless of
  production-model behavior; only the *relative* shape across rungs is
  trustworthy.
- **BM25 may dominate.** Co-mention judgments are lexical-coupled, so BM25
  often beats hybrid on synthetic data. This is a ground-truth bias, not a
  ranker bug.
- **`+freshness` may show a regression.** Synthetic timestamps span 90 days
  uniformly while judgments are timestamp-agnostic, so the freshness signal
  re-shuffles relevant-but-old hits below recent-but-irrelevant ones. On real
  conversation data where recent matches *are* more relevant, this flips.
- **`+anchor` is inert by default.** Synthetic queries don't carry
  `anchor_session_id`. Set `SyntheticConfig::anchored_query_fraction > 0.0` to
  exercise the anchor signal in custom configs.

### Notes

- Judgments are derived automatically (co-mention + entity-alias). Absolute
  NDCG values under-count HNSW (synonym hits hidden by the heuristic);
  relative deltas across rungs are reliable.
- The bench shares `pipeline::transcript_recall::score_candidates` with
  production — rung differences are config tuples, not parallel rankers.
- Output JSON shape: see `tests/bench/runner.rs::write_json`.

## LongMemEval & LoCoMo Parity Benches

External-comparison harnesses (`#[ignore]`, never in CI — real model,
real datasets). Shared metric discipline: ingest one capsule per
conversation session, retrieve with the production hybrid ranker, map
ranked capsules back to sessions, score `recall@k` / `any@k` / `mrr`
against the benchmark's evidence-session labels. Sample sizes are
env-tunable and always printed — carry them into any quote.

### LongMemEval (`tests/mempalace_bench.rs`)

Drop the official dataset (277 MB, gitignored) into place, then run:

    curl -L -o tests/mempalace_bench/data/longmemeval_s_cleaned.json \
      https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json
    LONGMEMEVAL_SAMPLE=50 cargo test --release --test mempalace_bench -- --ignored --nocapture

`LONGMEMEVAL_SAMPLE` (default 50, `0` = full 500) is a deterministic
type-stratified sample; `LONGMEMEVAL_DATA` overrides the path. Without
the real file the bench falls back to a bundled synthetic subset and
labels its output as illustrative-only. Wall-clock: ~6 h for n=50 on a
96-core CPU box (per-question fresh store; the session embedding
dominates). One question ≈ 40-50 session embeds.

### LoCoMo (`tests/locomo_bench.rs`)

    curl -L -o tests/locomo_bench/data/locomo10.json \
      https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json
    LOCOMO_SAMPLE=50 cargo test --release --test locomo_bench -- --ignored --nocapture

`LOCOMO_SAMPLE` (default 50, `0` = full ~1500 answerable QAs) is
category-stratified AND spread across all 10 conversations. LoCoMo QAs
share their conversation's haystack, so the bench builds one store per
conversation (not per question) — n=50 runs in ~35 min. Category 5
(adversarial / unanswerable) is excluded, mirroring LongMemEval's
`_abs` exclusion. Image-only turns fall back to their caption text.

### Caveats that ride every quote

- Retrieval recall ≠ QA accuracy. Comparing these numbers against
  Zep's 63.8% LongMemEval or mem0's LoCoMo scores compares different
  axes; the comparable external number is agentmemory's recall@5.
- Embedding-model parity: mem runs Qwen3-Embedding-0.6B (1024-dim);
  systems built on other embedders fold ranking AND embedding deltas
  into any absolute difference.

## Entity Registry (entities + entity_aliases)

Tenant-scoped registry that canonicalizes alias strings (`"Rust"` = `"Rust language"` = `"rustlang"`) to a stable `entity_id`. Three mechanisms feed it:

1. **`mem mine` / `POST /memories`** — caller-supplied `topics: Vec<String>` field plus existing `project` / `repo` / `module` / `task_type` strings auto-promote to entities on first ingest.
2. **`POST /entities`** — explicit creation with optional aliases.
3. **`POST /entities/{id}/aliases`** — add a synonym to an existing entity; idempotent; returns 409 on conflict.

After ingest, `graph_edges.to_node_id` is `"entity:<uuid>"` for every entity-typed edge. Memory→memory edges (`supersedes`) keep the `"memory:<id>"` prefix.

**Legacy rows**: `graph_edges` rows written before the registry shipped retain their legacy `"project:..."` / `"repo:..."` strings on `to_node_id`. The one-shot `repair --rebuild-graph` migration that re-derived these has been removed; new writes go through the registry. Legacy rows are harmless — they just don't participate in entity-keyed lookups.

**Aliases & normalization**: alias matching is lowercase + whitespace-collapsed; punctuation preserved (`C++` ≠ `c`). Caller's verbatim spelling lives on `entities.canonical_name`.

**MCP**: the registry is HTTP-only; no MCP surface (matches the conversation-archive / transcript-recall convention).

