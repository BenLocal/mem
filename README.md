# mem

Local-first Rust `axum` memory service for multi-agent engineering workflows. The MVP supports memory ingest, pending review, detail lookup, graph diagnostics, compressed search, feedback updates, and episode-driven workflow extraction backed by DuckDB.

## Run Locally

```bash
cargo run
```

The server binds to `127.0.0.1:3000` by default. Set `MEM_DB_PATH` to point at a specific DuckDB file if you do not want to use the default local dev path.

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

## Cross-compile server (Linux binary)

配置文件为仓库根目录的 **`Cross.toml`**（`cross` CLI 固定读取该文件名；若要用别的路径可设环境变量 `CROSS_CONFIG`）。

```bash
cargo install cross --locked
cross build --release
```

二进制：`target/x86_64-unknown-linux-gnu/release/mem`。静态 **musl** 构建：

```bash
cross build --release --target x86_64-unknown-linux-musl
```

产物：`target/x86_64-unknown-linux-musl/release/mem`（适合 Alpine 等无 glibc 环境）。

需要本机已安装并运行 Docker（`cross` 通过容器提供链接环境）。`duckdb` 使用 `bundled` 时若某目标编译失败，可先升级 `cross` 或在 `Cross.toml` 里为该 `target` 换用较新的 `image` 标签。

CI（`.github/workflows/ci.yml`）在 PR / push 上会跑 **`cross build --release`**，目标为 **`x86_64-unknown-linux-gnu`** 与 **`x86_64-unknown-linux-musl`**（与 `Cross.toml` / Docker builder 一致）。打 `v*.*.*` tag 时 Release 工作流会把 **`mem-<tag>-x86_64-unknown-linux-gnu`** 与 **`mem-<tag>-x86_64-unknown-linux-musl`** 一并上传到 GitHub Release。

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
MEM_DB_PATH=/var/lib/mem/mem.duckdb
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

Build and run locally（构建阶段使用与 `Cross.toml` 一致的 **cross-rs** `x86_64-unknown-linux-gnu` 镜像）：

```bash
docker build -t mem:local .
docker run --rm -p 3000:3000 -v mem_data:/data mem:local
```

Example compose (build context is repo root): [deploy/docker-compose.yml](deploy/docker-compose.yml).

Default in the image: `BIND_ADDR=0.0.0.0:3000`, `MEM_DB_PATH=/data/mem.duckdb`. Point MCP clients at the same host with `MEM_BASE_URL` (for example `http://127.0.0.1:3000`).

## Release (GHCR + binaries)

1. Push a semver tag: `git tag v0.1.0 && git push origin v0.1.0`（同时触发 **CI** 与 **Release**；Docker 镜像构建使用 GitHub Actions 缓存加速重复构建）。
2. Workflow [`.github/workflows/release.yml`](.github/workflows/release.yml) 推送 **`ghcr.io/<lowercase-owner>/mem:<tag>`** 与 **`:latest`**，并在 GitHub Release 上附带 **`mem-<tag>-x86_64-unknown-linux-gnu`**、**`mem-<tag>-x86_64-unknown-linux-musl`** 以及 **`mem-<tag>-SHA256SUMS`**（`sha256sum` 校验文件）。MCP server 已合入二进制，无需单独发布。

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
# Mine a transcript
mem mine ~/.claude/projects/.../session.jsonl

