---
name: mem
description: Local-first Rust memory service for multi-agent workflows. Use when the user mentions mem / mem serve / mem mine / memory ingest / memory search / wake-up context / DuckDB-backed memory, or when they want to interact with the local mem HTTP/MCP service.
allowed-tools: Bash, Read, Grep
---

# mem — local memory service

`mem` is a single-binary Rust service (DuckDB + HNSW + Tantivy + r2d2 read pool) exposing both an HTTP API and an MCP server. Default base URL: `http://127.0.0.1:3000`. Default tenant: `local`. Both can be overridden via `MEM_BASE_URL` / `MEM_TENANT`.

## Verifying the service is running

```bash
curl -sS "$MEM_BASE_URL"/memories/search -H 'content-type: application/json' \
  -d '{"tenant":"local","query":"ping","limit":1}' | head -c 200
```

If the service is down, start it from the repo:

```bash
cd <mem-repo> && cargo run -- serve   # default port 3000
```

## MCP tools (preferred over raw HTTP from inside Claude Code)

The `plugin:mem:mem` MCP server forwards to the local HTTP service. Use these tools, not curl:

- `mcp__mem__memory_search` — primary recall path (lexical + semantic + ranking)
- `mcp__mem__memory_search_contextual` — search with current scope/intent context
- `mcp__mem__memory_get` — fetch a specific memory by id
- `mcp__mem__memory_ingest` — write a structured memory (skips the `<mem-save>` extractor; use for high-signal facts)
- `mcp__mem__memory_feedback` / `_apply_feedback` — close the loop after using a retrieved memory (`useful` / `applies_here` / `outdated` / `does_not_apply_here` / `incorrect`)
- `mcp__mem__memory_propose_experience` / `_propose_preference` / `_commit_fact` — write into the review queue
- `mcp__mem__memory_review_accept` / `_review_edit_accept` / `_review_reject` — drive the review queue
- `mcp__mem__memory_graph_neighbors` — explore the entity / topic graph
- `mcp__mem__memory_bootstrap` — initial context dump for a new session
- `mcp__mem__episode_ingest` — write an entire episode at once
- `mcp__mem__mem_health` — quick liveness check

Set `MEM_MCP_EXPOSE_EMBEDDINGS=1` to also get the admin `embeddings_*` tools (rebuild, list_jobs, providers).

## CLI subcommands (run from the repo with `cargo run --`)

- `mem serve` — HTTP server on `BIND_ADDR` (default `127.0.0.1:3000`)
- `mem mcp` — stdio MCP forwarder; reads `MEM_BASE_URL` + `MEM_TENANT`
- `mem mine <transcript_path> --agent claude-code` — dual-sink: extracts memories from `<mem-save>` / "我会记住：" / "Important:" cues AND archives every block (text / tool_use / tool_result / thinking) verbatim to `conversation_messages`
- `mem wake-up --tenant local --token-budget 800` — short recent-context dump (used by the SessionStart hook)
- `mem repair --check` — diagnose vector index sidecar without modifying anything
- `mem repair --rebuild` — force-rebuild the HNSW sidecar (offline; stop `mem serve` first)
- `mem feedback-from-transcript <path> --tenant local` — auto-emit `applies_here` for memories the assistant referenced post-search

## Slash commands

This plugin ships matching commands under `/mem:`:

- `/mem:help` — this overview
- `/mem:health` — verify the service responds
- `/mem:search <query>` — invoke `memory_search` and show results
- `/mem:mine [<transcript_path>]` — mine the current Claude Code transcript (or an explicit path) into memories + archive
- `/mem:wake-up` — print the wake-up context block
- `/mem:summary` — one-screen state of the local mem instance (health + pending review + recent + wake-up)

## Verbatim rule (read before writing memories)

`memories.content` is the **fact source** — never rewrite, never truncate at storage time. `summary` is index/hint only. When using `memory_ingest`, do not copy a refined / summarized version of the same text into `content`; the ingest pipeline rejects ingests where `summary == content`. Detail in the project's `AGENTS.md`.

## Feedback discipline

Every retrieved memory you actually use should get one feedback signal (the strongest applicable one), at most once per memory per session:

| `feedback_kind` | when to send |
|---|---|
| `useful` | The memory directly unblocked / answered the task. |
| `applies_here` | Memory was relevant context but not load-bearing. |
| `outdated` | Was right at ingest, now stale (renamed file, reverted decision). |
| `does_not_apply_here` | Correct elsewhere, doesn't fit this scope. |
| `incorrect` | Verified factually wrong. **Archives the row** — destructive. |

Silence is a valid signal too. Don't fire `applies_here` for every search hit you skimmed and ignored.
