# mem 存储层 — 表 / Schema / 依赖关系

> **目的**：12 张 LanceDB 表的 schema、写入路径、查询路径、跨表依赖。半年后回头查"`graph_edges` 是哪个 service 在写 / 在读"，先翻这个。
>
> **同步源**：`src/storage/lance_store/mod.rs` 的 `*_schema()` 函数 + `src/storage/lance_store/**` 写入方法 + `src/storage/duckdb_query/**` 读取方法。改 schema 时**先改代码，再来更新本文档**，commit 引用本文件章节号（`docs(schema): … (closes schema §X)`）。
>
> 配套文档：HTTP 接口流向看 [`api-data-flow.md`](./api-data-flow.md)，路线图看 [`ROADMAP.MD`](./ROADMAP.MD)，本文档专注于"存储模型"。

---

## 0. 总体架构

```
                ┌── Service / Worker / HTTP / Pipeline ──┐
                │  capability_capsule                     │
                │  transcript                             │
                │  entity                                 │
                └─────────────────┬───────────────────────┘
                                  │ Arc<dyn Backend>      ← Phase 5 umbrella supertrait
                                  │ （9 个 sub-trait 聚合：CapsuleStore / CapsuleSearchStore /
                                  │  EmbeddingJobStore / EmbeddingVectorStore / GraphStore /
                                  │  TranscriptStore / EntityRegistry / SessionStore /
                                  │  MaintenanceStore）
                       ┌──────────▼──────────┐
                       │       Store         │   ← 当前唯一的 Backend 实现
                       │ (blanket impl       │      （pub —— 是给 storage 外部用的句柄）
                       │  Backend for Store) │
                       │ commit_lance_write()│
                       │ 写后刷新 DuckDB     │
                       └──────┬───────┬──────┘
                              │       │
                       写 ←───┘       └─→ 读
                       ▼                  ▼
          ┌──────────────────┐    ┌──────────────────┐
          │  LanceStore      │    │  DuckDbQuery     │
          │ (pub(crate)，    │    │ (pub(crate)，    │
          │  实现细节，外部  │    │  实现细节，外部  │
          │  代码不可直接拿) │    │  代码不可直接拿) │
          └────────┬─────────┘    └────────┬─────────┘
                   │                       │
                   └────── ATTACH ─────────┘
                              ▼
                     同一个 lance 数据集目录
                     （12 张表共存于 .lance/ 下）
```

**核心约束**：

- LanceDB 是**单写者**。所有写入必须经过 `LanceStore`；同一进程多个 `Store` 句柄共享 `Arc<LanceStore>`，不共享 `Connection`（`open_in_memory` swap 解决 snapshot 缓存）。
- DuckDB 端通过 `lance` 扩展 ATTACH 同一目录，承担**绝大多数读路径**。`Store::commit_lance_write` 在每次写入后 swap 一个新的 in-memory DuckDB 连接（Phase 1 QW-6 把 `lance_write_then_refresh!` 宏改成了显式 method）。
- **Phase 5 之后**，service / worker / pipeline 持 `Arc<dyn Backend>`（umbrella trait），不再持 `Arc<Store>`。`LanceStore` / `DuckDbQuery` 都收窄成 `pub(crate)`——外部代码物理上无法直接拿到具体实现的句柄，必须经 `Backend` 或 9 个 sub-trait 的某一个。`Store` 自身仍是 `pub`，因为 `app.rs::from_config` 需要 `Store::open` + 一次 `set_transcript_job_provider`（Lance-only 配置），然后立刻 upcast 成 `Arc<dyn Backend>`。
- **未来要换 backend**（如 Postgres）：再写一个 struct 把 9 个 sub-trait 实现一遍，blanket impl 自动让它满足 `Backend`，`app.rs` 改一个 binding 就完成切换——这是 Phase 5 抽 trait 的最终交付物。

---

## 1. 表清单（按业务域分组）

