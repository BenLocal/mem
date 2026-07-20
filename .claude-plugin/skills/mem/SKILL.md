---
name: mem
description: Local-first Rust memory service (Lance + Tantivy BM25 + MCP). Use proactively when the user mentions mem / memory / capsule / recall / remember when… / 上次怎么做的 / 之前那个 bug; when starting a non-trivial task in a known repo (search prior decisions before planning); when an error resembles a documented incident; or when a hard-won fix just landed (propose an experience capsule so the next session doesn't re-derive it).
allowed-tools: Bash, Read, Grep
---

# mem — local memory service

## When to load this skill (the original 5 triggers, expanded)

Don't wait for the user to invoke this skill by name — load it on any of:

(a) **Explicit mention** — user says mem / mem serve / mem mine / capability_capsule_ingest / capability_capsule_search / memory feedback / wake-up context / Lance-backed memory.

(b) **Start of a non-trivial task in a familiar repo** — debugging, design, refactoring, "how should I do X here?". Call `capability_capsule_search` *first* to surface prior decisions, incidents, conventions, or commit-message lessons before formulating a plan. Skipping this step means re-deriving context the user already taught you.

(c) **Recall intent signals** — "remember when…", "how did we handle…", "上次怎么做的", "之前那个 bug", "what was the URL / port / config for X". These are explicit recall asks; respond by querying memory before answering from training-time priors.

(d) **Error / bug resembling something we've seen** — Lance / index / embedding / hook / commit-conflict errors are often documented incidents. Search before guessing at the cause.

(e) **About to persist a learning** — finished any non-trivial task: debugging session, fix, gotcha, design decision, API quirk, configuration choice. **Default to calling `capability_capsule_ingest` with `capability_capsule_type="experience"` and `write_mode="propose"`** — that combination writes a capsule row with `status=PendingConfirmation`, which `capability_capsule_list_pending_review` surfaces and `review_accept` / `review_edit_accept` promote into the active pool. False positives are cheap (one `review_reject` click discards them); under-proposing silently loses learnings the next session would have to re-derive. Skip only when (i) the commit is a pure typo / formatting / dependency bump, or (ii) the same fact was already captured by an earlier capsule in this session. The plain `capability_capsule_ingest` with `write_mode="auto"` (the default) writes straight into the active pool, no review gate — reserve that for facts the user **explicitly** asks to save (via `/mem:save` or "remember this exactly"). The verbatim-rule applies to both write modes: `content` is the fact source, never a refined summary.

---

# Reference: mem — local memory service

`mem` is a single-binary Rust service (Lance datasets read natively via the lancedb API, with an in-RAM Tantivy BM25 index) exposing both an HTTP API and an MCP server. Default base URL: `http://127.0.0.1:3000`. Default tenant: `local`. Both can be overridden via `MEM_BASE_URL` / `MEM_TENANT`.

## Proactive use policy

**Default to searching before answering**, not after. When you load this skill via one of the (b)–(d) triggers above:

1. Issue a single `mcp__mem__capability_capsule_search` against the user's apparent intent (the question, the error message, the file path, the function name — whichever is most specific). Token budget 1500–2500 is plenty.
2. Read the returned `directives` + top 2–3 `relevant_facts` before formulating a response.
3. If a returned memory directly resolved the question, send `feedback_kind: useful` for that `capability_capsule_id` (one signal per memory per session, max).
4. If nothing relevant came back, proceed normally — silence is fine, don't over-invoke.

**When to persist, not just search** — pick the right tool for who's making the call:

- **You (agent) judged it's worth remembering** → `mcp__mem__capability_capsule_ingest` with `capability_capsule_type="experience"` and `write_mode="propose"`. Required args: `content` (cause + symptom + fix, verbatim), `scope` (e.g. `"repo"` or `"project"`), `capability_capsule_type="experience"`, `write_mode="propose"`. Optional: `summary` (one-line headline ≤80 chars), `project` (repo basename), `source_agent` (`"claude-code"`). Lands in the review queue with `status=PendingConfirmation` — `list_pending_review` surfaces it; user/agent later runs `review_accept` to promote. False positives cost nothing (one reject click).
- **User explicitly asked to save** ("remember this", `/mem:save`) → `mcp__mem__capability_capsule_ingest` with `capability_capsule_type: experience`. Writes directly to the active pool, no review gate.

After a `git commit fix(*)` / `feat(*)` / `refactor(*)`, a PostToolUse hook fires a system reminder nudging the propose path. Trust it — that's the canonical "we just landed something worth thinking about" trigger.

Verbatim rule applies in both: `content` is the fact source, never a refined summary.

## Verifying the service is running

```bash
curl -sS "$MEM_BASE_URL"/capability_capsules/search -H 'content-type: application/json' \
  -d '{"tenant":"local","query":"ping","limit":1}' | head -c 200
```

If the service is down, start it from the repo:

```bash
cd <mem-repo> && cargo run -- serve   # default port 3000
```

## MCP tools (preferred over raw HTTP from inside Claude Code)

The `plugin:mem:mem` MCP server forwards to the local HTTP service. Use these tools, not curl:

- `mcp__mem__capability_capsule_search` — primary recall path (lexical + semantic + ranking)
- `mcp__mem__capability_capsule_search_contextual` — search with current scope/intent context
- `mcp__mem__capability_capsule_get` — fetch a specific memory by id
- `mcp__mem__capability_capsule_ingest` — write a structured memory (skips the `<mem-save>` extractor; use for high-signal facts)
- `mcp__mem__capability_capsule_feedback` — close the loop after using a retrieved memory (`useful` / `applies_here` / `outdated` / `does_not_apply_here` / `incorrect`; optional `note`)
- `mcp__mem__capability_capsule_ingest` with `write_mode="propose"` — primary review-queue entry path (writes a capsule with `status=PendingConfirmation`; surfaces in `list_pending_review`)
- `mcp__mem__capability_capsule_propose_preference` / `_commit_fact` — preference / fact-specific review-queue variants
- `mcp__mem__episode_ingest` — records a goal/steps/outcome **episode** in the parallel `episodes` table for downstream workflow extraction (NOT the capsule review queue). For human review before promotion, use the `_ingest` + `write_mode=propose` path above instead.
- `mcp__mem__capability_capsule_review_accept` / `_review_edit_accept` / `_review_reject` — drive the review queue
- `mcp__mem__capability_capsule_graph_neighbors` — explore the entity / topic graph
- `mcp__mem__capability_capsule_bootstrap` — initial context dump for a new session
- `mcp__mem__episode_ingest` — write an entire episode at once
- `mcp__mem__mem_health` — quick liveness check

Set `MEM_MCP_EXPOSE_EMBEDDINGS=1` to also get the admin `embeddings_*` tools (rebuild, list_jobs, providers).

## CLI subcommands (run from the repo with `cargo run --`)

- `mem serve` — HTTP server on `BIND_ADDR` (default `127.0.0.1:3000`)
- `mem mcp` — stdio MCP forwarder; reads `MEM_BASE_URL` + `MEM_TENANT`
- `mem mine <transcript_path> --agent claude-code` — dual-sink: extracts memories from `<mem-save>...</mem-save>` tags only (prose cues like "我会记住：" / "Important:" used to also trigger extraction but were removed after a recursive false-positive bug — agents wanting a fact persisted without writing the tag should call `capability_capsule_ingest` MCP directly), AND archives every block (text / tool_use / tool_result / thinking) verbatim to `conversation_messages`
- `mem wake-up --tenant local --token-budget 800` — short recent-context dump (used by the SessionStart hook)
- `mem feedback-from-transcript <path> --tenant local` — auto-emit `applies_here` for memories the assistant referenced post-search

## Slash commands

This plugin ships matching commands under `/mem:`:

- `/mem:help` — this overview
- `/mem:health` — verify the service responds
- `/mem:search <query>` — invoke `capability_capsule_search` and show results
- `/mem:mine [<transcript_path>]` — mine the current Claude Code transcript (or an explicit path) into memories + archive
- `/mem:wake-up` — print the wake-up context block
- `/mem:summary` — one-screen state of the local mem instance (health + pending review + recent + wake-up)
- `/mem:save <text>` — direct write to active pool; bypasses both the `<mem-save>` extractor and the review queue. For verbatim user-provided facts only
- `/mem:feedback <capability_capsule_id> <kind>` — fire `useful` / `applies_here` / `outdated` / `does_not_apply_here` / `incorrect` for a capsule you just used (one signal per capsule per session, max)

## Verbatim rule (read before writing memories)

`memories.content` is the **fact source** — never rewrite, never truncate at storage time. `summary` is index/hint only. When using `capability_capsule_ingest`, do not copy a refined / summarized version of the same text into `content`; the ingest pipeline rejects ingests where `summary == content`. Detail in the project's `AGENTS.md`.

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
