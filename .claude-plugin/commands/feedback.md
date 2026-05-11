---
description: Send a feedback signal for a capsule — moves confidence / decay / status per the kind. Use after you've read AND actually used (or rejected) a retrieved capsule.
argument-hint: "<capability_capsule_id> <kind>"  · kind ∈ {useful | applies_here | outdated | does_not_apply_here | incorrect}
allowed-tools: mcp__mem__capability_capsule_feedback
---

Manual close-the-loop for the capsule feedback lifecycle. The Stop / PreCompact hooks fire `applies_here` automatically when retrieved text gets quoted verbatim later in the conversation, but **strong** signals (`useful`, `outdated`, `incorrect`, `does_not_apply_here`) need a human or agent to judge — auto-inference would burn the signal. This command is that path.

### Procedure

1. **Parse `$ARGUMENTS`.** Expect exactly two whitespace-separated tokens: `<capability_capsule_id> <kind>`. If shape is wrong, refuse with a one-line usage hint and stop.

2. **Validate `<kind>`** is one of the five values exactly (lowercase, with underscores):

   | kind | effect | when to send |
   |---|---|---|
   | `useful` | confidence +0.10, marks validated | the capsule **directly** unblocked / answered this task — strongest positive |
   | `applies_here` | confidence +0.05 | mild positive — relevant context but not the load-bearing fact |
   | `outdated` | decay +0.20 | capsule was correct at ingest but is now stale (renamed file, reverted decision, expired credential) |
   | `does_not_apply_here` | decay +0.10 | correct elsewhere but doesn't fit this scope/project — don't archive, just deprioritize |
   | `incorrect` | status → Archived (destructive) | you **verified** the capsule contains a factual error — same path as the admin UI's delete |

3. **Call the MCP tool:**

   ```
   mcp__mem__capability_capsule_feedback
     capability_capsule_id: <id>
     feedback_kind: <kind>
   ```

   Leave `tenant` unset so the server resolves from `MEM_TENANT`.

4. **Report.** On success render one line:

   ```
   ✦ feedback recorded · <kind> · <id>
   ```

   On `incorrect`, also remind the user: `archived (status=Archived) — this is permanent`.

   On failure (404 capsule not found / 4xx invalid kind), surface the server error verbatim.

### Guardrails

- **At most one signal per capsule per session.** Don't spam — pick the strongest signal that fits, don't fire `applies_here` for every search hit you skim.
- **Only fire on capsules you actually read and used.** Search hits you scrolled past don't count as feedback; silence is a valid signal too.
- **`incorrect` is destructive** (archives the row, permanent). Reserve for "I verified this is wrong," not "I disagree."
- This command does not run the consumed-text heuristic — it trusts the caller's judgment. Use it explicitly; don't fire it as a routine after every search.
