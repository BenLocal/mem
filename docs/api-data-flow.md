# mem HTTP API — 数据流转 / 技术栈 / 优化方向

> **架构（route-B，2026-06-24）**：存储与读取都是 **lance-native**（`LanceStore` + lancedb Rust API）——没有 DuckDB（读引擎与 `duckdb` crate 已删）、没有 `ATTACH`、没有 in-process 连接。BM25 走 in-RAM **Tantivy** 子系统（`src/storage/fts.rs`，jieba 分词，启动全量重建，非 lance 索引）；向量 ANN 走 lance 原生 `query().nearest_to()`（IVF_PQ 索引由 vacuum worker 建，无 sidecar）；hybrid 召回在 **Rust 侧 RRF**（`pipeline::ranking::rrf_merge`）融合后过 lifecycle 评分栈；decay/last-used 经 lancedb `table.update()` 同一写者，`commit_lance_write` 已是 no-op（读经 `read_consistency_interval(0)` Strong 直接看到写）。默认数据目录名 `mem.lance`。详见 `AGENTS.md` 架构段、`docs/remove-duckdb-keep-lance.md`、`docs/backend-coupling.md`。
>
> **目的**：给一个真正能用上的"接口地图"——每条 HTTP 路由对应哪条服务/管线/存储调用链、用到的技术、可优化点。半年后回头改任何一条接口前先读这个。
>
> **同步源**：本文应与 `src/http/*.rs` + `src/service/` + `src/pipeline/` + `src/storage/` 实际代码绑定；改架构时**先改代码、再来更新本文档**，commit 引用本文件章节号（`docs(api-flow): … (closes api-flow §X)`）。
>
> 设计原则与 mempalace 借鉴见 [`mempalace-diff.md`](./mempalace-diff.md) 与 [`mempalace-diff-v2.md`](./mempalace-diff-v2.md)，存储层耦合分析见 [`backend-coupling.md`](./backend-coupling.md)，路线图见 [`ROADMAP.MD`](./ROADMAP.MD)，本文档专注于"运行时"。

---

## 0. 总体架构

```
          ┌─────────────────── HTTP (axum :3000, src/http/*) ───────────────────┐
caller    │ /capability_capsules[ /search /batch /list /{id} /stats /taxonomy ]  │
(codex/   ─┼ /capability_capsules/feedback  /episodes  /reviews/*  /entities/*    │
 cursor/   │ /graph/*  /fact_check  /transcripts[ /messages /search /range ]      │
 CLI/mcp)  │ /embeddings/*  /mine/cursors  /admin/*  /health                      │
          └──────┬───────────────────────────────────────────────┬───────────────┘
                 │                                                │
       ┌─────────▼──────────────┐                     ┌───────────▼────────────┐
       │ CapabilityCapsuleService│  (service = 编排,    │ TranscriptService       │
       │ EntityService / FactCheck│   无业务规则)        │ (verbatim 对话归档)     │
       └─────────┬──────────────┘                     └───────────┬────────────┘
                 │                                                │
       ┌─────────▼──────────── pipeline (src/pipeline/*) ─────────▼────────────┐
       │ ingest(status·hash·graph-draft) · retrieve(RRF + lifecycle + graph)   │
       │ ranking(rrf_merge) · compress(token budget→4段) · transcript_recall   │
       └─────────┬────────────────────────────────────────────────────────────┘
                 │  读: query().only_if(<谓词>) · query().nearest_to(vec) · fetch_*_by_ids
                 │  写: open_table().add() / .update().only_if() / .delete()
       ┌─────────▼──────── storage = LanceStore (src/storage/lance_store/*) ────────┐
       │ on-disk Lance 数据集 @ MEM_DB_PATH (默认 mem.lance) — 读写都走 lancedb Rust API │
       │  ├ capability_capsules / capability_capsule_embeddings / embedding_jobs      │
       │  ├ feedback_events / sessions / episodes / mine_cursors / evolution_candidates│
       │  ├ graph_edges (Rust BFS) · entities / entity_aliases (EntityRegistry)       │
       │  └ conversation_messages / conversation_message_embeddings / transcript_embedding_jobs│
       │  词法 BM25: in-RAM Tantivy (src/storage/fts.rs, jieba, 启动全量重建)          │
       │  向量 ANN: lance 原生 nearest_to + IVF_PQ 索引 (vacuum worker 建, 无 sidecar) │
       └─────────┬──────────────────────────────────────────────────────────────────┘
                 │
   ┌─────────────▼──────────── 后台 worker (tokio::spawn, src/worker/*) ─────────────┐
   │ embedding_worker / transcript_embedding_worker → 消费 *_embedding_jobs 队列写向量 │
   │ vacuum_worker → manifest 修剪 + ensure_query_indexes(IVF_PQ) + Tantivy 重建      │
   │ decay_worker → 时间衰减   last_used_worker → 检索强化 (last_used_at)             │
   │ auto_promote(默认ON) · idle_archive / dedup / evolution / cooccurrence /          │
   │ potentiation / topic_tunnel (多数 opt-in, 默认 OFF)                              │
   └──────────────────────────────────────────────────────────────────────────────┘
```

