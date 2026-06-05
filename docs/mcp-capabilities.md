# mem MCP — Tool / Resource 能力清单

> **目的**：mem-mcp stdio 服务器对外暴露的所有工具的接口签名、HTTP 转发对照、典型用法。第三方 agent 想接入 mem 时先读这个；改 MCP surface 时先改代码再回来同步本文档（commit 引用本文件章节号 `docs(mcp): … (closes mcp §X)`）。
>
> **同步源**：`src/mcp/server.rs` 中的 `#[tool(...)]` 装饰器 + 对应 `*Args` struct + HTTP forward path。
>
> 配套文档：HTTP 接口流向看 [`api-data-flow.md`](./api-data-flow.md)，存储 schema 看 [`database-schema.md`](./database-schema.md)。

---

## 0. 架构与配置

```
agent (Claude Code / Codex / 其它 MCP 客户端)
  │   stdio JSON-RPC（rmcp, ProtocolVersion = 2024-11-05）
  ▼
┌────────────────────────────┐
│ mem mcp                    │   一进程，stdio in/out
│ MemMcpServer               │
│   tool_router (43 tools)   │
│   MemHttpClient (reqwest)  │
└─────────────┬──────────────┘
              │   HTTP（默认 127.0.0.1:3000）
              ▼
        mem serve (axum)
```

**没有 resource、没有 prompt** —— `get_info()` 仅声明 `enable_tools()`，本服务器只暴露 tools。

### 配置（`McpConfig::from_env`）

| 环境变量 | 默认 | 含义 |
|---|---|---|
| `MEM_BASE_URL` | `http://127.0.0.1:3000` | mem serve 的 HTTP 地址 |
| `MEM_TENANT` | `local` | 默认 tenant，调用方未传 `tenant` 字段时回填 |
| `MEM_MCP_EXPOSE_EMBEDDINGS` | unset | 设为 `1` 才会暴露三个 `embeddings_*` 管理工具 |

`server_info`：name=`mem-mcp`, version 跟随 crate（当前 0.1.1）。

### 错误模型

所有工具都返回 `CallToolResult`：

- 成功：`Content::text(<JSON>)`，body 是 `serde_json::to_string_pretty(...)` 的产物
- 失败：`is_error = Some(true) + Content::text(<人读错误信息>)`，**不抛 McpError**（外层永远 `Ok(...)`），所以客户端必须看 `is_error` 字段

写入类工具（`capability_capsule_ingest` / `_commit_fact` / `_propose_preference` / `episode_ingest`）成功时通过 `ok_json_with_content` 在 JSON 前加一行 `✓ <notice>: <content>` 摘要，便于 agent 在 transcript 里直接看到 "保存了什么"。

### 默认 tenant 解析

每个含 `tenant: Option<String>` 的工具都走 `resolve_tenant()`：

```rust
override_value
  .map(str::trim)
  .filter(|s| !s.is_empty())
  .map(String::from)
  .unwrap_or_else(|| self.default_tenant.clone())
```

—— 显式传入 `tenant` 时使用，否则回填 `MEM_TENANT`。

---

## 1. 工具清单（27 个，按业务域分组）

