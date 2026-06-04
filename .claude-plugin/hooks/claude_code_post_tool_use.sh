#!/usr/bin/env bash
# PostToolUse(Bash) commit-nudge — THIN wrapper.
#
# All logic lives in `mem hook commit-nudge` (src/cli/hook.rs): detect a
# successful, substantive `git commit` (via git's `[branch sha] subject`
# stdout envelope, since tool_response.success is not populated), skip
# routine types (chore(deps)/docs/test/style/--amend), and emit the
# propose-experience nudge. Bound to PostToolUse (success) on purpose.
#
# Fail-open: if `mem` is missing or errors, emit `{}`.
set -uo pipefail
command -v mem >/dev/null 2>&1 || { echo '{}'; exit 0; }
mem hook commit-nudge 2>/dev/null || echo '{}'
exit 0