**核心约束**（值得被反复念）：
- **读写都是 lance-native，无 DuckDB / 无 SQL 引擎**：写 `open_table().add()/.update().only_if()/.delete()`，读 `query().only_if(<sql 谓词>)` / `nearest_to(vec)` / `fetch_*_by_ids`；排序、聚合、RRF、图-BFS、版本链去重都在 **Rust 侧**做。读连接用 `read_consistency_interval(0)`（Strong），写后即可见，`commit_lance_write` 是 no-op pass-through。
- **召回两路在 Rust 侧融合**：BM25（Tantivy）+ 向量 ANN（lance `nearest_to`）各取候选 id，`pipeline::ranking::rrf_merge`（k=60）融合，再过 lifecycle 加性评分栈（intent×type / scope / confidence / freshness − decay + graph_boost）。当前两路**顺序**取候选（优化机会见 §3.6 / §4.2）。
- `capability_capsules.content` 是 verbatim 事实源，`summary` 是索引提示——**输出层永远基于 content**（见 mempalace-diff §7）。

---

## 1. 技术栈映射

| 层 | 技术 / Crate | 在哪里 |
|---|---|---|
| HTTP | **axum 0.x** + **tokio** + tower middleware | `src/http/*.rs`, `src/main.rs` |
| 序列化 | serde (snake_case) | `src/domain/*.rs` |
| 错误 | `thiserror` + 自家 `AppError` (HTTP status mapping) | `src/error.rs` |
| 存储 | **Lance** (列式，copy-on-write)，读写都经 **lancedb Rust API**（无 DuckDB、无 `ATTACH`、无 in-process 连接） | `src/storage/lance_store/` + `src/storage/store.rs` |
| ANN | **Lance 原生向量检索**（`query().nearest_to()`，IVF_PQ 索引由 vacuum worker 建，无外部 sidecar） | `src/storage/lance_store/maintenance.rs`（索引）+ `capability_capsules.rs` / `transcripts.rs`（查询） |
| 词法 | **in-RAM Tantivy BM25**（jieba 分词，启动全量重建，非 lance 索引） | `src/storage/fts.rs` |
| 排序 | **RRF**（k=60，×1000 scale）+ **lifecycle 加性层**（intent×memory_type / scope / freshness / decay / graph_boost） | `src/pipeline/retrieve.rs::score_candidates_hybrid_rrf` + `apply_lifecycle_score` |
| Tokenizer | **tiktoken-rs**（`o200k_base`，CJK 友好）用于压缩输出 | `src/pipeline/compress.rs` |
| 哈希 | **sha2** SHA-256（content_hash 跨进程稳定，迁移自 SipHash） | `src/pipeline/ingest.rs::compute_content_hash` |
| Embedding | **embed_anything** (本地 Qwen3, dim=1024 默认) / **OpenAI** BYOK / **fake** (测试) — `Arc<dyn EmbeddingProvider>` | `src/embedding/` |
| Graph | Lance 表 `graph_edges`（valid_from / valid_to 时序，Rust BFS），mempalace-diff §5 | `src/storage/lance_store/graph.rs` |
| Entity registry | Lance 表 `entities` + `entity_aliases`（PK = `(tenant, alias_text)`，normalize lower+ws-collapsed） | `src/storage/lance_store/entities.rs` |
| Async workers | tokio::spawn × 3：embedding / transcript-embedding / decay | `src/service/embedding_worker.rs` 等 |
| 观测 | `tracing` (env filter via `RUST_LOG`) | 全局 |

---

## 2. 接口逐条数据流转

约定（每条都用同一个图式）：

