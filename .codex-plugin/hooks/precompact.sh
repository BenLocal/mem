#!/usr/bin/env bash
# Codex PreCompact wrapper. No throttle (context loss is irreversible).
# Mining + envelope formatting happen inside `mem mine --format
# hook-precompact`; this script only owns stdin parse + transcript
# probe (with the codex-specific session-dir fallback).
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

exec mem mine "$TRANSCRIPT" \
    --tenant local \
    --agent codex \
    --mine-timeout-secs 90 \
    --format hook-precompact