> 入口列出的是 trait 方法。Service / worker 持 `Arc<dyn Backend>`，自动派发到 `Store`（当前唯一的 backend impl）；表名后括号注的是承载这些方法的 sub-trait（Phase 3 抽出，Phase 5 凝成 `Backend` umbrella）。

| 业务域 | 表 (sub-trait) | 主要写入入口 | 主要读取入口 |
|---|---|---|---|
| **能力胶囊** | `capability_capsules` (`CapsuleStore` + `CapsuleSearchStore`) | `CapsuleStore::insert_capability_capsule[s]` | `CapsuleSearchStore::hybrid_candidates` / `search_candidates` |
| | `capability_capsule_embeddings` (`EmbeddingVectorStore`) | `EmbeddingVectorStore::upsert_capability_capsule_embedding` | `CapsuleSearchStore::hybrid_candidates`（JOIN） |
| | `embedding_jobs` (`EmbeddingJobStore`) | `EmbeddingJobStore::try_enqueue_embedding_job` / `enqueue_embedding_jobs` | `EmbeddingJobStore::claim_next_n_embedding_jobs`（worker） |
| **会话 / 经验** | `sessions` (`SessionStore`) | `SessionStore::open_session` / `touch_session` / `close_session` | `SessionStore::latest_active_session` |
| | `episodes` (`SessionStore`) | `SessionStore::insert_episode` | `SessionStore::list_successful_episodes_for_tenant` |
| | `feedback_events` (`CapsuleStore`) | `CapsuleStore::apply_feedback` | `CapsuleStore::feedback_summary` |
| **对话归档** | `conversation_messages` (`TranscriptStore`) | `TranscriptStore::create_conversation_message[s]` | `TranscriptStore::bm25_transcript_candidates` / `semantic_search_transcripts` |
| | `conversation_message_embeddings` (`EmbeddingVectorStore`) | `EmbeddingVectorStore::upsert_conversation_message_embedding` | `TranscriptStore::semantic_search_transcripts`（JOIN） |
| | `transcript_embedding_jobs` (`EmbeddingJobStore`) | LanceStore 内部（`create_conversation_messages_batch` 触发） | `EmbeddingJobStore::claim_next_n_transcript_embedding_jobs` |
| **知识图谱** | `graph_edges` (`GraphStore`) | `GraphStore::sync_memory_edges` / `close_edges_for_capability_capsule` | `GraphStore::neighbors` / `related_capability_capsule_ids` |
| **实体注册表** | `entities` (`EntityRegistry`) | `EntityRegistry::resolve_or_create` | `EntityRegistry::get_entity` / `list_entities` |
| | `entity_aliases` (`EntityRegistry`) | `EntityRegistry::add_alias` / `resolve_or_create`（隐式） | `EntityRegistry::lookup_alias`（含在 `get_entity`） |

---

## 2. 表卡片（schema + 调用关系）

每张卡片格式：① schema 字段；② 写入方法（Lance）；③ 读取方法（DuckDB / Lance native）；④ Store 包装；⑤ Service / Pipeline 调用方；⑥ 跨表依赖。

### 2.1 `capability_capsules`

胶囊主表。verbatim `content` 是事实源，`summary` 仅用于检索/索引。

**Schema** (`fn capability_capsules_schema`, `mod.rs:258`)

