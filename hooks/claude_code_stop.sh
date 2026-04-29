#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // empty')

if [ -z "$TRANSCRIPT" ]; then
    echo '{}'
    exit 0
fi

EXCHANGE_COUNT=$(grep -c '"type":"user"' "$TRANSCRIPT" 2>/dev/null || echo 0)
LAST_SAVE_FILE="$HOME/.mem/last_save"
LAST_SAVE=$(cat "$LAST_SAVE_FILE" 2>/dev/null || echo 0)

if [ $((EXCHANGE_COUNT - LAST_SAVE)) -ge 15 ]; then
    mem mine "$TRANSCRIPT" --agent claude-code &
    echo "$EXCHANGE_COUNT" > "$LAST_SAVE_FILE"
fi

echo '{}'
