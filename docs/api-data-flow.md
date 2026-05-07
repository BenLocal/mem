# mem HTTP API — 数据流转 / 技术栈 / 优化方向

> **目的**：给一个真正能用上的"接口地图"——每条 HTTP 路由对应哪条服务/管线/存储调用链、用到的技术、可优化点。半年后回头改任何一条接口前先读这个。
>
> **同步源**：本文与 `src/http/*.rs` + `src/service/` + `src/pipeline/` + `src/storage/` 实际代码绑定；改架构时**先改代码、再来更新本文档**，commit 引用本文件章节号（`docs(api-flow): … (closes api-flow §X)`）。
>
> 设计原则与 mempalace 借鉴见 [`mempalace-diff.md`](./mempalace-diff.md)，路线图见 [`ROADMAP.MD`](./ROADMAP.MD)，本文档专注于"运行时"。

---

## 0. 总体架构

```
                 ┌───────────────────────── HTTP (axum :3000) ─────────────────────────┐
caller (codex/   │                                                                      │
 cursor/CI/CLI) ─┼─→  /memories       /memories/search   /memories/feedback             │
                 │   /memories/{id}   /reviews/*         /entities/*                    │
                 │   /episodes        /graph/neighbors   /transcripts/*                 │
                 │   /embeddings/*    /health                                           │
                 └─────┬────────────────────────────────────────────────────┬───────────┘
                       │                                                    │
              ┌────────▼─────────┐                              ┌───────────▼──────────┐
              │ MemoryService    │                              │ TranscriptService    │
              │ EntityService    │  (service 层 = 编排，无业务规则) │ (verbatim archive)   │
              └────────┬─────────┘                              └───────────┬──────────┘
                       │                                                    │
              ┌────────▼─────────┐                              ┌───────────▼──────────┐
              │ pipeline/        │   ingest / retrieve /        │ FTS BM25 +           │
              │  ingest          │   compress / workflow        │ HNSW sidecar (read)  │
              │  retrieve(RRF)   │                              │ context window merge │
              │  compress(tok.)  │                              │                      │
              │  workflow        │                              │                      │
              └────────┬─────────┘                              └───────────┬──────────┘
                       │                                                    │
                  ┌────▼────────────────── storage ─────────────────────────▼────┐
                  │  DuckDbRepository (Arc<Mutex<Connection>>)                    │
                  │   ├ memories / memory_embeddings / embedding_jobs             │
                  │   ├ episodes / feedback_events / sessions                     │
                  │   ├ graph_edges (DuckDbGraphStore)                            │
                  │   ├ entities / entity_aliases (EntityRegistry)                │
                  │   └ conversation_messages / transcript_embedding_jobs         │
                  │  VectorIndex (usearch HNSW) ×2: memories + transcripts        │
                  │  FTS extension (BM25) on memories + conversation_messages     │
                  └──────────────────────────┬────────────────────────────────────┘
                                             │
                       ┌─────────────────────┴────────────────────┐
                       │       后台 worker（tokio::spawn）         │
                       │  embedding_worker  →  embedding_jobs      │
                       │  transcript_embedding_worker              │
                       │  decay_worker      →  低频时间衰减         │
                       └──────────────────────────────────────────┘
```

**核心约束**（值得被反复念）：
- DuckDB 是 **single-writer**，整个进程通过 `Arc<Mutex<Connection>>` 串行访问；并发的代价已经付了，不要再叠"伪并发"。
- 检索层的两路（BM25 + HNSW）在 `merge_and_rank_hybrid_scored` 里**顺序**执行，**没有并行**——优化机会，详见 §3.6。
- `memories.content` 是 verbatim 事实源，`memories.summary` 是索引提示——**输出层永远基于 content**（见 mempalace-diff §7）。

---

## 1. 技术栈映射