| 字段 | 类型 | nullable | 备注 |
|---|---|---|---|
| capability_capsule_id | Utf8 | no | PK，UUIDv7 前缀 `mem_` |
| tenant | Utf8 | no | |
| capability_capsule_type | Utf8 | no | enum: implementation / experience / preference / episode / workflow |
| status | Utf8 | no | enum: provisional / pending_confirmation / active / archived / rejected |
| scope | Utf8 | no | enum: global / project / repo / workspace |
| visibility | Utf8 | no | enum: private / shared / system |
| version | Int64 | no | 版本链当前版本号（Phase 5 pain #1 从 UInt64 改 Int64，Postgres BIGINT 直接 bind 不需要 try_from） |
| summary | Utf8 | no | 索引/提示用，不作答案来源 |
| content | Utf8 | no | **verbatim 事实源**，存档不改写 |
| evidence | List\<Utf8\> | no | 证据链 / 引用 |
| code_refs | List\<Utf8\> | no | 代码引用 (`path:line`) |
| project / repo / module / task_type | Utf8 | yes | 范围标签 |
| tags | List\<Utf8\> | no | |
| topics | List\<Utf8\> | no | 知识图谱节点别名（解析进 `entities`） |
| confidence / decay_score | Float32 | no | 反馈循环驱动 |
| content_hash | Utf8 | no | sha256，去重用 |
| idempotency_key | Utf8 | yes | 客户端去重 |
| session_id | Utf8 | yes | FK → `sessions.session_id` |
| supersedes_capability_capsule_id | Utf8 | yes | 版本链上一版本 |
| source_agent | Utf8 | no | claude-code / codex / cli |
| created_at / updated_at | Utf8 | no | 20-digit 毫秒字符串 |
| last_validated_at | Utf8 | yes | feedback `useful` 时填 |

**写入** (`lance_store/capability_capsules.rs`)

| 方法 | 用途 |
|---|---|
| `insert_capability_capsule` | 单行插入 |
| `insert_capability_capsules_batch` | 批量插入（`/capability_capsules/batch`） |
| `update_status` | 状态机迁移（accept/reject 共用） |
| `accept_pending` / `reject_pending` | pending_confirmation → active / rejected |
| `replace_pending_with_successor` | 新版本替换待确认行 |
| `delete_capability_capsule_hard` | 硬删除（admin Web） |
| `apply_feedback` | 写 `feedback_events` + 调整本表 confidence/decay/status |

**读取** (`duckdb_query/capability_capsules.rs`)

| 方法 | 用途 |
|---|---|
| `hybrid_candidates` | BM25 + 向量 RRF 融合（核心 search） |
| `search_candidates` | 纯 status='active' 列表，喂给 lifecycle pool |
| `recent_active_capability_capsules` | wake-up fast path |
| `list_capability_capsules_for_tenant` | admin Web |
| `get_capability_capsule_for_tenant` | 详情页 |
| `get_pending` | review UI |
| `find_by_idempotency_or_hash` | ingest 去重探针 |
| `list_pending_review` | review queue |
| `fetch_capability_capsules_by_ids` | 排序后回填详情 |
| `list_capability_capsule_ids_for_tenant` | admin 删除前确认 |
| `list_capability_capsule_versions_for_tenant` | 版本链回放 |

**Service 调用** — 全部走 `CapabilityCapsuleService`（`service/capability_capsule_service.rs`）。

**跨表依赖**

- 1:N → `capability_capsule_embeddings`（id 对齐，soft delete via `delete_*_embedding`）
- 1:N → `embedding_jobs`（id 对齐，硬删除时级联清理）
- 1:N → `feedback_events`（id 对齐，软累积）
- N:1 → `sessions`（`session_id` FK）
- 自引用 → `supersedes_capability_capsule_id`
- 1:N → `graph_edges` 作为 `from_node_id`（`capability_capsule:<id>`）

---

### 2.2 `capability_capsule_embeddings`

胶囊向量表，`embedding_dim` 启动时确定后凝固（`ensure_capability_capsule_embeddings_table` 懒建）。

**Schema** (`fn capability_capsule_embeddings_schema`, `mod.rs:504`)

| 字段 | 类型 | nullable |
|---|---|---|
| capability_capsule_id | Utf8 | no |
| tenant | Utf8 | no |
| embedding_model | Utf8 | no |
| embedding_dim | Int64 | no |
| embedding | FixedSizeList\<Float32\>[dim] | no |
| content_hash | Utf8 | no |
| source_updated_at / created_at / updated_at | Utf8 | no |

**写入** (`lance_store/capability_capsules.rs`)

