#!/usr/bin/env bash
# PostToolUseFailure(Bash) error-recall — THIN wrapper.
#
# All logic lives in `mem hook recall-error` (src/cli/hook.rs): typed parse
# of the PostToolUseFailure payload (top-level `.error`, no tool_response),
# benign-failure filtering, signature extraction, the capsule search, and
# output formatting. Registered under PostToolUseFailure in hooks.json —
# PostToolUse fires only on SUCCESS, so error-recall must bind to the
# failure event or it never sees a failed command.
#
# Fail-open: if `mem` is missing or errors, emit `{}` so the hook never
# blocks the user's work. `mem` reads the payload from stdin and honors
# MEM_BASE_URL / MEM_TENANT (and MEM_RECALL_DISABLED / MEM_ERROR_RECALL_DISABLED).
set -uo pipefail
command -v mem >/dev/null 2>&1 || { echo '{}'; exit 0; }
mem hook recall-error 2>/dev/null || echo '{}'
exit 0