| 层 | 技术 / Crate | 在哪里 |
|---|---|---|
| HTTP | **axum 0.x** + **tokio** + tower middleware | `src/http/*.rs`, `src/main.rs` |
| 序列化 | serde (snake_case) | `src/domain/*.rs` |
| 错误 | `thiserror` + 自家 `AppError` (HTTP status mapping) | `src/error.rs` |
| 存储 | **DuckDB** (bundled, single-file) via `duckdb` crate, `Arc<Mutex<Connection>>` | `src/storage/duckdb.rs` |
| ANN | **usearch** (HNSW，cosine, F32, sidecar) | `src/storage/vector_index.rs` |
| 词法 | **DuckDB FTS extension**（BM25, conjunctive=0） | `db/schema/005_fts.sql` + `ensure_fts_index_fresh` |
| 排序 | **RRF**（k=60，×1000 scale）+ **lifecycle 加性层**（intent×memory_type / scope / freshness / decay / graph_boost） | `src/pipeline/retrieve.rs::score_candidates_hybrid_rrf` + `apply_lifecycle_score` |
| Tokenizer | **tiktoken-rs**（`o200k_base`，CJK 友好）用于压缩输出 | `src/pipeline/compress.rs` |
| 哈希 | **sha2** SHA-256（content_hash 跨进程稳定，迁移自 SipHash） | `src/pipeline/ingest.rs::compute_content_hash` |
| Embedding | **embed_anything** (本地 Qwen3, dim=1024 默认) / **OpenAI** BYOK / **fake** (测试) — `Arc<dyn EmbeddingProvider>` | `src/embedding/` |
| Graph | DuckDB 表 `graph_edges`（valid_from / valid_to 时序），mempalace-diff §5 | `src/storage/graph_store.rs` |
| Entity registry | DuckDB 表 `entities` + `entity_aliases`（PK = `(tenant, alias_text)`，normalize lower+ws-collapsed） | `src/storage/entity_repo.rs` |
| Async workers | tokio::spawn × 3：embedding / transcript-embedding / decay | `src/service/embedding_worker.rs` 等 |
| 观测 | `tracing` (env filter via `RUST_LOG`) | 全局 |

---

## 2. 接口逐条数据流转

约定（每条都用同一个图式）：

```
HTTP handler → service 方法 → pipeline / storage 调用链 → 写入哪些表 / 读哪些索引
```

### 2.1 Memory 生命周期

#### `POST /memories` — 写一条 memory

```
memory.rs::ingest_memory
 → MemoryService::ingest
   → pipeline::ingest::initial_status                      # write_mode + memory_type → status
   → pipeline::ingest::compute_content_hash (SHA-256)
   → DuckDbRepository::find_by_idempotency_or_hash         # 幂等 / 去重
   → DuckDbRepository::insert_memory                       ← 写 memories 表
   → pipeline::ingest::extract_graph_edge_drafts
   → service::memory_service::resolve_drafts_to_edges      # 含 EntityRegistry::resolve_or_create
   → DuckDbGraphStore::sync_memory_edges                   ← 写 graph_edges
   → DuckDbRepository::try_enqueue_embedding_job           ← 写 embedding_jobs（pending）
```

**写入**：`memories` / `graph_edges` / `entities` / `entity_aliases` / `embedding_jobs`。
**特点**：embed 异步——HTTP **不等** embedding 完成，job 入队就返回 201。

#### `POST /memories/search` — 检索（核心读路径）

```
memory.rs::search_memory
 → MemoryService::search
   ┌─ 词法路：DuckDbRepository::lexical_candidates
   │     → ensure_fts_index_fresh (按 dirty flag 重建 FTS)   ← 读 FTS index
   │     → bm25_candidates (LEFT JOIN memories)              ← 读 memories
   ├─ 语义路：embedding_provider.embed_text(query)            ← 调用本地/OpenAI 推理
   │     → DuckDbRepository::semantic_search_memories
   │       → VectorIndex::search (HNSW ANN)                  ← 读 HNSW sidecar
   │       → fetch_memories_by_ids                           ← 读 memories
   ├─ 融合：pipeline::retrieve::rank_with_graph_hybrid
   │     → merge_and_rank_hybrid_scored
   │     → score_candidates_hybrid_rrf  (RRF k=60, ×1000)
   │     → apply_lifecycle_score        (decay/confidence/scope/intent/graph)
   │     [graph 扩展] graph.related_memory_ids(anchors)      ← 读 graph_edges
   └─ 输出：pipeline::compress::compress
         → tiktoken o200k_base 切 token_budget
         → 4 段：directives / facts / patterns / workflow
```