- `upsert_capability_capsule_embedding` — delete-then-insert（Lance 无 PK 约束）
- `delete_capability_capsule_embedding` — 硬删除时调

**读取** — 不直接读，通过 `hybrid_candidates` SQL `lance_vector_search()` 表函数 JOIN `capability_capsules`。

**Service 调用** — `service/embedding_helpers.rs::store_capability_capsule_embedding`（embedding worker） + `delete_capability_capsule_hard`（清理）。

**跨表依赖** — 1:1 with `capability_capsules`（`capability_capsule_id` FK）。

---

### 2.3 `embedding_jobs`

胶囊向量化的持久化队列。worker 通过乐观 UPDATE 抢 row。

**Schema** (`fn embedding_jobs_schema`, `mod.rs:698`)

| 字段 | 类型 | nullable | 备注 |
|---|---|---|---|
| job_id | Utf8 | no | UUIDv7 前缀 `ej_` |
| tenant | Utf8 | no | |
| capability_capsule_id | Utf8 | no | FK |
| target_content_hash | Utf8 | no | 入队时锁定的 hash |
| provider | Utf8 | no | 与 worker 的 `job_provider_id()` 对齐 |
| status | Utf8 | no | pending / processing / completed / failed / stale |
| attempt_count | Int64 | no | |
| last_error | Utf8 | yes | |
| available_at | Utf8 | no | 调度时间戳 |
| created_at / updated_at | Utf8 | no | |

**写入** (`lance_store/capability_capsules.rs`)

| 方法 | 用途 |
|---|---|
| `try_enqueue_embedding_job` | 单行入队（带 `(tenant,capsule_id,hash,provider)` 活态去重） |
| `enqueue_embedding_jobs_batch` | 批量入队（无 dedup，调用方保证 fresh） |
| `claim_next_n_embedding_jobs` | 乐观 UPDATE pending→processing |
| `complete_embedding_job` | 成功 |
| `mark_embedding_job_stale` | 内容已变 |
| `reschedule_embedding_job_failure` | 暂时失败、退避重试 |
| `permanently_fail_embedding_job` | 重试上限 |
| `delete_embedding_jobs_by_capability_capsule_id` | 硬删胶囊时级联 |

**读取**

- DuckDB: `get_embedding_job_status`（`duckdb_query/embedding_jobs.rs`）— 查单 job 状态
- LanceStore native: `list_embedding_jobs` / `latest_embedding_job_status_for_hash` / `stale_live_embedding_jobs_for_capability_capsule`（admin Web + ingest hot path）

**Service 调用**

- `CapabilityCapsuleService::ingest_batch` — 批量 enqueue
- `service/embedding_worker.rs::tick` — claim → complete / reschedule / permanently_fail
- `delete_capability_capsule_hard` — 级联删除

**跨表依赖** — N:1 with `capability_capsules`，按 id 级联硬删。

---

### 2.4 `sessions`

会话生命周期（agent 启动 → 闲置/退出）。

**Schema** (`fn sessions_schema`, `mod.rs:380`)

| 字段 | 类型 | nullable |
|---|---|---|
| session_id | Utf8 | no |
| tenant | Utf8 | no |
| caller_agent | Utf8 | no |
| started_at / last_seen_at | Utf8 | no |
| ended_at | Utf8 | yes |
| goal | Utf8 | yes |
| memory_count | UInt32 | no |

**写入** (`lance_store/sessions.rs`)：`open_session` / `touch_session` / `close_session`。

**读取** — `latest_active_session`（LanceStore native，按 `tenant + caller_agent` 找未结束行）。

**Service 调用** — `pipeline/session.rs::resolve_session`（ingest 时触发）。

**跨表依赖** — 1:N with `capability_capsules`（`session_id` 为 FK，nullable）。

---

### 2.5 `episodes`

完整经验记录。`workflow_candidate` 是 JSON-encoded 提取候选。

