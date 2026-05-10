---
description: Search local mem memories (lexical + semantic + ranking) and show top results.
argument-hint: Search query string (required). Optionally include scope/intent hints.
allowed-tools: Bash
---

Run a memory search via the `mcp__mem__capability_capsule_search` tool with the user's query as `$ARGUMENTS`.

Procedure:

1. Treat the entire `$ARGUMENTS` as the query string. If empty, ask the user what they want to search for.
2. Call `mcp__mem__capability_capsule_search` with `tenant` (default `local` from `MEM_TENANT`), `query`, and `limit: 10`.
3. Render the response in this shape (skip empty fields):

   ```
   ✦ mem search results · N hits

   1. <capability_capsule_id> · <score>
      <summary or first 80 chars of content>
      tags: …  scope: …  updated_at: …

   2. …
   ```

4. If a returned memory directly answers the user's question, after using it, call `mcp__mem__capability_capsule_feedback` with `feedback_kind: "useful"` for that capability_capsule_id. If it's just relevant context, use `applies_here` instead. **At most one feedback signal per memory per session.** Don't fire feedback on memories you only skimmed.

If the service isn't running, the MCP call will error — fall back to suggesting `/mem:health`.
