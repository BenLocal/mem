---
name: mem-mcp-codex
description: Use shared mem service via MCP tools (memory_search, memory_ingest, memory_get, feedback, episodes) for Codex and multi-session agents.
---

# Shared memory (mem) via MCP

When the **mem** MCP server is enabled, use its tools to read and write the same DuckDB-backed memory as other Codex / Cursor sessions.

## Environment (host of `node …/mem-mcp/dist/index.js`)

- `MEM_BASE_URL` — mem HTTP root (default `http://127.0.0.1:3000`).
- `MEM_TENANT` — default tenant when a tool omits `tenant` (default `local`).
- `MEM_MCP_EXPOSE_EMBEDDINGS=1` — optional; registers embedding admin tools.

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

## Feedback

- When the user confirms a memory helped or hurt recall, use **`memory_feedback`** (`useful`, `outdated`, …).

## Do not

- Bypass MCP to invent URLs; tool schemas match the mem REST API.
- Set `caller_agent` to a generic string; use a **per-runtime** value for traceability.

**Spec:** `docs/superpowers/specs/2026-03-21-codex-mem-mcp-integration-design.md`  
**Package:** `integrations/mem-mcp/README.md`