**Schema** (`fn episodes_schema`, `mod.rs:394`) — 详见 mod.rs；字段同 capsule 但加 `goal/steps/outcome/workflow_candidate`。

**写入** — `insert_episode`。

**读取** — `list_successful_episodes_for_tenant`（LanceStore native，过滤 `outcome='success'`）。

**Service 调用**

- `CapabilityCapsuleService::ingest_episode` — 写入 + 触发 `workflow::maybe_extract_workflow`

**跨表依赖** — 弱关联：`workflow_candidate.capability_capsule_id` 指回胶囊（提取出 workflow 后回填）。

---

### 2.6 `feedback_events`

胶囊反馈事件流。`apply_feedback` 同时写本表 + 改 `capability_capsules.confidence/decay_score/status`。

**Schema** (`fn feedback_events_schema`, `mod.rs:437`)

| 字段 | 类型 | nullable | 备注 |
|---|---|---|---|
| feedback_id | Utf8 | no | PK，`fb_` UUIDv7 |
| capability_capsule_id | Utf8 | no | FK |
| feedback_kind | Utf8 | no | 6 种 `FeedbackKind`（含 Phase 1 加入的 `auto_promoted`） |
| created_at | Utf8 | no | |
| note | Utf8 | yes | 调用方提供的自由文本注释；不参与排序，仅审计 |

**写入** — `apply_feedback`（`note` 由 `Service::submit_feedback(.., note)` 透传过来；HTTP `/capability_capsules/feedback` body 接 `note?`，MCP 工具 `capability_capsule_apply_feedback` 也带 forward）。

**读取** — `list_feedback_for_memory` + `feedback_summary`（LanceStore native，被详情页用）。

**Service 调用**

- `CapabilityCapsuleService::submit_feedback` — 调 `apply_feedback`
- `CapabilityCapsuleService::get_capability_capsule_detail` — 拼 `feedback_summary`

**跨表依赖** — N:1 with `capability_capsules`，软累积、不级联删（保留审计）。

---

### 2.7 `conversation_messages`

verbatim 对话归档，零业务规则。

**Schema** (`fn conversation_messages_schema`, `mod.rs:1130`)

| 字段 | 类型 | nullable | 备注 |
|---|---|---|---|
| message_block_id | Utf8 | no | UUIDv7 前缀 `blk_` |
| session_id | Utf8 | yes | |
| tenant | Utf8 | no | |
| caller_agent | Utf8 | no | |
| transcript_path | Utf8 | no | dedup 三元组之一 |
| line_number | UInt64 | no | dedup 三元组之一 |
| block_index | UInt32 | no | dedup 三元组之一 |
| message_uuid | Utf8 | yes | Claude Code envelope uuid |
| role | Utf8 | no | user / assistant / system |
| block_type | Utf8 | no | text / tool_use / tool_result / thinking |
| content | Utf8 | no | text 直存；tool_use/result 可能是 JSON-encoded |
| tool_name / tool_use_id | Utf8 | yes | |
| embed_eligible | Boolean | no | true → 触发 `transcript_embedding_jobs` 入队 |
| created_at | Utf8 | no | ISO-8601 |
| meta_json | Utf8 | yes | envelope 元数据 (cwd / git_branch / parent_uuid / is_error)，JSON-encoded |

**写入** (`lance_store/transcripts.rs`)：

- `create_conversation_message` — 单行 + count_rows dedup
- `create_conversation_messages_batch` — 批量，一次 filter 拉所有现有 key 做内存 dedup + 一次 multi-row add + 一次 multi-row 入队 `transcript_embedding_jobs`

**读取** (`duckdb_query/transcripts.rs`)：

| 方法 | 用途 |
|---|---|
| `bm25_transcript_candidates` | 词法召回 |
| `semantic_search_transcripts` | 向量召回（JOIN embeddings） |
| `recent_conversation_messages` | wake-up fast path / 空 query 浏览 |
| `get_conversation_messages_by_session[_paged]` | session 全量回放 |
| `list_transcript_sessions` | admin / wake-up |
| `fetch_conversation_messages_by_ids` | 排序后回填 |
| `context_window_for_block` | search 命中后取 ±N 上下文 |
| `anchor_session_candidates` | 锚定 session 加权召回 |

