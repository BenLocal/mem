---
description: Mine the current Claude Code transcript (or a given path) into mem — extracts memories and archives every block.
argument-hint: Optional transcript path. Defaults to the current session's transcript.
allowed-tools: Bash
---

Run `mem mine` against a transcript. Two paths:

1. **No argument** — mine the current Claude Code session's transcript:

   ```bash
   SESSION_ID="$(jq -r .session_id ~/.claude/projects/*/SESSIONS.json 2>/dev/null | head -1)"
   # Fallback: ask the user for the session id, or look up via the CC settings
   TRANSCRIPT="$(find ~/.claude/projects -name "${SESSION_ID}.jsonl" 2>/dev/null | head -1)"
   ```

   If `$TRANSCRIPT` resolves to an existing file, run `mem mine "$TRANSCRIPT" --agent claude-code`.

2. **With an argument** — treat `$ARGUMENTS` as a path to a `.jsonl` transcript and run `mem mine "$ARGUMENTS" --agent claude-code` directly.

Output of `mem mine` looks like:

```
Mined: capsules sent=X/Y blocks sent=A/B (server-side dedup applied)
```

Report the numerator/denominator to the user. `capsules sent=0/N` is normal — the extractor only picks up the explicit `<mem-save>...</mem-save>` tag (the prose-cue heuristics like "我会记住：" / "Important:" were removed after a recursive false-positive bug; the verbatim block archive `blocks sent` is non-discriminating and everything goes in there).

If the user wants a memory written from this conversation, they should either embed `<mem-save>...</mem-save>` in the assistant turn or use `mcp__mem__capability_capsule_ingest` directly with structured fields. Do not rely on `mem mine` to infer memory-worthiness from prose.
