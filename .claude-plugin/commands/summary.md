---
description: Summarize the state of the local mem instance — service health, pending review queue, recent memories, top topics.
argument-hint: Optional tenant override. Defaults to MEM_TENANT or "local".
allowed-tools: Bash
---

Produce a one-screen summary of what the local mem service currently holds, so the user can answer "what's in my mem right now?" without browsing the DB by hand.

Procedure:

1. Resolve `TENANT = ${ARGUMENTS:-${MEM_TENANT:-local}}`.

2. Service health — call `mcp__mem__mem_health`. If it errors, stop and tell the user `mem serve` is down and how to start it (`/mem:health` for the canonical liveness check).

3. Pending review queue — call `mcp__mem__capability_capsule_list_pending_review` with `tenant: TENANT, limit: 50`. Capture the count and the top 3 oldest entries' summaries.

4. Recent activity — call `mcp__mem__capability_capsule_search` with `tenant: TENANT, query: "", limit: 10` (empty query falls through to the recent-active path). Note the most-recent `updated_at` and how many distinct `tags` / `topics` appeared.

5. Wake-up context — call `mem wake-up --tenant "$TENANT" --token-budget 600` for the high-confidence highlights (the same block the SessionStart hook injects).

Render a compact summary in this shape (skip empty sections):

```
✦ mem summary · tenant=<TENANT>

Service:    up · provider=<from mem_health> · sidecar=<from mem_health>
Pending:    <N> in review queue
            • <oldest-1 summary>
            • <oldest-2 summary>
            • <oldest-3 summary>
Recent:     <M> memories · last updated <last_updated_at>
            tags seen: <comma-list>
            topics seen: <comma-list>

Highlights:
<wake-up context block, verbatim>
```

Keep it terse — bullets, no walls of prose. If a section is empty (no pending review, no recent activity), say so in one line and move on.
