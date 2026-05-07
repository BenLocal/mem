#!/usr/bin/env bash
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
