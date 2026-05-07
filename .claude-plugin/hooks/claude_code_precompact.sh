#!/usr/bin/env bash
# Claude Code PreCompact hook: final sync mine before context compression
# so memories from the about-to-be-compacted exchanges are not lost. Emit
# the same "✦ mem · …" systemMessage so the user sees the save happened.
#
# 90 s timeout (vs 60 s on Stop) — pre-compact runs ahead of irreversible
# context loss, so it's worth giving mine a little more slack on a long
# transcript.
set -uo pipefail

INPUT=$(cat 2>/dev/null || echo '{}')
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // empty' 2>/dev/null || echo "")

if [ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ]; then
    echo '{}'
    exit 0
fi

MINE_OUT=$(timeout 90 mem mine "$TRANSCRIPT" --agent claude-code 2>&1 || true)

MEMS=$(echo "$MINE_OUT" | sed -n 's/.*memories sent=\([0-9]*\).*/\1/p' | head -1)
BLOCKS=$(echo "$MINE_OUT" | sed -n 's/.*blocks sent=\([0-9]*\).*/\1/p' | head -1)

if [ -n "$MEMS" ] && [ -n "$BLOCKS" ]; then
    MSG=$(printf '✦ mem · pre-compact · %s memories + %s blocks archived' "$MEMS" "$BLOCKS")
    jq -n --arg msg "$MSG" '{"systemMessage": $msg}'
else
    echo '{}'
fi