```
HTTP handler → service 方法 → pipeline / storage 调用链 → 写入哪些表 / 读哪些索引
```

### 2.1 Capsule 生命周期

#### `POST /capability_capsules` — 写一条 capsule

```
capability_capsule.rs::ingest_capability_capsule
 → CapabilityCapsuleService::ingest
   → pipeline::ingest::compute_content_hash (SHA-256)
   → Store::find_by_idempotency_or_hash                    # 幂等 / 去重 → 命中直接返回
   → pipeline::ingest::initial_status                      # capsule_type + write_mode → status
   → pipeline::ingest::{validate_verbatim, validate_scope_boundary, assess_ingest_quality}
   → pipeline::session::resolve_session                    ← 读/写 sessions
   → Store::insert_capability_capsule                      ← 写 capability_capsules
   → pipeline::ingest::extract_graph_edge_drafts
   → service::resolve_drafts_to_edges                      # 含 EntityRegistry::resolve_or_create → entities/entity_aliases
   → Store::sync_memory_edges                              ← 写 graph_edges
   → Store::enqueue_embedding_job_for_memory               ← 写 embedding_jobs（pending）
   → Store::touch_session                                  ← 写 sessions
```

**写入**：`capability_capsules` / `graph_edges` / `entities` / `entity_aliases` / `embedding_jobs` / `sessions`。
**特点**：embed 异步——HTTP **不等** embedding 完成，job 入队就返回 201。

#### `POST /capability_capsules/search` — 检索（核心读路径）

```
capability_capsule.rs::search_capability_capsule
 → CapabilityCapsuleService::search
   ├─ wake-up 快路（intent="wake_up" + 空 query）：
   │     Store::recent_active_capability_capsules → compress::compress   # 跳过排序
   │     [+ TranscriptService::recent_for_wake_up → compress_recent_sessions]
   └─ 正常路：
      tokio::join!(
        Store::search_candidates(tenant)             ← lifecycle pool：全活跃集（lance only_if 扫描）
        embedding_provider.embed_text(query)         ← 本地/OpenAI 推理出 query 向量
      )
      → Store::hybrid_candidates(tenant, query, vec, k=48)
          → bm25_candidate_ids                       ← Tantivy BM25（src/storage/fts.rs）
          → ann_candidate_ids                        ← lance nearest_to（capability_capsule_embeddings）
          → pipeline::ranking::rrf_merge（k=60）
      → pipeline::retrieve::rank_with_hybrid_and_graph
          → 合并 pool + hybrid hits，is_expired 过滤（硬过期）
          → score_with_hybrid：RRF 分 + lifecycle 栈（memory_type×intent / scope / confidence / freshness − decay）
          → [expand_graph] graph_anchor_nodes → compute_graph_boosts（GRAPH_BOOST=12，fanout 稀释，K9 边动态）  ← 读 graph_edges
          → finalize：relevance floor + O3 per-source cap
      → pipeline::compress::compress（tiktoken o200k_base 切 token_budget → 4 段）
      → enqueue_capsules_used                        # O1：异步标 last_used（读路径外）
```

**读取**：`capability_capsules` / Tantivy BM25 / `capability_capsule_embeddings`（lance ANN）/ `graph_edges`。
**特点**：lifecycle pool 与 query 向量并行（`tokio::join!`）；BM25 + ANN 两路在 `hybrid_candidates` 内顺序取候选；graph 扩展可选（`expand_graph=true`），graph 出错自动降级为无 graph boost 重试。

#### `GET /capability_capsules/{id}` — 单条详情

```
capability_capsule.rs::get_capability_capsule → CapabilityCapsuleService::get_capability_capsule
  → Store::get_capability_capsule         # 跨租户取行
  → embedding_meta                        # = capability_capsule_embeddings + 最近 embedding_jobs.status
  → Store::neighbors                      # 该 capsule 的活动图边 ← 读 graph_edges
  → list_capability_capsule_versions      # supersedes 链
  → feedback_summary
```

只读。带 embedding 状态、图边、版本链。

#### `POST /capability_capsules/feedback` — 反馈

```
capability_capsule.rs::submit_feedback → CapabilityCapsuleService::submit_feedback
  → Store::get_capability_capsule_for_tenant
  → Store::apply_feedback   ← 写 feedback_events，调权 confidence/decay/status
```

不动 embedding / 索引；只更新行 + 写事件。`incorrect` → status=Archived。