**读取**：`memories` / FTS index / `memory_embeddings` (HNSW sidecar) / `graph_edges`。
**特点**：lexical 与 semantic 当前**串行**调用；HNSW 走 ID → memories 二次 SQL；graph 扩展是可选（`expand_graph=true` 才走）。

#### `GET /memories/{id}` — 单条详情

```
memory.rs::get_memory → MemoryService::get_memory
  → DuckDbRepository::get_memory_for_tenant
  → embedding_meta_for_memory       # = memory_embeddings + 最近 embedding_jobs.status
  → DuckDbGraphStore::neighbors     # 该 memory 的活动图边
  → list_memory_versions_for_tenant # supersedes 链
  → feedback_summary
```

只读。带 embedding 状态、图边、版本链。

#### `POST /memories/feedback` — 反馈

```
memory.rs::submit_feedback → MemoryService::submit_feedback
  → DuckDbRepository::get_memory_for_tenant
  → DuckDbRepository::apply_feedback   ← 写 feedback_events，调权 confidence/decay
```

不动 embedding / 索引；只更新行 + 写事件。

#### `POST /episodes` — 工作流原料

```
memory.rs::ingest_episode → MemoryService::ingest_episode
  → list_successful_episodes_for_tenant
  → pipeline::workflow::maybe_extract_workflow
  ├ 命中 → 递归 MemoryService::ingest（创建 workflow memory，又触发 embed）
  └ DuckDbRepository::insert_episode    ← 写 episodes
```

可能触发 `try_enqueue_embedding_job`（间接经过 workflow ingest）。

### 2.2 Review（人工审核 / pending）

| 路由 | 服务 | 关键写入 |
|---|---|---|
| `GET /reviews/pending` | `MemoryService::list_pending_review` → `list_pending_review` | 只读 |
| `POST /reviews/pending/accept` | `accept_pending` | `memories.status` → Active |
| `POST /reviews/pending/reject` | `reject_pending` | `memories.status` → Rejected |
| `POST /reviews/pending/edit_accept` | `edit_and_accept_pending` → `replace_pending_with_successor` + `close_edges_for_memory` + `extract_graph_edge_drafts` + `sync_memory_edges` + `enqueue_embedding_job_for_memory` | memories（supersede）/ graph_edges / embedding_jobs |

`edit_accept` 是最重的——本质是"在审核里改完接受"，会重做一遍 ingest 路径。

### 2.3 Graph

`GET /graph/neighbors/{node_id}` → `MemoryService::graph_neighbors` → `DuckDbGraphStore::neighbors`。读 `graph_edges` 默认 `valid_to IS NULL`（活动边）。`node_id` 含 `:` 必须 URL-encode。

### 2.4 Entities（消歧 / 别名）

| 路由 | 服务 | 关键写入 |
|---|---|---|
| `POST /entities` | `EntityService::create_with_aliases` → `lookup_alias` 预检 + `resolve_or_create` + 多次 `add_alias` | entities / entity_aliases |
| `GET /entities` | `list_entities` | 只读 |
| `GET /entities/{id}` | `get_entity` | 只读 |
| `POST /entities/{id}/aliases` | `add_alias` | entity_aliases |

并发安全靠 `Arc<Mutex<Connection>>` 串行 + transaction；**没有 DB 级 unique partial index**（DuckDB bundled 不支持），靠应用层 PK `(tenant, alias_text)` 兜底。

### 2.5 Embeddings（运维 / 调试）

| 路由 | 行为 | 数据源 |
|---|---|---|
| `GET /embeddings/jobs` | 读队列状态 | `embedding_jobs` |
| `POST /embeddings/rebuild` | 给某 tenant 全量或指定 id 列表重建：可选清旧 sidecar 行 + stale 旧 job + 重新 enqueue | `memory_embeddings` / `embedding_jobs` / HNSW sidecar |
| `GET /embeddings/providers` | 当前 provider 配置 | 只读 (config) |

`/embeddings/*` 默认在 MCP 隐藏（`MEM_MCP_EXPOSE_EMBEDDINGS=1` 才暴露）；HTTP 默认开。

