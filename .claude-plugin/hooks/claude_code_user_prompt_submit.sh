#!/usr/bin/env bash
# UserPromptSubmit query-relevant auto-recall — THIN wrapper.
#
# All logic lives in `mem hook recall-prompt` (src/cli/hook.rs): prompt
# gating (skip slash/bang/short/continuations), the parallel capsule +
# transcript searches, merge, and output formatting.
#
# Fail-open: if `mem` is missing or errors, emit `{}` so a slow/flaky
# mem-serve never blocks or delays the user's prompt. `mem` reads the
# payload from stdin and honors MEM_BASE_URL / MEM_TENANT / MEM_RECALL_DISABLED.
set -uo pipefail
command -v mem >/dev/null 2>&1 || { echo '{}'; exit 0; }
mem hook recall-prompt 2>/dev/null || echo '{}'
exit 0