#### `POST /episodes` — 工作流原料

```
capability_capsule.rs::ingest_episode → CapabilityCapsuleService::ingest_episode
  → Store::list_successful_episodes_for_tenant
  → pipeline::workflow::maybe_extract_workflow
  ├ 命中 → 递归 CapabilityCapsuleService::ingest（创建 workflow capsule，又触发 embed）
  └ Store::insert_episode    ← 写 episodes
```

可能触发 embedding job 入队（间接经过 workflow ingest）。

### 2.2 Review（人工审核 / pending）

| 路由 | 服务 | 关键写入 |
|---|---|---|
| `GET /reviews/pending` | `CapabilityCapsuleService::list_pending_review` | 只读 |
| `POST /reviews/pending/accept` | `accept_pending` | `capability_capsules.status` → Active |
| `POST /reviews/pending/reject` | `reject_pending` | `capability_capsules.status` → Rejected |
| `POST /reviews/pending/edit_accept` | `edit_and_accept_pending` → `replace_pending_with_successor` + `close_edges_for_capability_capsule` + `extract_graph_edge_drafts` + `sync_memory_edges` + 重新入队 embedding job | capability_capsules（supersede）/ graph_edges / embedding_jobs |
| `POST /reviews/{auto_promote,idle_archive,evolution}` | `auto_promote_sweep` / `idle_archive_sweep` / `evolution_sweep`（`{dry_run}`） | 治理 sweep（预览 vs 执行；详见 README「治理 / 自进化」） |

`edit_accept` 是最重的——本质是"在审核里改完接受"，会重做一遍 ingest 路径。

### 2.3 Graph

`GET /graph/neighbors/{node_id}` → `CapabilityCapsuleService::graph_neighbors` → `Store::neighbors`。读 `graph_edges` 默认 `valid_to IS NULL`（活动边），多跳走 `neighbors_within`（Rust 迭代 BFS，`MAX_HOPS_CAP=3`）。其余：`/graph/edges`(+`/invalidate`)、`/graph/stats`、`/graph/timeline`、`/graph/predicate`、`/graph/tunnels`(`/find`·`/follow`)。`node_id` 含 `:` 必须 URL-encode。

### 2.4 Entities（消歧 / 别名）

| 路由 | 服务 | 关键写入 |
|---|---|---|
| `POST /entities` | `EntityService::create_with_aliases` → `lookup_alias` 预检 + `resolve_or_create` + 多次 `add_alias` | entities / entity_aliases |
| `GET /entities` | `list_entities` | 只读 |
| `GET /entities/{id}` | `get_entity`（lance-native，alias 在 Rust 侧关联） | 只读 |
| `POST /entities/{id}/aliases` | `add_alias` | entity_aliases |

别名匹配 lowercase + whitespace-collapsed，PK `(tenant, alias_text)` 兜重；并发安全靠 lance 的乐观并发提交 + Rust 侧 `resolve_or_create`（先查 alias，命中即返，否则插实体 + 插 alias）。

### 2.5 Embeddings（运维 / 调试）

| 路由 | 行为 | 数据源 |
|---|---|---|
| `GET /embeddings/jobs` | 读队列状态 | `embedding_jobs` |
| `POST /embeddings/rebuild` | 给某 tenant 全量或指定 id 列表重建：stale 旧 job + 重新 enqueue（`force` 绕去重） | `embedding_jobs` / `capability_capsule_embeddings` |
| `GET /embeddings/providers` | 当前 provider 配置 | 只读 (config) |

`/embeddings/*` 默认在 MCP 隐藏（`MEM_MCP_EXPOSE_EMBEDDINGS=1` 才暴露）；HTTP 默认开。向量由 `embedding_worker` 异步算入 `capability_capsule_embeddings`，ANN 的 IVF_PQ 索引由 `vacuum_worker` 维护。

### 2.6 Transcripts（对话归档 / parallel pipeline）

> 与 capsules 完全不共享：单独的表（`conversation_messages`）、单独队列（`transcript_embedding_jobs`）、单独向量表（`conversation_message_embeddings`）。详见 mempalace-diff §14。

