---
description: Print the mem wake-up context block (recent high-confidence memories under a token budget).
allowed-tools: Bash
---

Show the user the same wake-up context that the SessionStart hook injects at session start.

Procedure:

1. Run `mem wake-up --tenant "${MEM_TENANT:-local}" --token-budget 800`.
2. Pipe the markdown output to the user verbatim — the command emits a `## Recent Context` block followed by bullet items.
3. If the command errors (e.g. `mem` binary not on PATH), tell the user to run `cargo install --path .` from the mem repo, or invoke via `cargo run -- wake-up …`.

Use a larger `--token-budget` (e.g. `2000`) if the user asks for more context.