**Service 调用** — `TranscriptService::ingest[_batch]` / `search` / `get_by_session[_paged]` / `recent_for_wake_up`。

**跨表依赖**

- 1:1 with `conversation_message_embeddings`（embed_eligible 时）
- 1:1 with `transcript_embedding_jobs`（同上）
- 弱关联 → `sessions.session_id`（不强制 FK，session_id 可缺失）

---

### 2.8 `conversation_message_embeddings`

镜像 `capability_capsule_embeddings` 的 schema，差别在 PK 改成 `message_block_id`。详细字段见 `mod.rs:602`。写读路径同 `2.2` 但走 `transcripts.rs`。

---

### 2.9 `transcript_embedding_jobs`

镜像 `embedding_jobs`，去掉 `target_content_hash`（block 不可变，行 id 即 hash）。详见 `mod.rs:837`。Worker 在 `service/transcript_embedding_worker.rs`。

---

### 2.10 `graph_edges`

知识图谱边表，**双时态**（`valid_from` / `valid_to`）。`valid_to IS NULL` 即活态。

**Schema** (`fn graph_edges_schema`, `mod.rs:952`)

| 字段 | 类型 | nullable |
|---|---|---|
| from_node_id | Utf8 | no |
| to_node_id | Utf8 | no |
| relation | Utf8 | no |
| valid_from | Utf8 | no |
| valid_to | Utf8 | yes |

**节点 id 命名约定**

- `capability_capsule:<uuid>` — 胶囊节点（`memory_node_id` 工具函数）
- `entity:<uuid>` — 实体节点（解析自 `topics`）

**写入** (`lance_store/graph.rs`)：

- `sync_memory_edges` — UPSERT 活态边（先关闭旧的 `valid_to`，再插新的）
- `close_edges_for_capability_capsule` — 硬删胶囊时关闭关联边

**读取** (`duckdb_query/graph.rs`)：

- `neighbors` — 时间点查询（默认 now，过滤 `valid_to IS NULL`）
- `related_capability_capsule_ids` — 多跳邻居展开 + 回到胶囊节点

**Service 调用**

- `CapabilityCapsuleService::ingest[_batch]` — 写
- `CapabilityCapsuleService::get_graph_neighbors` — 读
- `pipeline/retrieve.rs::rank_with_hybrid_and_graph` — 读（ranking 加分）

**跨表依赖** — `from_node_id` / `to_node_id` 是字符串别名，松耦合 `capability_capsules.capability_capsule_id` / `entities.entity_id`。

---

### 2.11 `entities`

实体注册表。规整化 `topics` 中的字符串到稳定 UUID。

**Schema** (`fn entities_schema`, `mod.rs:1024`)

| 字段 | 类型 | nullable |
|---|---|---|
| entity_id | Utf8 | no |
| tenant | Utf8 | no |
| canonical_name | Utf8 | no |
| kind | Utf8 | no |
| created_at | Utf8 | no |

**写入** — `resolve_or_create`（先查 alias，找到则返；否则插实体 + 插 alias）。

**读取** — `get_entity` / `list_entities`（DuckDB SQL，JOIN aliases）。

**Service 调用** — `EntityService` + `service/capability_capsule_service.rs::resolve_drafts_to_edges`（ingest 时把 topics 字符串解析成 entity_id）。

**跨表依赖** — 1:N with `entity_aliases`；被 `graph_edges.to_node_id` 引用。

---

### 2.12 `entity_aliases`

`(tenant, alias_text)` 复合 PK，规整化（lowercase + whitespace-collapse）。

**Schema** (`fn entity_aliases_schema`, `mod.rs:1092`)：4 列（tenant / alias_text / entity_id / created_at）。

