#!/usr/bin/env bash
# Claude Code PreCompact hook. No throttle (context loss is
# irreversible — every pre-compact deserves a final mine). Mining +
# feedback + envelope formatting all happen inside `mem mine`
# (--with-feedback --format hook-precompact); this wrapper only owns
# stdin parse + transcript existence check.
#
# Mine timeout is 90 s vs 60 s on Stop because the about-to-be-lost
# context is worth a little more slack on a long transcript.
set -uo pipefail

INPUT=$(cat 2>/dev/null || echo '{}')
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null || echo "")

if [ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ]; then
    echo '{}'
    exit 0
fi

exec mem mine "$TRANSCRIPT" \
    --tenant local \
    --agent claude-code \
    --with-feedback \
    --mine-timeout-secs 90 \
    --feedback-timeout-secs 30 \
    --format hook-precompact