# Get wake-up context
mem wake-up --token-budget 800
```

## Transcript Archive (conversation_messages)

A second pipeline, fully isolated from `memories`, archives every Claude Code transcript block verbatim and exposes semantic search + ordered replay over those blocks. It exists alongside `memories` so the existing ranking / lifecycle / verbatim-guard surface is **untouched**: separate table (`conversation_messages`), separate embedding queue (`transcript_embedding_jobs`), and a separate Lance-native embedding table (`conversation_message_embeddings`). `mem mine` is now **dual-sink** — one transcript scan writes both extracted memories (existing path) and every block (text / tool_use / tool_result / thinking) to the archive.

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

## Benches: Two Different Tools, Different Questions

Mem ships two benches that look similar (both use `tests/bench/` plumbing,
both produce per-rung JSON in `target/bench-out/`) but answer different
questions. Pick the right one:

| Bench | Question | When to use | Where |
|---|---|---|---|
| **Recall Quality Bench** | Internal: do mem's own ranking signals each carry weight? Should we invest in cross-encoder rerank? | When tuning mem's stack; CI regression smoke | `tests/recall_bench.rs`, 10-rung ablation, synthetic + real fixtures |
| **MemPalace LongMemEval Parity** | External: how does mem's stack score on the same dataset + protocol that mempalace published baselines for? | Cross-system comparison; manual decision tool | `tests/mempalace_bench.rs`, 3-rung mapping (raw / rooms / full), LongMemEval dataset |

The recall-quality bench uses `FakeEmbeddingProvider` (CI-cheap, deterministic,
ablation-only); the LongMemEval parity bench uses production embedding
(`Config::from_env()`) so the numbers are real. Don't compare absolute
NDCG values across the two — different fixtures, different judgments,
different embedders.

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

## MemPalace LongMemEval Parity Bench

External-comparison benchmark for mem vs mempalace's published
LongMemEval baselines. Apple-to-apple at the protocol level: same
dataset (LongMemEval Standard), same per-Q ephemeral corpus, same
top-K retrieval, same Recall@5/Recall@10/NDCG@10 metrics. mem runs
its own ranking stack (BM25 + HNSW + ScoringOpts) under three
rungs (raw / rooms / full equivalents).

### Run

Pre-download `longmemeval_s_cleaned.json` from the LongMemEval
upstream repo (https://github.com/xiaowu0162/LongMemEval). Set
`EMBEDDING_PROVIDER=embedanything`, `EMBEDDING_MODEL=...`,
`EMBEDDING_DIM=...` per `.env.example`. Then:

    MEM_LONGMEMEVAL_PATH=/path/to/longmemeval_s_cleaned.json \
      cargo test --test mempalace_bench longmemeval -- --ignored --nocapture

For a smoke (50 questions instead of 500):

    MEM_LONGMEMEVAL_PATH=/path/... \
    MEM_LONGMEMEVAL_LIMIT=50 \
      cargo test --test mempalace_bench longmemeval -- --ignored --nocapture

Wall-clock: ~1.5-3 hours for 500 questions x 3 rungs (the embedding
ingest dominates; rung re-rank is fast).

### Reading the output

Three JSON files written to `target/bench-out/`:
- `results_mem_longmemeval_raw_<unix_ts>.json` (vs mempalace `raw` ≈ 0.966 R@5)
- `results_mem_longmemeval_rooms_<unix_ts>.json` (vs mempalace `rooms` ≈ 0.894 R@5)
- `results_mem_longmemeval_full_<unix_ts>.json` (vs mempalace `full` per their README)

Plus a stdout comparison table. The `! Embedding-model parity caveat`
footer notes that mem uses Qwen3 1024-dim while mempalace uses
all-MiniLM-L6-v2 384-dim — absolute mem-vs-mempalace deltas include
both ranking-algorithm AND embedding-model contributions.

## Entity Registry (entities + entity_aliases)

Tenant-scoped registry that canonicalizes alias strings (`"Rust"` = `"Rust language"` = `"rustlang"`) to a stable `entity_id`. Three mechanisms feed it:

1. **`mem mine` / `POST /memories`** — caller-supplied `topics: Vec<String>` field plus existing `project` / `repo` / `module` / `task_type` strings auto-promote to entities on first ingest.
2. **`POST /entities`** — explicit creation with optional aliases.
3. **`POST /entities/{id}/aliases`** — add a synonym to an existing entity; idempotent; returns 409 on conflict.

After ingest, `graph_edges.to_node_id` is `"entity:<uuid>"` for every entity-typed edge. Memory→memory edges (`supersedes`) keep the `"memory:<id>"` prefix.

**Legacy rows**: `graph_edges` rows written before the registry shipped retain their legacy `"project:..."` / `"repo:..."` strings on `to_node_id`. The one-shot `repair --rebuild-graph` migration that re-derived these has been removed; new writes go through the registry. Legacy rows are harmless — they just don't participate in entity-keyed lookups.

**Aliases & normalization**: alias matching is lowercase + whitespace-collapsed; punctuation preserved (`C++` ≠ `c`). Caller's verbatim spelling lives on `entities.canonical_name`.

**MCP**: the registry is HTTP-only; no MCP surface (matches the conversation-archive / transcript-recall convention).