**写入** — `add_alias` / `resolve_or_create`（隐式）。
**读取** — `lookup_alias` + `get_entity` 的 JOIN。
**Service 调用** — `EntityService::add_alias`。

---

## 3. 跨表依赖图

```
                                 ┌─ supersedes ─┐
                                 │              │
                                 ▼              │
       ┌───────────────────► capability_capsules ◄──────────────┐
       │                          │   ▲   │                     │
       │  session_id              │   │   │                     │
       │  (FK, nullable)          │   │   │                     │
       │                          │   │   │                     │
       │                  insert  │   │   │ apply_feedback      │
   sessions ─────┐               ▼   │   ▼  (events stream)     │
                 │      capability_capsule_embeddings            │
                 │      embedding_jobs ─────────► (worker)       │
                 │      feedback_events                          │
                 │                                               │
                 │              from_node_id (capability_capsule:<id>)
                 │                  ▲                            │
                 │                  │                            │
                 │              graph_edges                      │
                 │                  │                            │
                 │                  │ to_node_id (entity:<id>)   │
                 │                  ▼                            │
                 │              entities ◄─────── entity_aliases │
                 │                                               │
                 ▼                                               │
       conversation_messages                                     │
              │                                                  │
              ├─► conversation_message_embeddings (embed_eligible) 
              ├─► transcript_embedding_jobs (worker)             │
              │                                                  │
       (session_id 弱关联 sessions)                              │
                                                                 │
       episodes ──── workflow_candidate.capability_capsule_id ──┘
```

**强依赖（硬删除时级联）**

- `capability_capsules` 删除 → `capability_capsule_embeddings` + `embedding_jobs` + `graph_edges` 关闭
- `feedback_events` 不级联（保留审计）

**弱依赖（字符串别名）**

- `graph_edges.from_node_id` / `to_node_id` — 不是 FK，是 `<kind>:<uuid>` 字符串约定
- `conversation_messages.session_id` — 可缺失

---

## 4. 命名约定 / 生命周期

### 4.1 ID 前缀

| 前缀 | 含义 | 示例 |
|---|---|---|
| `mem_` | 胶囊 id | `mem_019e0054-6c48-...` |
| `ep_` | 经验 id | `ep_019e...` |
| `ej_` | 胶囊 embedding 任务 id | `ej_019e...` |
| `fb_` | 反馈事件 id | `fb_019e...` |
| `blk_` | 对话块 id | `blk_019e...` |
| 无前缀 UUID | session_id / entity_id / job_id（transcript） | |

所有 UUID 都是 v7（时间排序友好）。

### 4.2 时间戳

- `created_at` / `updated_at` / `last_validated_at`：20-digit 毫秒字符串（`current_timestamp()`），lexically sortable。
- `valid_from` / `valid_to`（graph_edges）：同上。
- `available_at`（embedding_jobs）：同上，调度比较用 `<=`。
- `conversation_messages.created_at`：ISO-8601 RFC-3339（来自 Claude Code transcript 原文）。

### 4.3 状态机

**capsule status** (`MemoryStatus`)

```
        provisional (write_mode=propose 默认)
            │
            ▼
       pending_confirmation (review queue)
            │
   ┌────────┼─────────┐
   │        │         │
   ▼        ▼         ▼
 active   archived  rejected
            ▲
            │
       feedback_kind=incorrect
```

**embedding_jobs status**

```
   pending → processing → completed
                 │
                 ├→ failed (attempt_count++, 退避重试)
                 │       └→ pending (when available_at 到期)
                 └→ stale (内容已变)
```

`transcript_embedding_jobs` 状态机相同。

### 4.4 反馈影响（6 种 `FeedbackKind`）

| kind | confidence Δ | decay Δ | side effect |
|---|---|---|---|
| useful | +0.10 | 0 | last_validated_at = now |
| applies_here | +0.05 | 0 | — |
| outdated | 0 | +0.20 | — |
| does_not_apply_here | 0 | +0.10 | — |
| incorrect | 0 | 0 | status → archived |
| auto_promoted | 0 | 0 | status → active（由 `worker/auto_promote_worker` 在长期 idle pending 上写入） |