```
POST /transcripts/messages(/batch) → TranscriptService::ingest(_batch)
  → Store::create_conversation_message(s)               ← 写 conversation_messages
       （内联按 embed_eligible 入队 transcript_embedding_jobs；MEM_TRANSCRIPT_EMBED_DISABLED=1 跳过嵌入）

POST /transcripts/search → TranscriptService::search
  ├ BM25 路：Store::bm25_transcript_candidates           ← Tantivy（conversation_messages.content）
  ├ 语义路：embed_text(query) → Store::semantic_search_transcripts
  │         ← lance nearest_to（conversation_message_embeddings，chunk-collapse MIN _distance）
  │   （任一 lance-scan 出错 → warn! 软降级，另一路兜底，绝不 500）
  → transcript_recall::{score_candidates, merge_windows}  # 上下文窗口合并（±context_window，anchor / 共现加权）
  → fetch_conversation_messages_by_ids                    # hydrate

GET /transcripts?session_id=… → get_by_session / get_by_session_paged（cursor scroll）
GET /transcripts/range（跨 session 范围）  ·  GET /transcripts/sessions（会话列表）
```

**MCP 表面不暴露**——transcript 搜索 HTTP 独占，agent 走 capsule search → 命中后用 `session_id` 拉对应 transcript。

### 2.7 Health

`GET /health` 返回纯文本 `ok`。**不**校验 Lance 数据集可写、Tantivy 索引就绪、embedding worker 存活——只确认进程在监听。需要更严的 readiness 检查就再加一条（见 §4 优化方向）。

---

## 3. 跨接口共享基础设施

### 3.1 写哪些表 / 读哪些索引（汇总）

| 路由 | embedding_jobs | 向量 ANN | BM25 (Tantivy) | graph_edges | entity registry |
|---|---|---|---|---|---|
| POST /capability_capsules | ✏️ enqueue | — | (vacuum worker 重建) | ✏️ | ✏️ |
| POST /capability_capsules/search | — | 🔍 read | 🔍 read | 🔍 read（可选）| — |
| POST /capability_capsules/feedback | — | — | — | — | — |
| POST /episodes | ✏️（间接）| — | — | ✏️（间接）| ✏️（间接）|
| POST /reviews/pending/edit_accept | ✏️ enqueue | — | — | ✏️ rewire | ✏️ |
| POST /embeddings/rebuild | ✏️ enqueue | (worker 重建索引) | — | — | — |
| POST /transcripts/messages | ✏️ enqueue (transcript) | — | (vacuum worker 重建) | — | — |
| POST /transcripts/search | — | 🔍 read | 🔍 read | — | — |

### 3.2 后台 worker

| Worker | 触发 | 输入 | 写入 / 作用 |
|---|---|---|---|
| `embedding_worker` | poll `EMBEDDING_WORKER_POLL_INTERVAL_MS`（默认 10s）+ claim `EMBEDDING_BATCH_SIZE`（默认 8） | `embedding_jobs (status=pending\|failed)` | `capability_capsule_embeddings`（lance 向量） |
| `transcript_embedding_worker` | 同上 poll（`MEM_TRANSCRIPT_EMBED_DISABLED=1` 时不跑） | `transcript_embedding_jobs` | `conversation_message_embeddings` |
| `vacuum_worker` | 每 `MEM_VACUUM_INTERVAL_SECS` | 全表 manifest | 修剪旧 manifest + `ensure_query_indexes`（IVF_PQ ANN）+ Tantivy FTS 全量重建 |
| `decay_worker` | 每小时 | `capability_capsules` | 硬过期 + 时间衰减 `decay_score`（lance `table.update()`） |
| `last_used_worker` | 每 `MEM_LAST_USED_FLUSH_SECS`（默认 5s，always-on） | search 发的 capsule-used 事件 | 盖 `last_used_at`（O1 检索强化，读路径外） |
| `auto_promote_worker` | 每小时（**默认 ON**） | `capability_capsules (PendingConfirmation)` | 长期 idle 候选 → Active |
| `idle_archive_worker` | 24h（opt-in，默认 OFF） | `capability_capsules (Active)` | 全维度 dead-weight → Archived |
| `dedup_worker` | opt-in（默认 OFF） | `capability_capsules` | union-find 近重复聚类（dry-run 预览） |
| `evolution_worker` | 每 `MEM_EVOLUTION_INTERVAL_SECS`（默认 24h，opt-in） | active pool 既有向量 | merge / generalize 候选 → `evolution_candidates`（K-cycle 反抖） |
| `cooccurrence_worker` | opt-in | search 共访问事件 | `cooccurs_with` 图边 |
| `potentiation_worker` | opt-in（K9） | 共召回事件 channel | Hebbian 边增强 |
| `topic_tunnel_worker` | opt-in | `capability_capsules` | 跨项目 `user_tunnel:topic:` 隧道边 |

