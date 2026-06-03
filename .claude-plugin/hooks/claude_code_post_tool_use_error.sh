#!/usr/bin/env bash
# PostToolUse(Bash) ERROR-triggered AUTO-RECALL.
#
# The highest-ROI moment to recall is right after something breaks — a
# failed build, a test failure, a runtime panic / stack trace. The mem
# SKILL.md calls this trigger (d) ("error resembling a documented
# incident") but it was judgment-driven and never fired in practice.
# This hook makes it deterministic: when a Bash command fails, pull an
# error signature out of the output, search mem for related incidents,
# and inject them so the agent checks the prior fix before re-deriving.
#
# Separate from claude_code_post_tool_use.sh (the commit-nudge) on
# purpose — different trigger, different payload gating. Both are
# registered under PostToolUse(Bash); hooks.json runs them independently.
#
# Output protocol: `{}` to inject nothing, or
#   {"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"..."}}
# `set -uo pipefail` (no -e) + fail-open: a flaky mem-serve must never
# interfere with the user's command results.
set -uo pipefail

BASE="${MEM_BASE_URL:-http://127.0.0.1:3000}"
TENANT="${MEM_TENANT:-local}"

LOG=/tmp/mem-error-recall-hook.log
if [ -f "$LOG" ] && [ "$(stat -c %s "$LOG" 2>/dev/null || echo 0)" -gt 262144 ]; then
    tail -n 200 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
fi

INPUT=$(cat 2>/dev/null || echo '{}')
echo "$(date -Iseconds) error-recall fired pid=$$" >> "$LOG"

# Escape hatches.
if [ "${MEM_RECALL_DISABLED:-0}" = "1" ];       then echo '{}'; exit 0; fi
if [ "${MEM_ERROR_RECALL_DISABLED:-0}" = "1" ]; then echo '{}'; exit 0; fi
command -v jq   >/dev/null 2>&1 || { echo '{}'; exit 0; }
command -v curl >/dev/null 2>&1 || { echo '{}'; exit 0; }

# Gate: Bash only.
TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null || echo "")
[ "$TOOL_NAME" = "Bash" ] || { echo '{}'; exit 0; }

# Collect whatever output fields the runtime exposes (shape varies), then
# fall back to stringifying the whole tool_response.
RESP=$(printf '%s' "$INPUT" | jq -r '
    [ .tool_response.stderr?, .tool_response.stdout?, .tool_response.output?,
      ( .tool_response.content?
        | if type=="array" then (map(.text? // (if type=="string" then . else "" end)) | join("\n"))
          elif type=="string" then . else "" end ) ]
    | map(select(. != null and . != "")) | join("\n")' 2>/dev/null || echo "")
[ -z "$RESP" ] && RESP=$(printf '%s' "$INPUT" | jq -r '.tool_response // empty | tostring' 2>/dev/null || echo "")

# Failure detection: explicit flag OR error-shaped output.
SUCCESS=$(printf '%s' "$INPUT" | jq -r '.tool_response.success // empty' 2>/dev/null || echo "")
IS_ERR=$(printf '%s' "$INPUT" | jq -r '.tool_response.is_error // .tool_response.interrupted // empty' 2>/dev/null || echo "")
ERR_RE='error|panic|exception|traceback|fail(ed|ure)?|cannot|no such|undefined|unresolved|not found|fatal|denied|refused|time(d)? ?out|assertion|segfault|core dumped|E[0-9]{3,4}'
if [ "$SUCCESS" != "false" ] && [ "$IS_ERR" != "true" ] && ! printf '%s' "$RESP" | grep -qiE "$ERR_RE"; then
    echo '{}'; exit 0
fi

# Build an error signature from the most salient lines.
SIG=$(printf '%s' "$RESP" | grep -iE "$ERR_RE" | head -n 3 | tr '\n' ' ' | tr -s ' ')
[ -z "$SIG" ] && SIG=$(printf '%s' "$RESP" | head -n 3 | tr '\n' ' ' | tr -s ' ')
SIG=${SIG:0:400}
# Too little signal to search on.
if [ "${#SIG}" -lt 8 ]; then echo '{}'; exit 0; fi

# Per-session dedup: don't re-fire on the same error signature twice in a
# row (agents retry the same failing command repeatedly).
SID=$(printf '%s' "$INPUT" | jq -r '.session_id // empty' 2>/dev/null || echo "")
HASH=$(printf '%s' "$SIG" | cksum 2>/dev/null | cut -d' ' -f1)
STATE="/tmp/mem-error-recall-last${SID:+_$SID}"
LAST=$(cat "$STATE" 2>/dev/null || echo "")
if [ -n "$HASH" ] && [ "$LAST" = "$HASH" ]; then echo '{}'; exit 0; fi
[ -n "$HASH" ] && printf '%s' "$HASH" > "$STATE" 2>/dev/null

# Search mem for related incidents. `min_score` is honored once the
# per-request floor lands server-side; unknown fields are ignored until
# then (serde does not deny unknowns here), so this is forward-safe.
BODY=$(jq -n --arg q "$SIG" --arg t "$TENANT" \
    '{query:$q,intent:"resolve an error / find related incident",scope_filters:[],token_budget:1000,caller_agent:"claude-code",expand_graph:false,tenant:$t,min_score:30}' 2>/dev/null)
CAP=$(curl -sS --max-time 3 "$BASE/capability_capsules/search" \
    -H 'content-type: application/json' -d "$BODY" 2>/dev/null)
printf '%s' "$CAP" | jq -e 'type=="object"' >/dev/null 2>&1 || CAP='{}'

JQ_PROG='
def clean: (. // "") | gsub("\n";" ") | gsub("\\s+";" ")
           | (if length>240 then .[0:240]+"…" else . end);
($cap.directives // [])          as $dir
| ($cap.relevant_facts // [])    as $facts
| ($cap.reusable_patterns // []) as $pat
| ([$dir,$facts,$pat] | map(length) | add) as $n
| if $n == 0 then {}
  else
    ( ["⚠️ The last Bash command failed — mem found related incidents/fixes. Check these BEFORE re-deriving; if one matches, `capability_capsule_get` it for the verbatim fix and send `mcp__mem__memory_feedback` `useful` after. Ignore if unrelated."]
      + (if ($dir|length)>0 then ["","**Directives**"]
          + ($dir[0:2]|map("- " + (.text|clean) + "  `[" + .capability_capsule_id + "]`")) else [] end)
      + (if ($facts|length)>0 then ["","**Related incidents**"]
          + ($facts[0:3]|map("- " + (.text|clean)
              + (if ((.code_refs//[])|length)>0 then " (" + ((.code_refs)|join(", ")) + ")" else "" end)
              + "  `[" + .capability_capsule_id + "]`")) else [] end)
      + (if ($pat|length)>0 then ["","**Reusable fixes**"]
          + ($pat[0:2]|map("- " + (.text|clean) + "  `[" + .capability_capsule_id + "]`")) else [] end)
      | join("\n")
    ) as $ctx
    | {hookSpecificOutput:{hookEventName:"PostToolUse", additionalContext:$ctx}}
  end
'
OUT=$(jq -n --argjson cap "$CAP" "$JQ_PROG" 2>/dev/null)
[ -z "$OUT" ] && OUT='{}'
HITS=$(printf '%s' "$OUT" | jq -r 'if .hookSpecificOutput then "inject" else "skip" end' 2>/dev/null || echo skip)
echo "$(date -Iseconds) result=$HITS sig=${SIG:0:80}" >> "$LOG"
printf '%s\n' "$OUT"
