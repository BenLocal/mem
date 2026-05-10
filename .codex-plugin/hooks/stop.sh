#!/usr/bin/env bash
# Codex Stop hook. Mirror of the Claude Code variant; differences:
#   - Codex hook payload uses `transcriptPath` (camelCase), with
#     `transcript_path` accepted as a fallback for runtime variants.
#   - Codex doesn't always include a transcript path on the wire —
#     fall back to the freshest `~/.codex/sessions/*.jsonl`.
#   - Per-session throttle file is `~/.mem/codex_last_save…` so it
#     doesn't collide with Claude Code's counter.
#
# Mining + feedback + envelope formatting are all handled by
# `mem mine --with-feedback --format hook-stop`; the wrapper only
# owns stdin parse + transcript probe + throttle.
set -uo pipefail

INPUT=$(cat 2>/dev/null || echo '{}')
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // .transcript_path // empty' 2>/dev/null || echo "")
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null || echo "")

if [ -z "$TRANSCRIPT" ] && [ -d "$HOME/.codex/sessions" ]; then
    # shellcheck disable=SC2012
    TRANSCRIPT=$(ls -t "$HOME/.codex/sessions"/*.jsonl 2>/dev/null | head -1 || true)
fi

if [ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ]; then
    echo '{}'
    exit 0
fi

EXCHANGE_COUNT=$(grep -c '"type":"user"' "$TRANSCRIPT" 2>/dev/null || echo 0)
LAST_SAVE_FILE="$HOME/.mem/codex_last_save${SESSION_ID:+_$SESSION_ID}"
mkdir -p "$(dirname "$LAST_SAVE_FILE")"
LAST_SAVE=$(cat "$LAST_SAVE_FILE" 2>/dev/null || echo 0)

if [ $((EXCHANGE_COUNT - LAST_SAVE)) -lt 15 ]; then
    echo '{}'
    exit 0
fi

echo "$EXCHANGE_COUNT" > "$LAST_SAVE_FILE"

exec mem mine "$TRANSCRIPT" \
    --tenant local \
    --agent codex \
    --mine-timeout-secs 60 \
    --format hook-stop