### 3.3 BM25 索引（in-RAM Tantivy）

BM25 是自带的 in-RAM Tantivy 子系统（`src/storage/fts.rs`，jieba precision-mode 分词），**不是 lance 索引、没有 dirty-flag**。两个 bucket（capsule + transcript）各一个倒排索引，从源 Lance 表**全量重建**：进程启动建一次，之后由 `vacuum_worker` 调 `MaintenanceStore::{ensure_query_indexes,rebuild_query_indexes}` 重建（`rebuild_capsule_fts` / `rebuild_transcript_fts`）。真实规模下一次全量重建 <1s（10× 规模 ~6s），所以没有磁盘索引、没有 stale-index 窗口。读路径有 `*_fts_built` 惰性 latch 兜底：万一重建被跳过，首次 `bm25_candidate_ids` 查询自行 lazy-build，索引永不缺失。

### 3.4 向量 ANN 索引（lance 原生 IVF_PQ）

ANN 是 lance 原生的：`query().nearest_to(vec)`，IVF_PQ 索引由 `vacuum_worker` 经 `MaintenanceStore::ensure_query_indexes`（`src/storage/lance_store/maintenance.rs`）维护——无外部 sidecar、无容量/flush 概念。策略：表行数 < `MIN_ROWS_TO_INDEX`（5000）保持 flat-scan（已 sub-second）；未索引增量超过 `REINDEX_DELTA_THRESHOLD`（4096）触发重建；`ivf_num_partitions` 按行数定分区数。`conversation_message_embeddings` 这种大表必须建索引（否则 flat-scan 5-11s）；capsule 嵌入表小，flat-scan 即可。

### 3.5 RRF + lifecycle 双层评分

```
score = round(1000 × (1/(60 + rank_lex) + 1/(60 + rank_sem)))   # 召回融合
       + apply_lifecycle_score(...)                              # 加性微调：
                                                                 #   confidence × 10
                                                                 #   validated +3
                                                                 #   freshness  + bucket
                                                                 #   decay      × 12
                                                                 #   provisional -4
                                                                 #   intent×memory_type
                                                                 #   scope ±18 / -4
                                                                 #   graph_boost ±12
                                                                 #   evidence  + 2
```

评分实现见 `pipeline/retrieve.rs::{score_with_hybrid, apply_lifecycle_score, finalize}`，RRF 融合在 `pipeline::ranking::rrf_merge`。

### 3.6 召回并行度（现状）

`CapabilityCapsuleService::search` 已用 `tokio::join!` 把 **lifecycle pool 加载** 与 **query 向量推理**并行（见 §2.1）。剩下的顺序点在 `Store::hybrid_candidates` 内部：`bm25_candidate_ids`（Tantivy）与 `ann_candidate_ids`（lance ANN）仍是先后 await（`store.rs::hybrid_candidates_compose`）。进一步并行机会见 §4.2。

### 3.7 批量写入端点性能（bench）

`POST /capability_capsules/batch` 与 `POST /transcripts/messages/batch` 把 N 行写入折叠成一次 Lance manifest commit（外加合并 service 层的 graph 解析 / entity 查询 / embedding 入队），吃掉了原来"每行一次写"的瓶颈。`examples/ingest_bench.rs` 在 service 层 release build + 干净 tempdir 上对比单条 / 批量两条路径（每场景独立开 store，不复用 disk cache）：

```
== Capability capsules ==
    N   single (ms)    batch (ms)    speedup    per-row μs
----------------------------------------------------------
   10       21998.1        2257.3       9.7x    2200 →  226
   50       93777.1       18218.6       5.1x    1876 →  364
  100      130312.1       14246.8       9.1x    1303 →  142

== Transcript blocks ==
    N   single (ms)    batch (ms)    speedup    per-row μs
----------------------------------------------------------
   10         673.9          72.8       9.3x      67 →    7
   50       12335.5          81.2     151.9x     247 →    2
  100       21808.4          96.3     226.6x     218 →    1
```

**几个值得记的观察**：

