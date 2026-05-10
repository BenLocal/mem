#!/usr/bin/env bash
# Codex SessionStart wrapper. `mem wake-up --format hook-session-start`
# emits the SessionStart envelope (or `{}` on empty body) directly,
# so the wrapper has no logic of its own.
set -euo pipefail
exec mem wake-up --tenant local --token-budget 800 --format hook-session-start