| # | 工具名 | 域 | HTTP 转发 | 默认 token_budget |
|---|---|---|---|---|
| 1 | `mem_health` | 健康 | `GET /health` | — |
| 2 | `capability_capsule_search` | 检索 | `POST /capability_capsules/search` | 400 |
| 3 | `capability_capsule_bootstrap` | 检索（任务启动） | `POST /capability_capsules/search` (response shaping) | 120 |
| 4 | `capability_capsule_search_contextual` | 检索（按 intent） | `POST /capability_capsules/search` (response shaping) | 400 |
| 5 | `capability_capsule_ingest` | 写入 | `POST /capability_capsules` | — |
| 6 | `capability_capsule_batch_ingest` | 写入（批量） | `POST /capability_capsules/batch` | — |
| 7 | `capability_capsule_commit_fact` | 写入（受验事实） | `POST /capability_capsules` | — |
| 8 | `capability_capsule_propose_preference` | 写入（提案） | `POST /capability_capsules` (write_mode=propose) | — |
| ~~9~~ | ~~`capability_capsule_propose_experience`~~ | **已移除** → `episode_ingest` | — | — |
| 10 | `capability_capsule_get` | 详情 | `GET /capability_capsules/{id}?tenant=…` | — |
| 11 | `capability_capsule_feedback` | 反馈 | `POST /capability_capsules/feedback` | — |
| ~~12~~ | ~~`capability_capsule_apply_feedback`~~ | **已移除** → `capability_capsule_feedback`（now 带 `note`） | — | — |
| 13 | `capability_capsule_list_pending_review` | 评审 | `GET /reviews/pending?tenant=…` | — |
| 14 | `capability_capsule_review_accept` | 评审 | `POST /reviews/pending/accept` | — |
| 15 | `capability_capsule_review_reject` | 评审 | `POST /reviews/pending/reject` | — |
| 16 | `capability_capsule_review_edit_accept` | 评审（编辑后接受） | `POST /reviews/pending/edit_accept` | — |
| 17 | `episode_ingest` | 写入（多步经验） | `POST /episodes` | — |
| 18 | `capability_capsule_graph_neighbors` | 图谱 | `GET /graph/neighbors/{node_id}` | — |
| 19 | `transcript_session_get` | 对话归档 | `POST /transcripts` | — |
| 20 | `transcripts_search` | 对话归档（hybrid 检索） | `POST /transcripts/search` | — |
| 21 | `entity_create` | 实体注册 | `POST /entities` | — |
| 22 | `entity_get` | 实体注册 | `GET /entities/{entity_id}?tenant=…` | — |
| 23 | `entity_add_alias` | 实体注册 | `POST /entities/{entity_id}/aliases` | — |
| 24 | `entity_list` | 实体注册 | `GET /entities?tenant=…[&kind&q&limit]` | — |
| 25 | `embeddings_list_jobs` | 管理（gated） | `GET /embeddings/jobs` | — |
| 26 | `embeddings_rebuild` | 管理（gated） | `POST /embeddings/rebuild` | — |
| 27 | `embeddings_providers` | 管理（gated） | `GET /embeddings/providers` | — |

---

## 2. 工具详细签名

每个条目结构：① 描述（来自 `#[tool(description = …)]` 原文）；② 参数（`*Args` struct 字段）；③ HTTP 转发；④ Service 路径；⑤ 典型用法。

### 2.1 `mem_health`

**描述**：`Check that the mem HTTP server is reachable (GET /health). Use when MCP tools fail to see if the service is up.`

**参数**：`EmptyArgs`（无）

**HTTP**：`GET /health` → 返回 `{ "reachable": true, "health_body": "<trim of ok>" }`，失败时 `is_error=true`。

**Service**：`http::health::router`（内联 `|| async { "ok" }`，不走 service 层）。

**典型用法**：MCP 工具报错时第一个调用，确认 `mem serve` 还活着。

---

### 2.2 `capability_capsule_search`

**描述**：`Search the shared mem service for compressed directives, facts, and patterns. Call early in a task; use scope_filters like repo:<name> to narrow results.`

**参数** (`CapabilityCapsuleSearchArgs`)

| 字段 | 类型 | 必填 | 默认 | 备注 |
|---|---|---|---|---|
| `query` | string | ✔ | | 用户问题或任务描述 |
| `intent` | string? | | `"general"` | 分支：general / wake_up / debugging / 等 |
| `scope_filters` | string[]? | | `[]` | 形如 `repo:mem` / `project:acme` / `scope:workspace` |
| `token_budget` | u32? | | 400 | 输出压缩预算 |
| `caller_agent` | string | ✔ | | 调用方名（claude-code / codex / cursor / …） |
| `expand_graph` | bool? | | true | 是否激活 graph 加分通道 |
| `tenant` | string? | | `MEM_TENANT` | 多租户 override |

**HTTP**：`POST /capability_capsules/search`，body 字段同上。

**返回**：`SearchCapabilityCapsuleResponse`（见 `domain/query.rs`），含 `directives` / `relevant_facts` / `reusable_patterns` / `suggested_workflow` / `recent_conversations`。

**Service**：`CapabilityCapsuleService::search` → `pipeline/retrieve.rs::rank_with_hybrid_and_graph` + `compress::compress`。

**典型用法**：每次进新任务时第一个调用，把检索结果作为 working context。`scope_filters: ["repo:mem"]` 把检索范围收窄到当前仓。

---

### 2.3 `capability_capsule_bootstrap`

**描述**：`Lightweight project-only bootstrap search for task-start context recovery.`

**参数** (`CapabilityCapsuleBootstrapArgs`)