- **Capsule 路径每行 ~1-2 ms 主要是每行一次 Lance manifest commit（copy-on-write 写整份 manifest）**——批量后 N 次 commit → 1 次，所以 9-11× 提升基本是常数项摊销。Service 层的 graph edge 解析、entity registry 查询、embedding job enqueue 也合并成单次调用。（route-B 后 `Store::commit_lance_write` 已是 no-op pass-through，不再有写后刷新开销。）
- **Transcript 收益更夸张（最高 227×）是因为单条路径在 `create_conversation_message` 里每行都跑一次 `count_rows((path,line,block))` 做 dedup**——表越大扫描越慢，N=100 时已经是 218 μs/行的扫描放大。批量路径用一次 `transcript_path IN (...)` filter 拉所有现有 key，剩下全部内存里 dedup → 近 O(1)。
- **典型 `mem mine` 场景**：一份会话 transcript 通常 100s–1000s blocks + 几十个 capsule。整体写入从分钟级降到秒级（100 capsule + 100 block 从 152 s → 14 s，约 10×）。

**复现命令**：

```bash
cargo run --example ingest_bench --release
# 或自定义规模：
MEM_BENCH_SIZES=10,50 cargo run --example ingest_bench --release
```

**HTTP 端点契约**：

- 批量 capsule 返回 `{items: [{result: "ok"|"err", capability_capsule_id?, status?, error?}, ...]}`。201（全成功）或 207 Multi-Status（任一失败）。
- 批量 transcript 返回 `{message_block_ids: [...], inserted: <usize>}`。`inserted` 是真正落库的行数（去重后），`message_block_ids` 是输入序号一一对应的 id。

详见 `bench(ingest)` commit `1b800e5`、`feat(ingest): batch …` commit `f3e7100`。

---

## 4. 可优化方向（按优先级）

### 4.1 ✅ FTS rebuild thrashing — route-B 的 Tantivy 子系统已根除

旧问题：DuckDB FTS 的「每写置 dirty → 下次 search 强制 drop+create 重建」在热路径上抖动。route-B 把 BM25 换成 in-RAM Tantivy（§3.3）后这个问题消失：没有 dirty-flag、没有 drop/create，索引由 `vacuum_worker` 全量重建，真实规模下 <1s（10× ~6s）。剩余唯一考量是**重建耗时随语料线性**——到极端规模（数十万行、单次重建 >几秒）才需要考虑增量索引；当前规模无需。

### 4.2 🟠 hybrid_candidates 内 BM25 / ANN 仍顺序

pool 加载与 query embed 已 `tokio::join!`（§3.6）。但 `Store::hybrid_candidates_compose` 内部仍是先 `bm25_candidate_ids().await?` 再 `ann_candidate_ids().await?`——两路无数据依赖，可并行：

```rust
let (bm25, ann) = tokio::join!(
    self.bm25_candidate_ids(tenant, query_text, oversample),
    self.ann_candidate_ids(tenant, query_embedding, oversample),
);
```

注意：Tantivy BM25 是 CPU-bound in-RAM、lance ANN 偏 IO——并行收益取决于两者耗时是否相当。另：query embed 仍是本地推理绑 CPU，可让 caller 预传 query 向量省一次推理。

### 4.3 🟠 没有 query-result cache

热门 query（agent autopilot 默认 query）反复打。candidate-level 不好缓存（lifecycle 信号会变），但可以：
- 缓存 BM25 候选 ID 列表（按 Tantivy 索引重建 epoch 失效）
- 缓存 query embedding（content hash → vec）

### 4.4 🟠 多进程写入检测缺失（incident TODO #3）

同一 Lance 数据集目录被多个 `mem serve` 同时写入会争抢 manifest commit（Lance 乐观并发会重试，但 `mem serve` 内本就有多个并发 writer task、非单写者，多进程叠加放大冲突）。进程内没有全局写锁（route-B 后读写都是 lance-native，无单一连接 mutex）。建议：
- 启动时取 OS 文件锁（`fs4::try_lock_exclusive` on `<MEM_DB_PATH>/.lock`），已被占用则告警 + 退出
- 优于 pid file（无 stale-PID / TOCTOU 问题）

### 4.5 🟡 health endpoint 太轻

`GET /health` 只返回 "ok"，不校验：
- Lance 数据集可写
- embedding worker tick 心跳
- queue 是否堆积（pending 数）
- Tantivy 索引已 build

建议加 `GET /health/deep` 做 readiness probe，pending job 数 / 嵌入向量行数 vs capsule 行数 drift / Tantivy 索引就绪全部检查；CI 用得上。

