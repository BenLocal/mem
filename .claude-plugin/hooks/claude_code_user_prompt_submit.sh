#!/usr/bin/env bash
# UserPromptSubmit hook: query-relevant AUTO-RECALL.
#
# Why this exists: writes into mem are hook-driven (Stop -> `mem mine`,
# PostToolUse -> commit-nudge to `capability_capsule_ingest`), but reads
# were left to agent judgment + SKILL.md discipline, which decays to ~0
# over time (active recall flatlined while ingest kept climbing). This
# hook gives recall a deterministic driver, symmetric to the ingest
# nudge: on every substantive user prompt it runs ONE capsule search +
# ONE transcript search against the prompt text and injects the top hits
# as `additionalContext`, so the agent gets relevant prior decisions and
# past conversations without having to remember to pull them.
#
# Output protocol (same as the other hooks): print `{}` to inject
# nothing, or
#   {"hookSpecificOutput":{"hookEventName":"UserPromptSubmit",
#    "additionalContext":"<recalled context>"}}
# to prepend context to the model's view of this turn.
#
# `set -uo pipefail` (no -e) + fail-open everywhere: a slow/flaky
# mem-serve must NEVER block or delay the user's prompt. Every failure
# path emits `{}` and exits 0.
set -uo pipefail

BASE="${MEM_BASE_URL:-http://127.0.0.1:3000}"
TENANT="${MEM_TENANT:-local}"
# Relevance floor for auto-recall (stricter than the global default 25 to
# keep injected hits high-signal). Honored once the server supports a
# per-request `min_score`; ignored (forward-safe) by older builds.
MIN_SCORE="${MEM_RECALL_MIN_SCORE:-35}"
[[ "$MIN_SCORE" =~ ^[0-9]+$ ]] || MIN_SCORE=35

LOG=/tmp/mem-userprompt-hook.log
if [ -f "$LOG" ] && [ "$(stat -c %s "$LOG" 2>/dev/null || echo 0)" -gt 262144 ]; then
    tail -n 200 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
fi

INPUT=$(cat 2>/dev/null || echo '{}')
echo "$(date -Iseconds) userprompt fired pid=$$ payload=${INPUT:0:160}" >> "$LOG"

# Global escape hatch.
if [ "${MEM_RECALL_DISABLED:-0}" = "1" ]; then echo '{}'; exit 0; fi

# Need jq + curl; if either is missing, fail open.
command -v jq  >/dev/null 2>&1 || { echo '{}'; exit 0; }
command -v curl >/dev/null 2>&1 || { echo '{}'; exit 0; }

PROMPT=$(printf '%s' "$INPUT" | jq -r '.prompt // empty' 2>/dev/null || echo "")