| 字段 | 必填 | 备注 |
|---|---|---|
| `tenant`, `project`, `caller_agent`, `query` | ✔ | |
| `repo`, `module` | | 仅作 metadata，不进 scope_filters |
| `token_budget` | | 默认 120（轻量） |

**HTTP**：`POST /capability_capsules/search`，固定 `intent="bootstrap"` + `scope_filters=["project:<project>"]` + `expand_graph=false`。

**返回 shaping**：MCP 端跑 `pick_search_summary()` 过滤掉 `recent_conversations` 等大字段，只保留 `directives` / `relevant_facts` / `reusable_patterns` / `suggested_workflow`，进一步压短。

**Service**：同 2.2。

**典型用法**：项目级"任务启动 5 行注入"，比 `capability_capsule_search` 更精简。

---

### 2.4 `capability_capsule_search_contextual`

**描述**：`Intent-aware search for implementation, debugging, or review. Defaults to project scope and only widens when explicitly requested.`

**参数** (`CapabilityCapsuleSearchContextualArgs`)

| 字段 | 必填 | 备注 |
|---|---|---|
| `tenant`, `project`, `caller_agent`, `query`, `intent` | ✔ | `intent`：`implementation` / `debugging` / `review` |
| `repo`, `module` | | |
| `include_repo` | | 为 true 时把 `repo:<repo>` 加进 scope_filters；**要求 `repo` 也提供**，否则返回错误 |
| `include_personal` | | 为 true 时把 `scope:workspace` 加进 scope_filters |
| `token_budget` | | 默认 400 |

**HTTP**：`POST /capability_capsules/search`，含动态拼装的 `scope_filters` + `expand_graph=true`。

**返回 shaping**：同 2.3，跑 `pick_search_summary()`。

**Service**：同 2.2。

**典型用法**：在 implementation/debugging/review 三个具体场景下，按 intent 收窄检索；可选地把 personal workspace 也放进来。

---

### 2.5 `capability_capsule_ingest`

**描述**：`Create a memory in mem. Use write_mode propose for preferences; auto is fine for implementation facts.`

**参数** (`CapabilityCapsuleIngestArgs`)

| 字段 | 必填 | 默认 | 备注 |
|---|---|---|---|
| `tenant` | | `MEM_TENANT` | |
| `capability_capsule_type` | ✔ | | `implementation` / `experience` / `preference` / `episode` / `workflow` |
| `content` | ✔ | | verbatim，不可被服务端改写 |
| `evidence` | | `[]` | |
| `code_refs` | | `[]` | |
| `scope` | ✔ | | `global` / `project` / `repo` / `workspace` |
| `visibility` | | `"private"` | `private` / `shared` / `system` |
| `project`, `repo`, `module`, `task_type` | | | metadata |
| `tags` | | `[]` | |
| `source_agent` | | `"mem-mcp"` | |
| `idempotency_key` | | | 客户端去重 |
| `write_mode` | | `"auto"` | `auto`（可能直接 active）/ `propose`（进 review queue） |

**HTTP**：`POST /capability_capsules`。

**Service**：`CapabilityCapsuleService::ingest`（`service/capability_capsule_service.rs:131`）。

**返回**：`{ "capability_capsule_id": "mem_…", "status": "active" | "pending_confirmation" | … }`，前缀 `✓ Memory saved: <content 前 N 行>` 摘要。

**典型用法**：直接持久化已确认的事实/经验。`<mem-save>...</mem-save>` 标记 + transcript mining 也走这个端点。

---

### 2.6 `capability_capsule_batch_ingest`

**描述**：`Bulk-insert multiple capsules in one call (server folds N rows into one Lance write + one DuckDB refresh; bench shows 9-227x speedup over looping capability_capsule_ingest). Returns 201 with per-item {result: ok | err} preserving input order, or 207 if any item failed.`

**参数** (`CapabilityCapsuleBatchIngestArgs`)

| 字段 | 必填 | 备注 |
|---|---|---|
| `tenant` | | 缺省走 `MEM_TENANT`；批内每行共享 |
| `items` | ✔ | `Vec<CapabilityCapsuleBatchIngestItem>`，每项与 `capability_capsule_ingest` 字段同（无 per-item tenant） |

**HTTP**：`POST /capability_capsules/batch`，body 是 `Vec<HttpIngestMemoryRequest>`，每项已注入解析后的 tenant。

