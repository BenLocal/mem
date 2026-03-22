# mem-mcp

MCP server that proxies to the [`mem`](../../README.md) HTTP API so Cursor / Codex (and other MCP clients) can **search**, **ingest**, and **maintain** shared memory.

## Requirements

- Node.js ≥ 20
- A running `mem` instance (`cargo run` or release binary)

## Install from npm

```bash
npm install -g mem-mcp
```

Or run without a global install:

```bash
npx mem-mcp
```

## Install from source

```bash
cd integrations/mem-mcp
npm install
npm run build
```

## Environment

| Variable | Default | Purpose |
|----------|---------|---------|
| `MEM_BASE_URL` | `http://127.0.0.1:3000` | mem HTTP root |
| `MEM_TENANT` | `local` | Default tenant for tools that take optional `tenant` |
| `MEM_MCP_EXPOSE_EMBEDDINGS` | unset | Set to `1` to register `embeddings_*` admin tools |

## Run (stdio)

After a global install:

```bash
mem-mcp
```

From a clone (dev):

```bash
cd integrations/mem-mcp
npm start
```

## Cursor MCP (`mcp.json`)

With **`mem-mcp` on `PATH`** (e.g. after `npm install -g mem-mcp`):

```json
{
  "mcpServers": {
    "mem": {
      "command": "mem-mcp",
      "env": {
        "MEM_BASE_URL": "http://127.0.0.1:3000",
        "MEM_TENANT": "local"
      }
    }
  }
}
```

When developing from a clone, you can still point at the built file with `"command": "node"` and `"args": ["/ABS/PATH/TO/mem/integrations/mem-mcp/dist/index.js"]`.

After editing TypeScript locally, run `npm run build` before restarting the MCP client.

## Tools

| Tool | mem API |
|------|---------|
| `mem_health` | `GET /health` (plain text) |
| `memory_search` | `POST /memories/search` |
| `memory_ingest` | `POST /memories` |
| `memory_get` | `GET /memories/{id}` |
| `memory_feedback` | `POST /memories/feedback` |
| `memory_list_pending_review` | `GET /reviews/pending` |
| `memory_review_accept` | `POST /reviews/pending/accept` |
| `memory_review_reject` | `POST /reviews/pending/reject` |
| `memory_review_edit_accept` | `POST /reviews/pending/edit_accept` |
| `episode_ingest` | `POST /episodes` |
| `memory_graph_neighbors` | `GET /graph/neighbors/{node_id}` |
| `embeddings_*` | optional; see spec |

Design reference: `docs/superpowers/specs/2026-03-21-codex-mem-mcp-integration-design.md`

## CI

The repo root workflow `.github/workflows/ci.yml` runs `npm ci`, `npm test`, and `npm run build` in this directory on every push/PR.