详细见 `domain/capability_capsule.rs::FeedbackKind`。`FeedbackSummary` 6 个槽位（`total / useful / outdated / incorrect / applies_here / does_not_apply_here / auto_promoted`），每个 backend 的聚合器都要覆盖所有 kind——Phase 5 pain #5 修复过 `auto_promoted` 落入 catch-all 的静默 bug。

---

## 5. Schema 演进

LanceDB 没有外置 migration 文件 —— schema 直接用 `Schema::new(vec![Field::new(...)])` 在 `lance_store/mod.rs` 内联声明。

**加列**：

1. `*_schema()` 加 `Field::new(...)`；nullable 字段更安全（旧行无值）
2. `*_to_record_batch()` builder 同步加 builder + `append_value` / `append_null`
3. `record_batch_to_*()` parser 同步加 col 解析；**defensive 读** —— `column_by_name` + `as_any().downcast_ref()`，pre-existing 行没有该列时返回 None
4. domain 类型加字段 + `#[serde(default, skip_serializing_if = "Option::is_none")]`

**删列**：先把代码里的所有引用全删 + 在新插入时 `append_null`；旧行的列空间是浪费但不会错。需要真彻底删除时只能整表重写一遍（目前没有内置 CLI，直接 Lance API `Dataset::add_columns` / `drop_columns` 走脚本）。

**改类型**：不能原地改。需要：① 加新列 ② 双写一段时间 ③ 切读到新列 ④ 后续整表重建去掉旧列。**例外**：纯整数宽度切换（如 Phase 5 pain #1 `version: UInt64 → Int64`）实际上是 breaking schema change——本仓选择直接改 schema + 接受现有 dev DB 需要重建的代价（local-first 工具的合理取舍）。换 prod 部署得走双写迁移。

**Postgres 端的 schema 演进**：Phase 4 的 Postgres spike 走传统 SQL migration（见 `migrations/postgres/0001_capsule_store.sql`，CREATE TABLE + indexes + CHECK 约束），sqlx 没有内置 migration runner——目前手动 `psql -f` 应用。如果 Phase 5+ 真的部署 Postgres，得引入 sqlx-migrate 或类似工具。

参考已落地的 schema 演进案例：

- `capability_capsules.topics` —— Task 9 加列
- `capability_capsules.supersedes_capability_capsule_id` —— Task 11 加列
- `conversation_messages.meta_json` —— Task 23 加列（envelope 元数据 + tool_result is_error）
- `entities` / `entity_aliases` —— 整个表族新增
- `capability_capsules.version` —— Phase 5 改类型 `UInt64 → Int64`（pain #1，对齐 Postgres BIGINT）
- `FeedbackSummary.auto_promoted` —— Phase 5 加 domain struct 字段（pain #5，serde-default 反序列化为 0 保持向后兼容）

---

## 6. 维护命令

- **`POST /admin/vacuum`** —— Phase 1 加的 Lance manifest prune endpoint，body 可选 `{older_than_days}`（默认走 `MEM_VACUUM_OLDER_THAN_DAYS` 配置）。返回 `VacuumStats { bytes_removed, old_versions_removed, tables_pruned, tables_skipped }`。背景里 `worker/vacuum_worker` 每天扫一次同样的 logic。
- **`mem mine`** —— 一次性脚本，把 Claude Code transcript 喂给 `mem serve` 做双 sink（capsules + transcript archive）。
- **`mem wake-up` / `mem feedback`** —— 同样是一次性 CLI。

旧的 `mem repair` 子命令已经删掉——HNSW sidecar 已经被 LanceDB 0.27 native ANN 替换（Phase 1 QW-4 清理过）；其他 schema 级运维（重建列、迁移历史 edge 等）仍然走一次性脚本。