**返回**：`{items: [{result: "ok", capability_capsule_id, status} | {result: "err", error}]}`，全成功 201，部分失败 207 Multi-Status。

**Service**：`CapabilityCapsuleService::ingest_batch`（`service/capability_capsule_service.rs`），把 N 条的 idempotency 探针 + session resolve + 验证保留 per-row，但 **insert / graph edge sync / embedding job enqueue / touch_session 全部合并成单次调用**——一次 Lance manifest commit + 一次 DuckDB refresh。

**典型用法**：`mem mine` 类批量回填、agent 一次任务完成后多 capsule 落地。bench 数据见 [`api-data-flow.md §3.7`](./api-data-flow.md#37-批量写入端点性能bench)：N=100 时 capsule path 9.1×、transcript path 226×。

---

### 2.7 `capability_capsule_commit_fact`

**描述**：`Commit a verified project fact. Uses auto write mode and project scope.`

**参数** (`CapabilityCapsuleCommitFactArgs`)

必填：`project`, `caller_agent`, `source_agent`, `summary`, `content`, `evidence`。可选：`tenant`, `repo`, `module`, `tags`, `idempotency_key`。

**HTTP**：`POST /capability_capsules`，固化以下字段：

```
capability_capsule_type = "implementation"
content                 = "<summary>\n\n<content>"
scope                   = "project"
visibility              = "private"
write_mode              = "auto"
tags                    = [...] + ["caller_agent:<caller_agent>"]
```

**Service**：同 2.5。

**典型用法**：对 implementation 类事实的快速通道——参数比 `capability_capsule_ingest` 少，但语义固定。

---

### 2.8 `capability_capsule_propose_preference`

**描述**：`Propose a preference for review. Uses the standard memories endpoint with write_mode=propose.`

**参数**：与 2.6 类似，去掉 `idempotency_key`、`evidence` 改成 optional。

**HTTP**：`POST /capability_capsules`，固化 `capability_capsule_type=preference` + `write_mode=propose` + `scope=project` + `visibility=private`。

**Service**：同 2.5。Service 层 `initial_status()` 把 `(preference, propose)` 落到 `pending_confirmation`，进 review queue。

**典型用法**：agent 提议一条偏好（"这个项目用 bun 不用 npm"），等人审。

---

### 2.9 `capability_capsule_propose_experience` — **已移除（2026-06-05，oss MCP 瘦身）**

它只是 `episode_ingest` 的"steps=[] + project 作用域"预设，名字却带 capsule/experience 误导（实际写 `episodes` 表，不进 capsule review 队列，历史上让 agent 误用）。→ 直接用 `episode_ingest`（episode/workflow 提取），或 `capability_capsule_ingest` `write_mode=propose`（要人审的 experience capsule）。

---

### 2.10 `capability_capsule_get`

**描述**：`Fetch one memory by id (detail, version chain, graph links, embedding metadata).`

**参数** (`CapabilityCapsuleGetArgs`)：`capability_capsule_id`（必填），`tenant`（默认 `MEM_TENANT`）。

**HTTP**：`GET /capability_capsules/{id}?tenant=…`，`{id}` 走 `encode_segment()` 百分号编码。

**Service**：`CapabilityCapsuleService::get_capability_capsule_detail`（含 version chain + feedback summary + embedding meta）。

**返回**：`CapabilityCapsuleDetailResponse`（`domain/capability_capsule.rs`）。

**典型用法**：检索结果给的只是 `capability_capsule_id` 列表时，需要详情就调这个；admin Web 也走它。

---

### 2.11 `capability_capsule_feedback`

**描述**：`Record feedback on a memory to adjust future ranking.`

**参数** (`CapabilityCapsuleFeedbackArgs`)：

| 字段 | 必填 | 备注 |
|---|---|---|
| `capability_capsule_id` | ✔ | |
| `feedback_kind` | ✔ | `useful` / `outdated` / `incorrect` / `applies_here` / `does_not_apply_here` |
| `tenant` | | |
| `note` | | 可选自由文本，verbatim 写入 `feedback_events.note`（原 `_apply_feedback` 的能力，2026-06-05 并入本工具） |

**HTTP**：`POST /capability_capsules/feedback`，body：`{tenant, capability_capsule_id, feedback_kind, note?}`。

**Service**：`CapabilityCapsuleService::submit_feedback` → `Store::apply_feedback`（写 `feedback_events` + 调 `confidence/decay/status`）。

**反馈影响表**（详见 [`database-schema.md`](./database-schema.md) §4.4）：

| feedback_kind | confidence Δ | decay Δ | side effect |
|---|---|---|---|
| `useful` | +0.10 | 0 | last_validated_at = now |
| `applies_here` | +0.05 | 0 | — |
| `outdated` | 0 | +0.20 | — |
| `does_not_apply_here` | 0 | +0.10 | — |
| `incorrect` | 0 | 0 | status → archived（**不可逆**） |

**典型用法**：检索命中的胶囊真用上了 → 发 `useful`；过期了 → `outdated`；事实错了 → `incorrect`。每会话每胶囊只发一次最强的信号；详见 `CLAUDE.md` 的反馈纪律段落。

---

### 2.12 `capability_capsule_apply_feedback` — **已移除（2026-06-05，oss MCP 瘦身）**

它与 2.11 `capability_capsule_feedback` 转发到**同一条** `POST /capability_capsules/feedback`，只是参数名 `kind` vs `feedback_kind`（`project`/`caller_agent` 仅自描述、不进 body）。唯一实际差异 `note` 已并入 2.11（`feedback` 现接受可选 `note`，写 `feedback_events.note`）。→ 统一用 `capability_capsule_feedback`。

---

### 2.13 `capability_capsule_list_pending_review`

**描述**：`List memories awaiting human confirmation for this tenant.`

**参数** (`TenantOnlyArgs`)：`tenant`（默认 `MEM_TENANT`）。

**HTTP**：`GET /reviews/pending?tenant=…`。

**Service**：`CapabilityCapsuleService::list_pending_review` → `Store::list_pending_review`（filter `status='pending_confirmation'`）。

**典型用法**：admin / review UI 拉待审列表。

---

### 2.14 `capability_capsule_review_accept`

**描述**：`Accept a pending memory (activate without edits). Use after human confirms.`

**参数** (`ReviewSimpleArgs`)：`capability_capsule_id` + `tenant`。

**HTTP**：`POST /reviews/pending/accept`，body：`{tenant, capability_capsule_id}`。

**Service**：`CapabilityCapsuleService::accept_pending` → `Store::accept_pending`（status 迁移 pending → active）。

**典型用法**：人审通过、原文不动。

---

### 2.15 `capability_capsule_review_reject`

**描述**：`Reject a pending memory (mark rejected, no successor).`

**参数 / HTTP / Service**：与 2.13 同形，端点为 `POST /reviews/pending/reject`，状态迁移 pending → rejected。

**典型用法**：人审驳回。**这是软删除**（保留行 + status=rejected），与 `capability_capsule_feedback feedback_kind=incorrect` 的归档语义不同（后者是反馈链路触发）。

---

### 2.16 `capability_capsule_review_edit_accept`

**描述**：`Edit pending memory content then accept: creates an active successor and rejects the original pending row.`

**参数** (`ReviewEditAcceptArgs`)：

| 字段 | 必填 | 备注 |
|---|---|---|
| `capability_capsule_id` | ✔ | 待审行 id |
| `summary`, `content` | ✔ | 编辑后的新内容 |
| `evidence`, `code_refs`, `tags` | | 改写后的 metadata |
| `tenant` | | |

**HTTP**：`POST /reviews/pending/edit_accept`。

**Service**：`CapabilityCapsuleService::edit_and_accept_pending` → 写新版本（active）+ 拒绝原 pending 行 + 在 `supersedes_capability_capsule_id` 链上挂上来源。

**典型用法**：人审看到原文不对但要保留意图，在 review UI 里改写后接受。

---

### 2.17 `episode_ingest`

**描述**：`Record a successful multi-step episode; may produce workflow candidates.`

**参数** (`EpisodeIngestArgs`)：

| 字段 | 必填 | 默认 | 备注 |
|---|---|---|---|
| `goal`, `steps`, `outcome` | ✔ | | 三段式 episode |
| `evidence` | | `[]` | |
| `scope` | | `"workspace"` | |
| `visibility` | | `"private"` | |
| `project`, `repo`, `module`, `tags` | | | |
| `source_agent` | | `"mem-mcp"` | |
| `idempotency_key` | | | |
| `tenant` | | | |

**HTTP**：`POST /episodes`。

**Service**：同 2.8（`ingest_episode`），但 steps 不为空 + 整套 episode 字段都填。命中条件后，service 层会自动跑 `workflow::maybe_extract_workflow` —— 把多个相似 episode 抽成一条 workflow 类型的 capsule。

**典型用法**：完成一个多步任务后留底，触发 workflow 自动归纳。

---

### 2.18 `capability_capsule_graph_neighbors`

**描述**：`List graph edges adjacent to a node id (e.g. module:mem:billing, project:acme). Complements capability_capsule_search when expand_graph is not enough.`

**参数** (`GraphNeighborsArgs`)：`node_id`（必填，形如 `capability_capsule:<uuid>` / `entity:<uuid>` / `project:<name>` / `module:<name>:<sub>`）。

**HTTP**：`GET /graph/neighbors/{node_id}`，`{node_id}` 走 `encode_segment()` 百分号编码。

**Service**：`http::graph::graph_neighbors` → `Store::neighbors`（filter `valid_to IS NULL` 取活态边）。

**典型用法**：从一个 entity 出发查关联 capsule / 反向查 entity 被哪些 capsule 引用。

---

### 2.19 `transcript_session_get`

**描述**：`Fetch the full block sequence for one Claude Code transcript session, identified by 'session_id' (as exposed on the wake-up response's 'recent_conversations[].session_id'). Returns chronological text/thinking/tool blocks; use this when you saw a session reference in the wake-up payload and want the full conversation.`

**参数** (`TranscriptSessionGetArgs`)：

| 字段 | 必填 | 备注 |
|---|---|---|
| `session_id` | ✔ | |
| `tenant` | | |
| `limit` | | 默认 200，上限 1000（`TranscriptService::get_by_session_paged` 服务端封顶） |

**HTTP**：`POST /transcripts`，body：`{tenant, session_id, limit}`。

**Service**：`TranscriptService::get_by_session` / `get_by_session_paged`（视服务端是否走分页路径）。

**返回**：`{ messages: [...ConversationMessage], next_cursor?, has_more }`。

**典型用法**：wake-up 响应里看到一个 session_id 想反查全文时用。

---

### 2.20 `transcripts_search`

**描述**：`Hybrid (BM25 + semantic) search over the verbatim transcript archive. Returns merged context windows around each primary hit. Use to recall earlier conversations beyond what wake-up surfaces; pair with transcript_session_get to fetch full sessions.`

**参数** (`TranscriptSearchArgs`)

| 字段 | 必填 | 备注 |
|---|---|---|
| `query` | ✔ | 空字符串走 recent-time 浏览路径 |
| `tenant` | | 默认 `MEM_TENANT` |
| `session_id` | | 限定一个 session |
| `role` | | `user` / `assistant` / `system` |
| `block_type` | | `text` / `tool_use` / `tool_result` / `thinking` |
| `time_from`, `time_to` | | ISO-8601 字符串字面比较 |
| `limit` | | 1..=100，默认 20 |
| `anchor_session_id` | | 该 session 的块作为锚定加权候选 |
| `context_window` | | ±N 块上下文，capped at 10 |
| `include_tool_blocks_in_context` | | 默认 false（context window 排除 tool 块） |

**HTTP**：`POST /transcripts/search`，body 字段同上。

**返回**：`{ windows: [{session_id?, blocks: [...], primary_ids: [...], score}, ...] }`，每个 window 含 primary 命中 + 上下文块 + 是否为 primary 的标记。

**Service**：`TranscriptService::search` → BM25 + semantic 双通道融合 + context window hydrate + window merge。

**典型用法**：agent 想搜历史对话（"之前聊过 X 怎么处理？"），先打这个，从 `windows[].session_id` 里挑一个再用 `transcript_session_get` 拉全文。比 `mem mine` 单条 transcript 检索更通用。

---

### 2.21 `entity_create`

**描述**：`Create or resolve a canonical entity in the registry. Idempotent on alias hit (re-POSTing the same canonical_name returns the existing entity_id). Returns 201 / 409 (alias already bound to a different entity).`

**参数** (`EntityCreateArgs`)

| 字段 | 必填 | 备注 |
|---|---|---|
| `canonical_name` | ✔ | 显示名（保留大小写） |
| `kind` | ✔ | `topic` / `project` / `repo` / `module` / `workflow` |
| `aliases` | | 额外别名列表；`canonical_name` 隐式也是 alias |
| `tenant` | | 默认 `MEM_TENANT` |

**HTTP**：`POST /entities`，body：`{tenant, canonical_name, kind, aliases}`。

**返回**：201 + `{entity_id, canonical_name, kind, aliases: [...]}`，或 409 + `{existing_entity_id, conflicting_alias}` 当任一别名已被另一实体占用。

**Service**：`EntityService::create_with_aliases` → `Store::resolve_or_create` + `add_alias`（事务式，失败时回滚已写入的 alias）。

**典型用法**：把胶囊 `topics` 字段里的字符串规整化到稳定 entity_id；agent 启动时给每个项目/仓库注册 canonical entity。

---

### 2.22 `entity_get`

**描述**：`Fetch one entity (canonical_name, kind, aliases) by entity_id. Returns 404 when the id is unknown.`

**参数** (`EntityGetArgs`)：`entity_id`（必填，走 `encode_segment` 编码）+ `tenant`（默认 `MEM_TENANT`）。

**HTTP**：`GET /entities/{entity_id}?tenant=…`。

**返回**：`EntityWithAliases { entity_id, canonical_name, kind, created_at, aliases: [...] }` 或 404。

**Service**：`EntityService::get` → `Store::get_entity`（DuckDB JOIN entities + entity_aliases）。

**典型用法**：拿到一个 `entity:<uuid>` 想查它实际是哪个名字 / 有哪些别名时用。

---

### 2.23 `entity_add_alias`

**描述**：`Declare an additional alias for an existing entity. Returns 200 (inserted / already_on_same_entity) or 409 (conflict_with_different_entity).`

**参数** (`EntityAddAliasArgs`)：`entity_id`（必填）、`alias`（必填）、`tenant`（默认 `MEM_TENANT`）。

**HTTP**：`POST /entities/{entity_id}/aliases`，body：`{tenant, alias}`。

**返回**：

- 200 `{outcome: "inserted", existing_entity_id: null}`
- 200 `{outcome: "already_on_same_entity", existing_entity_id: <self>}`
- 409 `{outcome: "conflict_with_different_entity", existing_entity_id: <other>}`

**Service**：`EntityService::add_alias` → `Store::add_alias`（带规整化 lowercase + whitespace-collapse）。

**典型用法**：agent 在写入时遇到 entity 别名分歧（`mem` vs `mem-rs` vs `MEM`），手动绑到同一 entity_id 上。

---

### 2.24 `entity_list`

**描述**：`List entities for the tenant, ordered by created_at desc. Supports filtering by kind and substring q on canonical_name. Default limit 50, server-side cap 100.`

**参数** (`EntityListArgs`)：`tenant?`, `kind?`, `q?`（substring on canonical_name）, `limit?`（1..=100，默认 50）。

**HTTP**：`GET /entities?tenant=…&limit=…[&kind=…&q=…]`。

**返回**：`{entities: [...EntityWithAliases]}`。

**Service**：`EntityService::list` → `Store::list_entities`。

**典型用法**：admin / debug：看本 tenant 注册了哪些 entity，按 kind 浏览。

---

### 2.25 `embeddings_list_jobs` 🔒

**描述**：`Admin: list embedding jobs (requires MEM_MCP_EXPOSE_EMBEDDINGS=1).`

**Gating**：`expose_embeddings == false` 时返回 `embeddings tools are disabled; set MEM_MCP_EXPOSE_EMBEDDINGS=1 to enable`（`is_error=true`）。

**参数** (`EmbeddingsListJobsArgs`)：`tenant`, `status`, `capability_capsule_id`, `limit`（1..=10000，默认 200）。

**HTTP**：`GET /embeddings/jobs?tenant=…&limit=…[&status=…&capability_capsule_id=…]`。

**Service**：`http::embeddings::list_jobs` → `Store::list_embedding_jobs`（LanceStore native）。

**典型用法**：debug 卡住的向量化队列。

---

### 2.26 `embeddings_rebuild` 🔒

**描述**：`Admin: enqueue embedding rebuild; force clears vector row and stale live jobs server-side.`

**Gating**：同 2.19。

**参数** (`EmbeddingsRebuildArgs`)：`tenant`, `capability_capsule_ids`, `force`。

**HTTP**：`POST /embeddings/rebuild`。

**Service**：`http::embeddings::rebuild` → 多步副作用：（可选 force 时）清 vector 行 + 关掉 live jobs + 重新入队 `embedding_jobs`。

**典型用法**：embed 模型升级后强制重建向量；批量补全缺失向量。

---

### 2.27 `embeddings_providers` 🔒

**描述**：`Admin: describe configured embedding provider and dimension.`

**Gating**：同 2.19。

**参数**：`EmptyArgs`（无）。

**HTTP**：`GET /embeddings/providers`。

**Service**：`http::embeddings::providers` → 当前 provider id + 维度 + model 名。

**典型用法**：确认运行时实际加载的 embedder 是哪个（`fake` / `embedanything` / `openai`）。

---

## 3. Resource / Prompt

**Resources**：无。`get_info()` 仅声明 `enable_tools()`，未声明 `enable_resources()` / `enable_prompts()`。

**Prompts**：无。

如果需要把 admin Web、`api-data-flow.md` 之类的静态文档暴露成 resource，需要新增 `#[resource]` 装饰 + `enable_resources()`。这是已知 backlog，未实施。

---

## 4. 与 HTTP 端点对照表

| HTTP 端点 | 方法 | MCP 工具 |
|---|---|---|
| `/health` | GET | `mem_health` |
| `/capability_capsules/search` | POST | `capability_capsule_search` / `_bootstrap` / `_search_contextual` |
| `/capability_capsules` | POST | `capability_capsule_ingest` / `_commit_fact` / `_propose_preference` |
| `/capability_capsules/batch` | POST | `capability_capsule_batch_ingest`（性能见 [`api-data-flow.md §3.7`](./api-data-flow.md#37-批量写入端点性能bench)） |
| `/capability_capsules/{id}` | GET | `capability_capsule_get` |
| `/capability_capsules/{id}` | DELETE | （admin Web 独享，MCP 未暴露） |
| `/capability_capsules/feedback` | POST | `capability_capsule_feedback` / `_apply_feedback` |
| `/episodes` | POST | `episode_ingest` |
| `/reviews/pending` | GET | `capability_capsule_list_pending_review` |
| `/reviews/pending/accept` | POST | `capability_capsule_review_accept` |
| `/reviews/pending/reject` | POST | `capability_capsule_review_reject` |
| `/reviews/pending/edit_accept` | POST | `capability_capsule_review_edit_accept` |
| `/graph/neighbors/{node_id}` | GET | `capability_capsule_graph_neighbors` |
| `/transcripts` | POST | `transcript_session_get` |
| `/transcripts/messages` | POST | （CLI 独享，`mem mine` 调） |
| `/transcripts/messages/batch` | POST | （CLI 独享） |
| `/transcripts/search` | POST | `transcripts_search` |
| `/transcripts/sessions` | GET | （admin Web 独享） |
| `/entities` | POST | `entity_create` |
| `/entities` | GET | `entity_list` |
| `/entities/{entity_id}` | GET | `entity_get` |
| `/entities/{entity_id}/aliases` | POST | `entity_add_alias` |
| `/embeddings/jobs` | GET | `embeddings_list_jobs` 🔒 |
| `/embeddings/rebuild` | POST | `embeddings_rebuild` 🔒 |
| `/embeddings/providers` | GET | `embeddings_providers` 🔒 |

🔒 = 受 `MEM_MCP_EXPOSE_EMBEDDINGS=1` 控制。

**MCP 未暴露但 HTTP 有的能力**（剩余）：

- `/transcripts/messages` / `/transcripts/messages/batch` —— transcript 写入，故意不暴露给 agent（mining 由 `mem mine` CLI 在 hook 里跑）
- `/transcripts/sessions` —— 跨 session 聚合摘要，admin Web 用；agent 通过 `capability_capsule_search` 的 `recent_conversations` 段拿到等价信息
- `DELETE /capability_capsules/{id}` —— admin 硬删除，禁止 agent 自己调

---

## 5. 已知 placeholder / 未完工

- **没有 resources/prompts**：rmcp 已经支持，但本服务器没声明 `enable_resources()` / `enable_prompts()`。如果要把 `docs/api-data-flow.md` / `docs/database-schema.md` / 本文件暴露成 resource 让 agent 自取，需要新增 `#[resource]` 装饰。这是 backlog，不是 bug。
- **`/transcripts/messages` 仍未暴露给 MCP**：故意保守的产品决策——transcript ingest 由 `mem mine` CLI（被 hook 触发）独占，避免 agent 自己往归档里写引导性内容。如有第三方 ingest 需求再开。
