---
description: Save a capsule directly via the MCP `capability_capsule_ingest` tool — bypasses `<mem-save>` tag extraction so you can persist a fact in one shot.
argument-hint: The fact to save (≥12 chars). Optional `type=…` / `scope=…` / `tags=a,b,c` prefixes override defaults.
allowed-tools: mcp__mem__capability_capsule_ingest
---

Save a capsule with the user's `$ARGUMENTS` as `content`. This is the **B path** in mem's docs: skip `mem mine`'s narrow `<mem-save>` extractor and POST a capsule directly. Use when the user explicitly asks to remember something rather than letting it get picked up from transcript prose.

Procedure:

1. **Parse `$ARGUMENTS`.** Strip leading `key=value` pairs (separated from the content by whitespace) and treat the rest as the capsule's `content`. Recognized keys:
   - `type=` — one of `implementation | experience | preference | episode | workflow`. Defaults to `experience` (matches `mem mine`'s default).
   - `scope=` — one of `global | project | repo | workspace`. Defaults to `project`.
   - `tags=` — comma-separated; passes to `tags: ["a", "b", "c"]`. Defaults to `[]`.
   - `visibility=` — `private | shared | system`. Defaults to `private`.

   Example arg shapes you must accept:
   - `这是一条规则` → type=experience, scope=project, content=`这是一条规则`
   - `type=preference scope=global 偏好简短回答` → overrides applied
   - `tags=docker,deploy 部署前必须 docker compose down` → tags=`["docker","deploy"]`

2. **Validate content.** If trimmed content is shorter than 12 characters, refuse with a one-line message ("内容太短，capsule 至少 12 字符") and stop — that's the same heuristic `looks_like_real_memory` uses for the extractor.

3. **Call the MCP tool:**

   ```
   mcp__mem__capability_capsule_ingest
     capability_capsule_type: <type>
     content: <content verbatim>
     scope: <scope>
     visibility: <visibility>
     tags: <tags>
     source_agent: "claude-code"
     write_mode: "auto"
   ```

   Leave `tenant` unset so the server resolves it from `MEM_TENANT` (default `local`). Don't supply `idempotency_key` — the same content posted twice will get a fresh row, which is fine; if the user wants dedup they should call the MCP tool directly with a key.

4. **Report the result.** On success the response carries `capability_capsule_id` + `status`. Render:

   ```
   ✦ saved capsule <id> · type=<type> scope=<scope>
   ```

   On failure, surface the server error verbatim. Common cases:
   - mem service not running → suggest `/mem:health`
   - invalid `type` / `scope` after parsing → list the allowed values

Do **not** wrap the content in `<mem-save>...</mem-save>` — that's only for the extractor path. Here we're talking to the ingest endpoint directly, so the raw text is the fact.
