---
description: Overview of the local mem memory service — endpoints, MCP tools, common workflows.
allowed-tools: Bash, Read
---

Invoke the `mem` skill (using the Skill tool) and present its overview to the user. Specifically, summarize:

1. Where `mem serve` is running (`MEM_BASE_URL`, default `http://127.0.0.1:3000`) and how to start it if it's down.
2. The most-used MCP tools under `plugin:mem:mem` (`capability_capsule_search`, `capability_capsule_ingest`, `capability_capsule_feedback`, etc.).
3. The CLI subcommands (`mem serve`, `mem mine`, `mem wake-up`, `mem repair`).
4. The other slash commands this plugin provides (`/mem:health`, `/mem:search`, `/mem:mine`, `/mem:wake-up`, `/mem:summary`, `/mem:save`, `/mem:feedback`).
5. The verbatim rule and feedback discipline (one signal per used memory, at most).

Keep the output concise — bullet lists, no walls of prose.
