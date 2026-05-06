#!/usr/bin/env bash
# Codex Stop hook: every ~15 user exchanges, fire `mem mine` in background.
# Idempotent — same transcript line never re-ingests (idempotency_key handled
# by `mem mine` itself).
set -euo pipefail

INPUT=$(cat 2>/dev/null || echo '{}')

# Try common transcript-path field names. Codex hook payload fields may
# differ from Claude Code; fall back to latest jsonl in ~/.codex/sessions/.
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // .transcript_path // empty' 2>/dev/null || echo "")

if [ -z "$TRANSCRIPT" ] && [ -d "$HOME/.codex/sessions" ]; then
    # shellcheck disable=SC2012
    TRANSCRIPT=$(ls -t "$HOME/.codex/sessions"/*.jsonl 2>/dev/null | head -1 || true)
fi

if [ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ]; then
    echo '{}'
    exit 0
fi

EXCHANGE_COUNT=$(grep -c '"type":"user"' "$TRANSCRIPT" 2>/dev/null || echo 0)
LAST_SAVE_FILE="$HOME/.mem/codex_last_save"
mkdir -p "$(dirname "$LAST_SAVE_FILE")"
LAST_SAVE=$(cat "$LAST_SAVE_FILE" 2>/dev/null || echo 0)

if [ $((EXCHANGE_COUNT - LAST_SAVE)) -ge 15 ]; then
    mem mine "$TRANSCRIPT" --agent codex >/dev/null 2>&1 &
    echo "$EXCHANGE_COUNT" > "$LAST_SAVE_FILE"
fi

echo '{}'
