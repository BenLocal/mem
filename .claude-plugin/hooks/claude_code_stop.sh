#!/usr/bin/env bash
# Claude Code Stop hook. Throttle by user-exchange count (every 15);
# transcript path comes from the runtime's stdin payload. Mining +
# feedback + envelope formatting all happen inside `mem mine`
# (--with-feedback --format hook-stop), so this script only owns the
# stdin parse and the per-session throttle file.
#
# `set -uo pipefail` (no -e): a flaky mem-serve must never break the
# Stop event itself.
set -uo pipefail

LOG=/tmp/mem-stop-hook.log
# Self-rotate: keep last 200 lines if log > 256 KB.
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
# Per-session throttle — multiple sessions share `~/.mem/`, so a
# global throttle would let one session starve another.
LAST_SAVE_FILE="$HOME/.mem/last_save${SESSION_ID:+_$SESSION_ID}"
mkdir -p "$(dirname "$LAST_SAVE_FILE")"
LAST_SAVE=$(cat "$LAST_SAVE_FILE" 2>/dev/null || echo 0)

if [ $((EXCHANGE_COUNT - LAST_SAVE)) -lt 15 ]; then
    echo '{}'
    exit 0
fi

# Bump the throttle even on mine failure — otherwise a flaky mem-serve
# makes every Stop re-mine the same transcript.
echo "$EXCHANGE_COUNT" > "$LAST_SAVE_FILE"

exec mem mine "$TRANSCRIPT" \
    --tenant local \
    --agent claude-code \
    --with-feedback \
    --mine-timeout-secs 60 \
    --feedback-timeout-secs 30 \
    --format hook-stop
