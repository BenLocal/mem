#!/usr/bin/env bash
# PostToolUse hook for Bash. After a successful `git commit` of a
# substantive type (fix / feat / refactor / perf), nudge the agent to
# fire `capability_capsule_ingest` with `write_mode=propose` so the
# durable learning lands in the review queue (status =
# PendingConfirmation; visible via `list_pending_review`,
# promoted via `review_accept`). The `propose_experience` MCP tool
# writes to a parallel `episodes` table that's NOT in the review
# queue, so it's NOT what we nudge here — see commit on the
# misleading naming and the doc-fix in mempalace-diff-v2. Routine commits (chore(deps) / docs / pure
# test moves) and commits that didn't actually succeed are skipped.
#
# Output protocol: print `{}` to skip injection, or
# `{"hookSpecificOutput":{"hookEventName":"PostToolUse",
# "additionalContext":"<reminder>"}}` to inject a system reminder.
#
# `set -uo pipefail` (no -e) so a flaky jq / grep can never block the
# user's actual work — the hook is advisory, not load-bearing.
set -uo pipefail

LOG=/tmp/mem-posttooluse-hook.log
if [ -f "$LOG" ] && [ "$(stat -c %s "$LOG" 2>/dev/null || echo 0)" -gt 262144 ]; then
    tail -n 200 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
fi

INPUT=$(cat 2>/dev/null || echo '{}')
echo "$(date -Iseconds) posttooluse fired pid=$$ payload=${INPUT:0:200}" >> "$LOG"

# Gate 1: only Bash tool calls.
TOOL_NAME=$(echo "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null || echo "")
if [ "$TOOL_NAME" != "Bash" ]; then
    echo '{}'
    exit 0
fi

# Gate 2: command actually contained `git commit` (and is not --amend,
# which is fixing an already-nudged commit).
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null || echo "")
if ! echo "$COMMAND" | grep -qE '(^|[[:space:]&;|`(])git[[:space:]]+commit($|[[:space:]])'; then
    echo '{}'
    exit 0
fi
if echo "$COMMAND" | grep -qE '\-\-amend\b'; then
    echo '{}'
    exit 0
fi

# Gate 3: commit succeeded. Claude Code's tool_response exposes both
# `success: bool` and the raw stdout — git's success line looks like
# `[branch sha] subject` so we use that as the secondary signal.
SUCCESS=$(echo "$INPUT" | jq -r '.tool_response.success // .tool_response.is_error // empty' 2>/dev/null || echo "")
STDOUT=$(echo "$INPUT" | jq -r '.tool_response.stdout // .tool_response.output // empty' 2>/dev/null || echo "")
if [ "$SUCCESS" = "false" ] || ! echo "$STDOUT" | grep -qE '^\['; then
    echo '{}'
    exit 0
fi

# Gate 4: skip routine commit types. Pull the subject line off the
# `[branch sha] subject` envelope; if it starts with chore(deps) /
# docs( / chore(makefile) / chore(logging), the durable-learning
# threshold isn't usually met. The agent can always still propose
# manually if it disagrees.
SUBJECT=$(echo "$STDOUT" | head -n 1 | sed -E 's/^\[[^]]+\][[:space:]]*//')
if echo "$SUBJECT" | grep -qE '^(chore\(deps\)|chore\(makefile\)|chore\(logging\)|docs(\(|:)|test(\(|:)|style(\(|:))'; then
    echo '{}'
    exit 0
fi

# Emit the nudge. The reminder is intentionally specific about which
# MCP tool to call and what each arg means, so the agent doesn't have
# to re-read the SKILL.md to act on it.
#
# Heredoc-quoting notes: this is an unquoted `<<EOF` so $vars expand
# and backticks would trigger command substitution — backticks for
# inline-code in the reminder text are escaped (`\``). Embedded JSON
# quotes use the *single* backslash form (`\"`); writing `\\\"` here
# emits `\\"` in the JSON, which is two-backslash-then-quote and
# breaks the parse.
echo "$(date -Iseconds) emitting nudge for subject=$SUBJECT" >> "$LOG"
SUBJECT_JSON=${SUBJECT//\"/\\\"}
cat <<EOF
{
  "hookSpecificOutput": {
    "hookEventName": "PostToolUse",
    "additionalContext": "Commit just landed: \`${SUBJECT_JSON}\`. **Default action: call \`mcp__mem__capability_capsule_ingest\` with \`capability_capsule_type=\"experience\"\` and \`write_mode=\"propose\"\` now.** This writes a capsule row with status=PendingConfirmation — it sits in the review queue (visible via \`capability_capsule_list_pending_review\`), NOT the active pool, so a human or future agent must run \`review_accept\` (or \`review_edit_accept\` for edits) to promote, and over-proposing is harmless (one \`review_reject\` click discards a noise row). The threshold is low: any commit that touches business logic, non-trivial config, a bug fix, an architectural decision, or a learned API gotcha is worth proposing. Required args: capability_capsule_type=\"experience\", content (full cause/symptom/fix verbatim — never refine), scope (e.g. \"repo\" or \"project\"), write_mode=\"propose\". Optional but useful: summary (≤80 char headline), project (repo basename), source_agent (\"claude-code\"). Skip only for: typo-only commits, dependency bumps, pure formatting / rename-only refactors, or commits whose entire content was already captured by an earlier capsule in this session. NOTE: do NOT use \`capability_capsule_propose_experience\` — that one writes to a parallel \`episodes\` table that the review queue does not see. When in doubt → propose."
  }
}
EOF