### 2.6 Transcripts（对话归档 / parallel pipeline）

> 与 memories 完全不共享：单独的表、单独的队列、单独的 HNSW sidecar。详见 mempalace-diff §14。

```
POST /transcripts/messages → TranscriptService::ingest
  → DuckDbRepository::create_conversation_message       ← 写 conversation_messages
  → set_transcripts_fts_dirty                           # FTS 标 dirty，下次 search 重建
  → 可选 enqueue transcript_embedding_jobs              # MEM_TRANSCRIPT_EMBED_DISABLED=1 跳过

POST /transcripts/search → TranscriptService::search
  → bm25_transcript_candidates                          ← 读 FTS (conversation_messages)
  → 可选 VectorIndex(transcript).search                  ← 读 transcript HNSW sidecar
  → fetch_conversation_messages_by_ids
  → score_candidates + merge_windows                    # 上下文窗口合并

POST /transcripts {session_id, tenant, [limit, cursor, since, until]}
  → get_conversation_messages_by_session  (when limit omitted)
  → get_conversation_messages_by_session_paged  (cursor scroll)
```

**MCP 表面不暴露**——transcript 搜索 HTTP 独占，agent 走 `memory_search` → 命中后用 `session_id` 拉对应 transcript。

### 2.7 Health

`GET /health` 返回纯文本 `ok`。**不**校验 DB 可写、HNSW 完整、embedding worker 存活——只确认进程在监听。需要更严的 readiness 检查就再加一条（见 §4 优化方向）。

---

## 3. 跨接口共享基础设施

### 3.1 写哪些表 / 读哪些索引（汇总）

| 路由 | embedding_jobs | HNSW | FTS | graph_edges | entity registry |
|---|---|---|---|---|---|
| POST /memories | ✏️ enqueue | — | (写后变 dirty) | ✏️ | ✏️ |
| POST /memories/search | — | 🔍 read | 🔍 read | 🔍 read（可选）| — |
| POST /memories/feedback | — | — | — | — | — |
| POST /episodes | ✏️（间接）| — | — | ✏️（间接）| ✏️（间接）|
| POST /reviews/pending/edit_accept | ✏️ enqueue | — | — | ✏️ rewire | ✏️ |
| POST /embeddings/rebuild | ✏️ enqueue | (worker 间接清理) | — | — | — |
| POST /transcripts/messages | ✏️ enqueue (transcript) | — | (写后变 dirty) | — | — |
| POST /transcripts/search | — | 🔍 read | 🔍 read | — | — |

### 3.2 后台 worker

| Worker | 触发 | 输入表 | 写入 |
|---|---|---|---|
| `embedding_worker` | 1 Hz tick + claim job | `embedding_jobs (status=pending\|failed)` | `memory_embeddings` + HNSW sidecar |
| `transcript_embedding_worker` | 1 Hz tick (除非 disabled) | `transcript_embedding_jobs` | `conversation_message_embeddings` + transcript HNSW sidecar |
| `decay_worker` | 低频（按 updated_at 推 decay） | `memories` | `memories.decay_score` |

### 3.3 FTS dirty-flag 模型

每条写（ingest / edit_accept / feedback）`fts_dirty.store(true)`；下次 search 时 `ensure_fts_index_fresh` swap-and-rebuild：`drop_fts_index('memories')` → `create_fts_index('memories', 'memory_id', 'summary', 'content')`。**这是当前实测最痛的瓶颈**，详见 §4.1 / §5.1。

### 3.4 HNSW sidecar 容量

`upsert` 在写入前若 `size+1 > capacity` 会自动 ×2 grow（fix d49d49b）。flush 由 `MEM_VECTOR_INDEX_FLUSH_EVERY` 控制（默认 1024 / transcripts 256）。

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

`MEM_RANKER=legacy` 一档兜底（旧加性求和），用于 A/B。

### 3.6 lexical 与 semantic 当前**顺序**执行

`MemoryService::search` 里两路顺序 await，没有 `tokio::join!`。BM25 + 一次 embed_text + ANN 全在 request 关键路径上。优化机会，见 §4.6。

---

