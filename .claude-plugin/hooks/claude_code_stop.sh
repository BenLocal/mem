#!/usr/bin/env bash
# Claude Code Stop hook: every ~15 user exchanges, run `mem mine` and emit
# a status notification ("✦ mem · N memories + K blocks woven into the
# archive") via `systemMessage`. Mining is synchronous so the count is
# accurate at the time the line shows; on a typical 15-exchange increment
# the post-loopback HTTP POSTs finish in well under a second.
#
# `set -uo pipefail` (no -e): we want to keep going past mine failures so a
# transient mem-serve outage never breaks the Stop event itself.
set -uo pipefail

LOG=/tmp/mem-stop-hook.log
# Self-rotate: if log > 256 KB, keep only the last 200 lines.
if [ -f "$LOG" ] && [ "$(stat -c %s "$LOG" 2>/dev/null || echo 0)" -gt 262144 ]; then
    tail -n 200 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
fi
echo "$(date -Iseconds) stop fired pid=$$" >> "$LOG"

INPUT=$(cat 2>/dev/null || echo '{}')
echo "$(date -Iseconds) input=$INPUT" >> "$LOG"
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null || echo "")
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null || echo "")

if [ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ]; then
    echo '{}'
    exit 0
fi

EXCHANGE_COUNT=$(grep -c '"type":"user"' "$TRANSCRIPT" 2>/dev/null || echo 0)
# Throttle file is per-session — multiple Claude Code sessions share one
# `~/.mem/` dir, so a global throttle would have one session's count
# starve another from ever crossing the 15-msg threshold. Falls back to
# `last_save` (no suffix) when session_id is missing.
LAST_SAVE_FILE="$HOME/.mem/last_save${SESSION_ID:+_$SESSION_ID}"
mkdir -p "$(dirname "$LAST_SAVE_FILE")"
LAST_SAVE=$(cat "$LAST_SAVE_FILE" 2>/dev/null || echo 0)

if [ $((EXCHANGE_COUNT - LAST_SAVE)) -lt 15 ]; then
    echo '{}'
    exit 0
fi

# Sync mine, capped at 60 s. Output looks like:
#   "Mined: memories sent=3/3 blocks sent=3244/3244 (server-side dedup applied)"
MINE_OUT=$(timeout 60 mem mine "$TRANSCRIPT" --agent claude-code 2>&1 || true)

echo "$EXCHANGE_COUNT" > "$LAST_SAVE_FILE"

# Auto-feedback: scan the same transcript for mcp__mem__memory_search
# results whose text was referenced in subsequent assistant blocks, and
# POST `applies_here` for each. Capped at 30 s; failures are logged to
# stderr but never break the hook (we don't want a flaky feedback round
# to hide a successful mine).
FEEDBACK_OUT=$(timeout 30 mem feedback-from-transcript "$TRANSCRIPT" --tenant local 2>&1 || true)
FEEDBACK_SENT=$(echo "$FEEDBACK_OUT" | sed -n 's/.*sent=\([0-9]*\).*/\1/p' | head -1)

MEMS=$(echo "$MINE_OUT" | sed -n 's/.*memories sent=\([0-9]*\/[0-9]*\).*/\1/p' | head -1)
BLOCKS=$(echo "$MINE_OUT" | sed -n 's/.*blocks sent=\([0-9]*\/[0-9]*\).*/\1/p' | head -1)

if [ -n "$MEMS" ] && [ -n "$BLOCKS" ]; then
    if [ -n "$FEEDBACK_SENT" ] && [ "$FEEDBACK_SENT" != "0" ]; then
        MSG=$(printf '✦ mem · %s memories + %s blocks archived · %s feedback applied' "$MEMS" "$BLOCKS" "$FEEDBACK_SENT")
    else
        MSG=$(printf '✦ mem · %s memories + %s blocks woven into the archive' "$MEMS" "$BLOCKS")
    fi
    jq -n --arg msg "$MSG" '{"systemMessage": $msg}'
else
    echo '{}'
fi