### 4.6 🟡 metrics 缺位

`tracing` log 有，没有 `/metrics`（Prometheus）。建议加：
- request 数 / latency 直方图（按 route）
- embedding job pending / processing / failed 计数
- 嵌入向量行数 / IVF_PQ 索引未覆盖增量
- Tantivy FTS rebuild 次数 / 耗时
- decay worker last-tick 时间

### 4.7 🟡 没有 rate limiting / auth

本地用没事；远程暴露需要先做 API key（mempalace-diff §7 已论证）。当前 README / AGENTS.md 写明"不能暴露公网"。

### 4.8 🟢 ingest 写路径上的"重活"可异步化

`POST /capability_capsules` 现在做：幂等查 → insert → graph_edge_drafts → entity resolve → sync edges → embed enqueue。其中 entity resolve 涉及 `lookup_alias` + `resolve_or_create` + `add_alias` 多次 lance 读写。可以：
- 把 entity / graph 写入丢给独立 worker（一类"ingest_postprocess_jobs" 队列），HTTP 立刻返回
- 风险：search 在 graph 写入完成前看不到新边——和 embedding 相同的 eventual consistency

### 4.9 🟢 transcript / memory 双向链接还没暴露

caller 只能 `memory_search` 拿命中再用 `session_id` 拉 transcript，反向（transcript hit → 关联的 memory）没接口。可以加 `GET /transcripts/{block_id}/related-memories`。

### 4.10 🟢 graph 读路径每次 N+1

`get_capability_capsule` 拿邻居用 `Store::neighbors` 单次扫描，但 search 里 `related_capability_capsule_ids(anchors)` 对每个 anchor 单独扫 `graph_edges`。可合并成一次 `from_node_id IN (...)` 谓词扫描。

---

## 5. 已知问题

### 5.1 ⓧ FTS rebuild "stopwords has been deleted" — route-B 后不复存在

历史：DuckDB FTS extension 1.x 的 `drop_fts_index`/`create_fts_index` dependency tracker 在热路径反复 rebuild 时 commit 失败（500，`stopwords has been deleted`），靠 `INSTALL fts; LOAD fts;` reload 兜底修过。route-B 删掉了整个 DuckDB FTS 引擎（BM25 改 in-RAM Tantivy，§3.3），**这个故障类已不可能发生**——没有 drop/create、没有 extension dependency catalog。仅作历史记录保留。

### 5.2 ⚠️ embedding orphan FK 循环（incident memory `mem_019dfba4-9e08-71b2-a676-f0218c01f9b6`）

历史事故；已沉淀三层防御（open-time / claim-time / tick last-resort）。详见 ROADMAP #1 注脚 + incident memory。

### 5.3 ⓧ HNSW 容量不自动增长 — route-B 后不复存在

历史：usearch HNSW sidecar 的 upsert 不调 `reserve()`，过容量后新 memory 落不进，曾加 ×2 几何增长修过。route-B 删掉了 HNSW sidecar（ANN 改 lance 原生 IVF_PQ，§3.4），**容量概念已不存在**。仅作历史记录保留。

### 5.4 ⓧ migrate_content_hash_to_sha256 FK 安全 — DuckDB FK 行为已不适用

历史：DuckDB 把带子表行的 parent UPDATE 实现成 DELETE+INSERT、撞 FK RESTRICT，曾套 load/delete/restore dance 修过 bootstrap。route-B 后存储是 Lance（无外键约束、无该 UPDATE 重写行为），这个隐患不再存在。仅作历史记录保留。

### 5.5 后续 TODO（不计入 ROADMAP）

来自 incident memory：
1. 找出 `embedding_jobs` orphan 行的产生路径（疑在某条 supersede / delete pipeline）
2. 检测同一 Lance 数据集目录多进程写入（OS 文件锁，见 §4.4）

---

## 6. 维护规则

1. 改任何一条接口的服务/管线/存储调用链 → 更新 §2 对应小节。
2. 加新表 / 新索引 → 更新 §3 矩阵。
3. 加新工人 → 更新 §3.2 + §1 工作量表。
4. 解决 §4 / §5 任意一条 → 状态改 ✅ 并写日期；commit 引用本文件章节号。
5. 新增本质架构差异（mem ⇄ MemPalace）→ 同步更新 `mempalace-diff §15.2/15.3/15.4`。