## 4. 可优化方向（按优先级）

### 4.1 ✅ FTS rebuild thrashing — async worker 已落（剩余仅极端规模）

`src/service/fts_worker.rs`（`MEM_FTS_REBUILD_INTERVAL_MS` 默认 2000）背景 tick，每次扫两个 dirty flag（memories + conversation_messages）并 rebuild。`bm25_candidates` 路径的同步 `ensure_fts_index_fresh` 保留作为兜底（rare race window：写完到下次 worker tick 之间正好来了 search）。worker 跑过的 swap-and-drain 操作是原子的，与 reader fallback 不会双 rebuild。

**Trade-off**：BM25 命中新写入 memory 的排名延迟 ≤ 1 tick interval。HNSW 语义一路不受影响（embedding worker 自己负责），所以新 memory **依然立即可被找到**——只是缺 BM25 boost ≤ 2s。

**触发更深优化的条件**（仍然可能要做）：
1. 即使 worker 跑了，rebuild 本身随 memories 行数线性。10k 行起单次 rebuild 也吃 CPU，影响 worker tick 间隔之外的其他 worker（embedding 也在同一连接 mutex 上）
2. **incremental FTS**：DuckDB FTS 1.x 不支持。等 2.x 或换 tantivy

**触发条件**：当 P95 `/memories/search` 仍 >200ms 或 memories.count > 5k 时再考虑。

### 4.2 🟠 search 关键路径顺序执行 → 并行化

```rust
// 现在
let lexical = repo.lexical_candidates(...).await?;
let semantic = embed_then_search(...).await?;

// 改成
let (lexical, semantic) = tokio::join!(
    repo.lexical_candidates(...),
    embed_then_search(...),
);
```

注意：embed 调用本地推理时绑 CPU，HTTP 路径上**不该**直接调；建议：
1. provider 提供 batch + cache（重复 query 不重新 embed）
2. 或者提前算好 query embedding（caller 传入，省一次推理）

### 4.3 🟠 没有 query-result cache

热门 query（agent autopilot 默认 query）反复打。candidate-level 不好缓存（lifecycle 信号会变），但可以：
- 缓存 BM25 候选 ID 列表（按 dirty flag 失效）
- 缓存 query embedding（content hash → vec）

### 4.4 🟠 多进程检测缺失（incident TODO #3）

同一 `.duckdb` 多个 `mem serve` 写入会出 FK race；当前只 `Arc<Mutex<Connection>>` 进程内串行。建议：
- 启动时写 pid file `<MEM_DB_PATH>.pid`，存在则告警 + 退出
- 或 advisory lock（DuckDB 不直接支持，可用 fcntl 锁 sidecar 文件）

### 4.5 🟡 health endpoint 太轻

`GET /health` 只返回 "ok"，不校验：
- DB 可写
- HNSW 行数与 DuckDB 一致
- embedding worker tick 心跳
- queue 是否堆积（pending 数）

建议加 `GET /health/deep` 做 readiness probe，pending job 数 / HNSW vs DB drift / FTS extension loaded 全部检查；CI 用得上。

### 4.6 🟡 metrics 缺位

`tracing` log 有，没有 `/metrics`（Prometheus）。建议加：
- request 数 / latency 直方图（按 route）
- embedding job pending / processing / failed 计数
- HNSW size / DuckDB embedding row count
- FTS rebuild 次数 / 耗时
- decay worker last-tick 时间

### 4.7 🟡 没有 rate limiting / auth

本地用没事；远程暴露需要先做 API key（mempalace-diff §7 已论证）。当前 README / AGENTS.md 写明"不能暴露公网"。

### 4.8 🟢 ingest 写路径上的"重活"可异步化

`POST /memories` 现在做：FK 查 → insert → graph_edge_drafts → entity resolve → sync edges → embed enqueue。其中 entity resolve 涉及 lookup_alias + resolve_or_create + add_alias 多次 SQL。可以：
- 把 entity / graph 写入丢给独立 worker（一类"ingest_postprocess_jobs" 队列），HTTP 立刻返回
- 风险：search 在 graph 写入完成前看不到新边——和 embedding 相同的 eventual consistency

### 4.9 🟢 transcript / memory 双向链接还没暴露

