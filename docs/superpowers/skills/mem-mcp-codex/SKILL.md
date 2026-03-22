---
name: mem-mcp-codex
description: Use shared mem via MCP (mem_health, memory_search, ingest, get, graph, feedback, pending review actions, episodes, optional embeddings admin).
---

# Shared memory (mem) via MCP

When the **mem** MCP server is enabled, use its tools to read and write the same DuckDB-backed memory as other Codex / Cursor sessions.

If other tools fail with connection errors, call **`mem_health`** first to confirm `MEM_BASE_URL` is correct and `cargo run` (or your deployment) is up.

## Environment (host of `node …/mem-mcp/dist/index.js`)

| Variable | Default | Meaning |
|----------|---------|---------|
| `MEM_BASE_URL` | `http://127.0.0.1:3000` | mem HTTP root (no trailing slash required). |
| `MEM_TENANT` | `local` | Default `tenant` when a tool omits it. |
| `MEM_MCP_EXPOSE_EMBEDDINGS` | unset | Set to `1` to register `embeddings_*` admin tools. |

Run **`cargo run`** (or your deploy) for mem before starting the MCP server. See repo root [README.md](../../../../README.md) §「Codex / MCP」for links to spec, plan, and this skill.

## Before substantive work

1. Call **`memory_search`** with a short natural-language `query`, a **distinct** `caller_agent` (e.g. `cursor`, `codex-cli`, `ci:job-42`), and tight **`token_budget`** (e.g. 300–500).
2. Prefer **`scope_filters`** aligned with the repo: e.g. `repo:my-app`, `module:billing` when known.
3. Set `intent` to the task type when it helps ranking (`debugging`, `implementation`, `general`, …).

## During the task

- If the user asks about past decisions, conventions, or bugs, **`memory_search`** again or **`memory_get`** if you already have a `memory_id`.
- To explore entity relationships when you already have a graph **`node_id`** (from search metadata or docs), use **`memory_graph_neighbors`** (ids often look like `module:repo:foo`).

## Writing

- **Implementation / factual** knowledge: **`memory_ingest`** with `memory_type: implementation`, appropriate `scope` / `repo` / `module`, `write_mode: auto` when policy allows.
- **Preferences / strong constraints**: prefer **`write_mode: propose`** and types that require review (`preference`, etc.) per product policy.
- After a **successful multi-step run**, consider **`episode_ingest`** so workflows can be mined later.

## Pending review

- List with **`memory_list_pending_review`**. After the human decides:
  - **`memory_review_accept`** / **`memory_review_reject`**, or
  - **`memory_review_edit_accept`** when content should be fixed before activation (matches HTTP `edit_accept`).

## Feedback

- When the user confirms a memory helped or hurt recall, use **`memory_feedback`** (`useful`, `outdated`, …).

## Do not

- Bypass MCP to invent URLs; tool schemas match the mem REST API.
- Set `caller_agent` to a generic string; use a **per-runtime** value for traceability.

**Spec:** `docs/superpowers/specs/2026-03-21-codex-mem-mcp-integration-design.md`  
**Plan:** `docs/superpowers/plans/2026-03-21-codex-mem-mcp-integration.md`  
**Package:** `integrations/mem-mcp/README.md`
