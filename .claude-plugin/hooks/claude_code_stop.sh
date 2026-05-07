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

INPUT=$(cat 2>/dev/null || echo '{}')
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // empty' 2>/dev/null || echo "")
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

MEMS=$(echo "$MINE_OUT" | sed -n 's/.*memories sent=\([0-9]*\).*/\1/p' | head -1)
BLOCKS=$(echo "$MINE_OUT" | sed -n 's/.*blocks sent=\([0-9]*\).*/\1/p' | head -1)

if [ -n "$MEMS" ] && [ -n "$BLOCKS" ]; then
    MSG=$(printf '✦ mem · %s memories + %s blocks woven into the archive' "$MEMS" "$BLOCKS")
    jq -n --arg msg "$MSG" '{"systemMessage": $msg}'
else
    echo '{}'
fi
