#!/usr/bin/env bash
# Codex PreCompact hook: final mine before context compression so that
# memories from the about-to-be-compacted exchanges are not lost.
set -euo pipefail

INPUT=$(cat 2>/dev/null || echo '{}')
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // .transcript_path // empty' 2>/dev/null || echo "")

if [ -z "$TRANSCRIPT" ] && [ -d "$HOME/.codex/sessions" ]; then
    # shellcheck disable=SC2012
    TRANSCRIPT=$(ls -t "$HOME/.codex/sessions"/*.jsonl 2>/dev/null | head -1 || true)
fi

if [ -n "$TRANSCRIPT" ] && [ -f "$TRANSCRIPT" ]; then
    mem mine "$TRANSCRIPT" --agent codex >/dev/null 2>&1 &
fi

echo '{}'