# ---- Gates: skip prompts that aren't worth a recall round-trip --------
# Trim leading whitespace for the prefix checks.
TRIMMED="${PROMPT#"${PROMPT%%[![:space:]]*}"}"
[ -z "$TRIMMED" ] && { echo '{}'; exit 0; }
case "$TRIMMED" in
    /*|\!*) echo '{}'; exit 0 ;;                 # slash command / bash passthrough
esac
# Too short to carry intent.
if [ "${#TRIMMED}" -lt 4 ]; then echo '{}'; exit 0; fi
# Common continuations / acks that carry no query signal.
LC=$(printf '%s' "$TRIMMED" | tr '[:upper:]' '[:lower:]')
case "$LC" in
    继续|继续吧|嗯|好|好的|行|可以|go|ok|okay|yes|y|yep|yeah|sure|proceed|continue|"do it"|next|"go on")
        echo '{}'; exit 0 ;;
esac

# ---- Fire both searches in parallel (bounded latency) -----------------
CAPF=$(mktemp 2>/dev/null) || { echo '{}'; exit 0; }
TRF=$(mktemp 2>/dev/null)  || { rm -f "$CAPF"; echo '{}'; exit 0; }
trap 'rm -f "$CAPF" "$TRF"' EXIT

CAP_BODY=$(jq -n --arg q "$PROMPT" --arg t "$TENANT" --argjson ms "$MIN_SCORE" \
    '{query:($q[0:1000]),intent:"",scope_filters:[],token_budget:1200,caller_agent:"claude-code",expand_graph:false,tenant:$t,min_score:$ms}' 2>/dev/null)
TR_BODY=$(jq -n --arg q "$PROMPT" --arg t "$TENANT" \
    '{query:($q[0:1000]),tenant:$t,limit:3,context_window:1}' 2>/dev/null)

curl -sS --max-time 3 "$BASE/capability_capsules/search" \
    -H 'content-type: application/json' -d "$CAP_BODY" >"$CAPF" 2>/dev/null &
P1=$!
curl -sS --max-time 3 "$BASE/transcripts/search" \
    -H 'content-type: application/json' -d "$TR_BODY" >"$TRF" 2>/dev/null &
P2=$!
wait "$P1" 2>/dev/null
wait "$P2" 2>/dev/null

CAP=$(cat "$CAPF" 2>/dev/null); printf '%s' "$CAP" | jq -e 'type=="object"' >/dev/null 2>&1 || CAP='{}'
TR=$(cat "$TRF" 2>/dev/null);  printf '%s' "$TR"  | jq -e 'type=="object"' >/dev/null 2>&1 || TR='{"windows":[]}'

# ---- Merge into one compact recall block ------------------------------
JQ_PROG='
def clean: (. // "") | gsub("\n";" ") | gsub("\\s+";" ")
           | (if length>240 then .[0:240]+"…" else . end);
($cap.directives // [])        as $dir
| ($cap.relevant_facts // [])  as $facts
| ($cap.reusable_patterns // []) as $pat
| ($tr.windows // [])          as $win
| ([$dir,$facts,$pat,$win] | map(length) | add) as $n
| if $n == 0 then {}
  else
    ( ["🧠 mem auto-recall — memories & past conversations relevant to this prompt (auto-retrieved). Read before answering. If a hit is load-bearing, `capability_capsule_get` it for the verbatim content and then send `mcp__mem__memory_feedback` `useful` for that id — silence freezes ranking. Ignore if irrelevant."]
      + (if ($dir|length)>0 then ["","**Directives**"]
          + ($dir[0:3]|map("- " + (.text|clean) + "  `[" + .capability_capsule_id + "]`")) else [] end)
      + (if ($facts|length)>0 then ["","**Relevant facts**"]
          + ($facts[0:3]|map("- " + (.text|clean)
              + (if ((.code_refs//[])|length)>0 then " (" + ((.code_refs)|join(", ")) + ")" else "" end)
              + "  `[" + .capability_capsule_id + "]`")) else [] end)
      + (if ($pat|length)>0 then ["","**Reusable patterns**"]
          + ($pat[0:2]|map("- " + (.text|clean) + "  `[" + .capability_capsule_id + "]`")) else [] end)
      + (if ($win|length)>0 then ["","**Past conversations** (`transcripts_search` for full threads)"]
          + ($win[0:2]|map(
              . as $w
              | ($w.session_id // "?") as $sid
              | ([ $w.blocks[]? | select(.is_primary==true) ][0] // ($w.blocks[0] // {})) as $p
              | "- [" + ($sid|tostring|.[0:8]) + "] " + (($p.created_at // "")[0:10]) + ": " + (($p.content // "")|clean)
            )) else [] end)
      | join("\n")
    ) as $ctx
    | {hookSpecificOutput:{hookEventName:"UserPromptSubmit", additionalContext:$ctx}}
  end
'

OUT=$(jq -n --argjson cap "$CAP" --argjson tr "$TR" "$JQ_PROG" 2>/dev/null)
[ -z "$OUT" ] && OUT='{}'
HITS=$(printf '%s' "$OUT" | jq -r 'if .hookSpecificOutput then "inject" else "skip" end' 2>/dev/null || echo skip)
echo "$(date -Iseconds) result=$HITS query=${TRIMMED:0:80}" >> "$LOG"
printf '%s\n' "$OUT"
