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

- **Agent skill (workflow):** [docs/superpowers/skills/mem-mcp-codex/SKILL.md](docs/superpowers/skills/mem-mcp-codex/SKILL.md)
- **CI without MCP:** [docs/superpowers/examples/ci-mem-http-snippet.md](docs/superpowers/examples/ci-mem-http-snippet.md)
- **Historical context** (now superseded by the Rust implementation): [spec](docs/superpowers/specs/2026-03-21-codex-mem-mcp-integration-design.md), [plan](docs/superpowers/plans/2026-03-21-codex-mem-mcp-integration.md)

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

Plan (历史): [docs/superpowers/plans/2026-03-22-mem-publish-docker-actions.md](docs/superpowers/plans/2026-03-22-mem-publish-docker-actions.md).

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

A second pipeline, fully isolated from `memories`, archives every Claude Code transcript block verbatim and exposes semantic search + ordered replay over those blocks. It exists alongside `memories` so the existing ranking / lifecycle / verbatim-guard surface is **untouched**: separate table (`conversation_messages`), separate embedding queue (`transcript_embedding_jobs`), and a separate HNSW sidecar at `<MEM_DB_PATH>.transcripts.usearch`. `mem mine` is now **dual-sink** — one transcript scan writes both extracted memories (existing path) and every block (text / tool_use / tool_result / thinking) to the archive. Design: [`docs/superpowers/specs/2026-04-30-conversation-archive-design.md`](docs/superpowers/specs/2026-04-30-conversation-archive-design.md).

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

# Semantic search over archived blocks (filters are all optional).
curl -X POST localhost:3000/transcripts/search \
  -H 'content-type: application/json' \
  -d '{
    "tenant": "local",
    "query": "invoice retry debugging",
    "role": "assistant",
    "block_type": "text",
    "limit": 10
  }'

# Time-ordered replay of one session (verbatim transcript).
curl 'localhost:3000/transcripts?tenant=local&session_id=sess_abc'
```

**MCP does not expose transcript search by design** — agents go through `memory_search`, then use the resulting `session_id` to pull the surrounding transcript via the HTTP endpoints above.

| Variable | Default | Meaning |
|----------|---------|---------|
| `MEM_TRANSCRIPT_EMBED_DISABLED` | unset | Set to `1` to stop the transcript embedding worker (e.g. when using OpenAI to avoid double provider spend). Blocks still archive verbatim. |
| `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY` | 256 | Flush cadence for the transcripts HNSW sidecar; lower than the memories sidecar because per-session bursts are larger. |

`mem repair --check|--rebuild` covers both sidecars (memories and transcripts) in one pass.
