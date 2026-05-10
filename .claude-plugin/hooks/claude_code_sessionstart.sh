#!/usr/bin/env bash
# Wake-up envelope is rendered by `mem wake-up --format hook-session-start`
# itself; this wrapper only exists so the plugin manifest keeps a stable
# command path. Errors silently fall back to `{}` (skip injection) because
# `mem wake-up` already does that internally on empty body.
set -euo pipefail
exec mem wake-up --tenant local --token-budget 800 --format hook-session-start
