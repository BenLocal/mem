# mem

Local-first Rust `axum` memory service for multi-agent engineering workflows. The MVP supports memory ingest, pending review, detail lookup, graph diagnostics, compressed search, feedback updates, and episode-driven workflow extraction backed by DuckDB.

## Run Locally

```bash
cargo run
```

The server binds to `127.0.0.1:3000` by default. Set `MEM_DB_PATH` to point at a specific DuckDB file if you do not want to use the default local dev path.

## Codex / MCP (shared memory)

- **Spec:** [docs/superpowers/specs/2026-03-21-codex-mem-mcp-integration-design.md](docs/superpowers/specs/2026-03-21-codex-mem-mcp-integration-design.md)
- **Implementation plan:** [docs/superpowers/plans/2026-03-21-codex-mem-mcp-integration.md](docs/superpowers/plans/2026-03-21-codex-mem-mcp-integration.md)
- **MCP package:** [integrations/mem-mcp](integrations/mem-mcp) — publishable npm package `mem-mcp` (`npm install -g mem-mcp` or `npx mem-mcp`; stdio)
- **Agent skill (workflow):** [docs/superpowers/skills/mem-mcp-codex/SKILL.md](docs/superpowers/skills/mem-mcp-codex/SKILL.md)
- **CI without MCP:** [docs/superpowers/examples/ci-mem-http-snippet.md](docs/superpowers/examples/ci-mem-http-snippet.md)

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

## Release (npm + GHCR)

1. Add an npm **Automation** access token as repository secret **`NPM_TOKEN`**.
2. Push a semver tag: `git tag v0.1.0 && git push origin v0.1.0`.
3. Workflow [`.github/workflows/release.yml`](.github/workflows/release.yml) publishes **`mem-mcp`** to npm (version taken from the tag, without `v`), pushes **`ghcr.io/<lowercase-owner>/mem:<tag>`** plus **`:latest`**, and attaches **`mem-<tag>-x86_64-unknown-linux-gnu`** (glibc，与 Debian 运行时一致) 与 **`mem-<tag>-x86_64-unknown-linux-musl`** (静态 musl) 到 GitHub Release。

Plan: [docs/superpowers/plans/2026-03-22-mem-publish-docker-actions.md](docs/superpowers/plans/2026-03-22-mem-publish-docker-actions.md).

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
