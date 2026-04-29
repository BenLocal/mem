#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // empty')

if [ -n "$TRANSCRIPT" ]; then
    mem mine "$TRANSCRIPT" --agent claude-code &
fi

echo '{}'
