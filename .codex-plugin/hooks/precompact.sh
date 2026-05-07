#!/usr/bin/env bash
# Codex PreCompact hook: final sync mine before context compression so
# memories from the about-to-be-compacted exchanges are not lost.
# Mirror of the Claude Code variant.
set -uo pipefail

INPUT=$(cat 2>/dev/null || echo '{}')
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // .transcript_path // empty' 2>/dev/null || echo "")

if [ -z "$TRANSCRIPT" ] && [ -d "$HOME/.codex/sessions" ]; then
    # shellcheck disable=SC2012
    TRANSCRIPT=$(ls -t "$HOME/.codex/sessions"/*.jsonl 2>/dev/null | head -1 || true)
fi

if [ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ]; then
    echo '{}'
    exit 0
fi

MINE_OUT=$(timeout 90 mem mine "$TRANSCRIPT" --agent codex 2>&1 || true)

MEMS=$(echo "$MINE_OUT" | sed -n 's/.*memories sent=\([0-9]*\).*/\1/p' | head -1)
BLOCKS=$(echo "$MINE_OUT" | sed -n 's/.*blocks sent=\([0-9]*\).*/\1/p' | head -1)

if [ -n "$MEMS" ] && [ -n "$BLOCKS" ]; then
    MSG=$(printf '✦ mem · pre-compact · %s memories + %s blocks archived' "$MEMS" "$BLOCKS")
    jq -n --arg msg "$MSG" '{"systemMessage": $msg}'
else
    echo '{}'
fi
