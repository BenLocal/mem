---
name: mem-mcp-codex
description: Use shared mem via MCP (mem_health, memory_search, ingest, get, graph, feedback, pending review actions, episodes, optional embeddings admin).
---

# Shared memory (mem) via MCP

When the **mem** MCP server is enabled, use its tools to read and write the same DuckDB-backed memory as other Codex / Cursor sessions.

If other tools fail with connection errors, call **`mem_health`** first to confirm `MEM_BASE_URL` is correct and `cargo run` (or your deployment) is up.

## Environment (host of `mem mcp`)

| Variable | Default | Meaning |
|----------|---------|---------|
| `MEM_BASE_URL` | `http://127.0.0.1:3000` | mem HTTP root (no trailing slash required). |
| `MEM_TENANT` | `local` | Default `tenant` when a tool omits it. |
| `MEM_MCP_EXPOSE_EMBEDDINGS` | unset | Set to `1` to register `embeddings_*` admin tools. |

Start the HTTP service in one terminal (`mem serve` / `cargo run`), then run **`mem mcp`** as the stdio MCP server (same binary, separate process). See repo root [README.md](../../../../README.md) §「Codex / MCP」.

## Default autopilot workflow (every user turn)

1. **Read first**: call **`memory_search`** before planning or coding.
   - Use a short natural-language `query` from the latest user ask.
   - Set a distinct `caller_agent` (e.g. `cursor`, `codex-cli`, `ci:job-42`).
   - Keep `token_budget` small (typically 300-500).
   - Add narrow `scope_filters` when known (e.g. `repo:mem`, `module:billing`).
   - Set `intent` when obvious (`debugging`, `implementation`, `general`, ...).
2. **Use retrieved context**: apply relevant facts/constraints in the answer and implementation.
3. **Write back after completion**: when the turn produces durable value, call **`memory_ingest`**.
   - Durable value = decisions, bug root causes, fixes, constraints, reusable patterns, runbook steps.
   - Skip write for chit-chat, temporary thoughts, or low-signal output.

## During the task

- If the user asks about past decisions, conventions, or bugs, **`memory_search`** again or **`memory_get`** if you already have a `memory_id`.
- To explore entity relationships when you already have a graph **`node_id`** (from search metadata or docs), use **`memory_graph_neighbors`** (ids often look like `module:repo:foo`).

## Writing policy (what and how to ingest)

- **Implementation / factual** knowledge: **`memory_ingest`** with `memory_type: implementation`, appropriate `scope` / `repo` / `module`, `write_mode: auto` when policy allows.
- **Preferences / strong constraints**: prefer **`write_mode: propose`** and types that require review (`preference`, etc.) per product policy.
- After a **successful multi-step run**, consider **`episode_ingest`** so workflows can be mined later.
- Keep entries concise and de-duplicated. If the same fact already exists, do not create near-duplicates.
- Include useful metadata (`project`, `repo`, `module`, `task_type`, `tags`) to improve future recall precision.

## Safety and privacy gates (mandatory)

- Never ingest secrets or credentials (tokens, API keys, passwords, private keys, auth headers).
- Never ingest private personal data unless explicitly approved.
- When uncertain about sensitivity, do not ingest and ask the user.

## Pending review

- List with **`memory_list_pending_review`**. After the human decides:
  - **`memory_review_accept`** / **`memory_review_reject`**, or
  - **`memory_review_edit_accept`** when content should be fixed before activation (matches HTTP `edit_accept`).

## Feedback

- When the user confirms a memory helped or hurt recall, use **`memory_feedback`** (`useful`, `outdated`, …).

## Do not

- Bypass MCP to invent URLs; tool schemas match the mem REST API.
- Set `caller_agent` to a generic string; use a **per-runtime** value for traceability.
- Write on every turn by default without signal checks (avoid memory noise).

## Strict mode (optional)

Use this stricter policy when memory quality matters more than recall volume:

1. **Read still runs every turn** (`memory_search`), but:
   - `token_budget` <= 300
   - always set `scope_filters` when repo/module is known
2. **Write only if ALL are true**:
   - the output contains a durable decision/root-cause/fix/workflow
   - confidence is high (repeatable or validated by test/log/evidence)
   - no equivalent memory already exists in recent search results
3. **Explicit user consent override**:
   - if the user says "记下来/保存到记忆", ingest even if confidence is medium
4. **Hard blocklist**:
   - secrets, credentials, personal sensitive data, legal/compliance-risk text
5. **Write budget**:
   - max 1 memory_ingest per user turn (except explicit user request)
6. **Episode rule**:
   - `episode_ingest` only after a truly completed multi-step task (not partial progress)

**Server:** `mem mcp` (Rust binary, replaces the historical Node `integrations/mem-mcp/`)  
**Spec (historical):** `docs/superpowers/specs/2026-03-21-codex-mem-mcp-integration-design.md`  
**Plan (historical):** `docs/superpowers/plans/2026-03-21-codex-mem-mcp-integration.md`