caller 只能 `memory_search` 拿命中再用 `session_id` 拉 transcript，反向（transcript hit → 关联的 memory）没接口。可以加 `GET /transcripts/{block_id}/related-memories`。

### 4.10 🟢 graph 读路径每次 N+1

`get_memory` 拿邻居用 `DuckDbGraphStore::neighbors` 单次 SELECT，但 search 里 `related_memory_ids(anchors)` 对每个 anchor 单独 SELECT。可改 `IN (?,?,?,...)` 一次拉。

---

## 5. 已知问题

### 5.1 ✅ FTS rebuild "stopwords has been deleted"（2026-05-06 修复）

**历史症状**：

```
500 {"error":"duckdb error: TransactionContext Error: Failed to commit:
        Could not commit creation of dependency, subject \"stopwords\" has been deleted"}
```

**根因**：DuckDB FTS extension 1.x 的 dependency tracker 在 drop+create 同 connection 上第二次起会留 stale 边——`drop_fts_index` 删除 stopwords macro 后，下一次 `create_fts_index` commit 事务时 dependency edge 命中已删 id，commit 失败。每条 ingest 让 FTS dirty，下次 search 强制 rebuild → 进入这个洞，热路径上第二次以后必现。

**实测过的不行的方案**：
- `CHECKPOINT;` 在 drop 和 create 之间——dependency 是 in-memory，CHECKPOINT 只 flush WAL，不清依赖。
- `create_fts_index(..., overwrite := 1)`——bundled DuckDB 1.x binder 只认 `(VARCHAR, VARCHAR)` 形式，named arg 直接 reject。

**生效的修复**（`src/storage/duckdb.rs::rebuild_memories_fts` + `rebuild_transcripts_fts`）：检测到 create 抛 dependency-stopwords error 后，自动跑 `INSTALL fts; LOAD fts;` 强制 reload extension state（清掉 in-memory dependency catalog），再重做一次 drop+create。第二次基本一定成功；如果还失败 error 直透传，`fts_dirty` 仍 true 让下次 query 重试。

`is_fts_dependency_error` 只抠 `"stopwords"` / `'stopwords'` 子串作为 sentinel——稳定跨 1.x 版本的 error 文案。

**回归保护**：
- `tests/search_api.rs::fts_rebuild_survives_repeat_dirty_cycles` —— 走 HTTP search → ingest → search 二轮，验证第二次 rebuild 不再 500
- `src/storage/duckdb.rs::fts_recovery_tests` —— `is_fts_dependency_error` 三向检测单测

### 5.2 ⚠️ embedding orphan FK 循环（incident memory `mem_019dfba4-9e08-71b2-a676-f0218c01f9b6`）

历史事故；已沉淀三层防御（open-time / claim-time / tick last-resort）。详见 ROADMAP #1 注脚 + incident memory。

### 5.3 ✅ HNSW 容量不自动增长（已修 d49d49b）

之前 upsert 不调 `reserve()`，过容量后所有新 memory 落不进 sidecar。已加 ×2 几何增长。

### 5.4 ✅ migrate_content_hash_to_sha256 FK 安全（已修 6af5a3e）

带子表行的 legacy memory，DuckDB 把 parent UPDATE 实现成 DELETE+INSERT 撞 FK RESTRICT，bootstrap 起不来。已套 load/delete/restore dance。

### 5.5 后续 TODO（不计入 ROADMAP）

来自 incident memory：
1. 找出 `embedding_jobs` orphan 行的产生路径（疑在某条 supersede / delete pipeline）
2. 检测同 `.duckdb` 多进程写入（advisory lock / pid file）

---

## 6. 维护规则

1. 改任何一条接口的服务/管线/存储调用链 → 更新 §2 对应小节。
2. 加新表 / 新索引 → 更新 §3 矩阵。
3. 加新工人 → 更新 §3.2 + §1 工作量表。
4. 解决 §4 / §5 任意一条 → 状态改 ✅ 并写日期；commit 引用本文件章节号。
5. 新增本质架构差异（mem ⇄ MemPalace）→ 同步更新 `mempalace-diff §15.2/15.3/15.4`。
