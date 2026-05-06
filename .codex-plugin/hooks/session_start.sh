#!/usr/bin/env bash
# Codex SessionStart hook: inject ~800-token wake-up summary into the
# session as additionalContext. Silent on failure (mem service may be down).
set -euo pipefail

WAKEUP=$(mem wake-up --tenant local --token-budget 800 2>/dev/null || echo "")

if [ -n "$WAKEUP" ]; then
    ESCAPED=$(echo "$WAKEUP" | jq -Rs .)
    cat <<EOF
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": $ESCAPED
  }
}
EOF
else
    echo '{}'
fi
