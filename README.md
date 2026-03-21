# mem

Local-first Rust `axum` memory service for multi-agent engineering workflows. The MVP supports memory ingest, pending review, detail lookup, graph diagnostics, compressed search, feedback updates, and episode-driven workflow extraction backed by DuckDB.

## Run Locally

```bash
cargo run
```

The server binds to `127.0.0.1:3000` by default. Set `MEM_DB_PATH` to point at a specific DuckDB file if you do not want to use the default local dev path.

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
