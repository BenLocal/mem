#!/usr/bin/env bash
# Codex Stop hook: every ~15 user exchanges, run `mem mine` and emit a
# status line ("✦ mem · N memories + K blocks woven into the archive")
# via `systemMessage`. Mirrors the Claude Code variant; only differences
# are the per-runtime --agent label, the throttle file, and the transcript
# path probe (Codex hook payload field shape varies; fall back to the
# latest .jsonl under ~/.codex/sessions/).
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
# Per-session throttle (multi-session safety) — see Claude Code stop.sh.
LAST_SAVE_FILE="$HOME/.mem/codex_last_save${SESSION_ID:+_$SESSION_ID}"
mkdir -p "$(dirname "$LAST_SAVE_FILE")"
LAST_SAVE=$(cat "$LAST_SAVE_FILE" 2>/dev/null || echo 0)

if [ $((EXCHANGE_COUNT - LAST_SAVE)) -lt 15 ]; then
    echo '{}'
    exit 0
fi

MINE_OUT=$(timeout 60 mem mine "$TRANSCRIPT" --agent codex 2>&1 || true)

echo "$EXCHANGE_COUNT" > "$LAST_SAVE_FILE"

MEMS=$(echo "$MINE_OUT" | sed -n 's/.*memories sent=\([0-9]*\).*/\1/p' | head -1)
BLOCKS=$(echo "$MINE_OUT" | sed -n 's/.*blocks sent=\([0-9]*\).*/\1/p' | head -1)

if [ -n "$MEMS" ] && [ -n "$BLOCKS" ]; then
    MSG=$(printf '✦ mem · %s memories + %s blocks woven into the archive' "$MEMS" "$BLOCKS")
    jq -n --arg msg "$MSG" '{"systemMessage": $msg}'
else
    echo '{}'
fi
