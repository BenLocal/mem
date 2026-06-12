---
description: Overview of the local mem memory service — endpoints, MCP tools, common workflows.
allowed-tools: Bash, Read
---

Invoke the `mem` skill (using the Skill tool) and present its overview to the user. Specifically, summarize:

1. Where `mem serve` is running (`MEM_BASE_URL`, default `http://127.0.0.1:3000`) and how to start it if it's down.
2. The most-used MCP tools under `plugin:mem:mem` (`capability_capsule_search`, `capability_capsule_ingest`, `capability_capsule_feedback`, etc.).
3. The CLI subcommands (`mem serve`, `mem mcp`, `mem mine`, `mem wake-up`, `mem feedback-from-transcript`).
4. The other slash commands this plugin provides (`/mem:health`, `/mem:search`, `/mem:mine`, `/mem:wake-up`, `/mem:summary`, `/mem:save`, `/mem:feedback`).
5. The verbatim rule and feedback discipline (one signal per used memory, at most).
6. **How to send feedback** — present this section to the user verbatim-faithfully (it answers "召回的记忆过时了/不适用，我该怎么办"):

   Three ways, most convenient first:

   - **Just say it in conversation** (recommended, zero memory burden). Every
     recall banner line carries a `[mem_…]` id. Tell the agent in plain
     language — "刚召回的那条 XXX 已经过时了，发个 outdated" / "mem_…1632 跟当前项目无关，标
     does_not_apply_here" — and the agent calls
     `capability_capsule_feedback` per the discipline. Quoting the id is
     optional; describing which entry is enough.
   - **Slash command**: `/mem:feedback <capability_capsule_id> <kind>` —
     validates the kind, fires the MCP tool, echoes one confirmation line.
   - **Raw HTTP** (outside any agent session):
     `curl -s -X POST $MEM_BASE_URL/capability_capsules/feedback -H 'content-type: application/json' -d '{"tenant":"local","capability_capsule_id":"mem_…","feedback_kind":"outdated","note":"why"}'`

   Kind cheat-sheet (effects are immediate — the next `memory_search`
   already ranks differently; there is no delayed batch):

   | situation | kind | effect |
   |---|---|---|
   | was right, now stale (renamed file / reverted decision / expired credential) | `outdated` | decay +0.20 |
   | correct elsewhere, wrong scope/project here | `does_not_apply_here` | decay +0.10 (deprioritize, never archive) |
   | **verified** factual error | `incorrect` | ⚠️ archives the row, permanent — not for "I disagree" |
   | directly unblocked the task | `useful` | confidence +0.10 (strongest positive) |
   | relevant context, not load-bearing | `applies_here` | confidence +0.05 (the auto-loop's kind — rarely needs manual sending) |

   Discipline: at most ONE signal per capsule per session (send the
   strongest); only for entries actually read AND used/rejected — skimmed
   silence is itself a valid signal; put the reason in `note`, it lands
   verbatim in the `feedback_events` audit row. The negative channel
   (`outdated` / `does_not_apply_here`) is the part humans forget — without
   it, bad recalls only sink by slow time-decay.

Keep the output concise — bullet lists, no walls of prose.
