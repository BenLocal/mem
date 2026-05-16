# mem storage 层耦合剖析与 backend trait 演进 (v1 — 2026-05-15)

> 这一篇竖向解剖 mem 自家 `src/storage/`，回答"能不能把 store 抽成 backend trait 接 Postgres / SQLite / 其他后端"。与 `mempalace-diff-v2.md` 是不同维度——那篇横向对照 MemPalace 的 MCP 表面层，本篇只看自家 storage 内部。
>
> 用途：评估 backend trait 抽象的**可行性**与**落实路径**。结论是可行但不简单，建议分 5 phase 渐进推进，第一个真正的验证点在 Phase 2（抽 `CapsuleStore` + 一个 in-memory 替代实现跑通）。
>
> 维护原则：本篇与代码不一致时**以代码为权威**；落地一项 quick win / long-term 工程后回到对应表格更新状态（✅ done / 🚧 in progress / 📦 shipped + commit）。

---

## 0. 方法论

| 步骤 | 来源 |
|---|---|
| commit 基线 | `4d46d7d` (2026-05-15) |
| `src/storage/` 总行数 | `wc -l src/storage/**/*.rs` → 12,476 行 |
| `Store` 方法数 | `grep -cE '^    pub (async )?fn ' src/storage/store.rs` → **88** |
| Lance Schema 函数数 | `grep -cE 'fn .*schema\(' src/storage/lance_store/mod.rs` → **12** |
| `lance_write_then_refresh!` call site | `grep -c 'lance_write_then_refresh!' src/storage/store.rs` → **27** |
| Service / pipeline 持 Store 引用的位置 | `grep -rnE 'Arc<Store>|&Store\b' src/service/ src/pipeline/` |
| 调用 `self.store.` 的次数 | `grep -cE 'self\.store\.' src/service/capability_capsule_service.rs` → **27** (单文件) |

非 storage 的依赖（service / pipeline / worker / HTTP / tests）这一层只统计"持有 `Store` 引用的入口"，不展开内部使用——本报告只对 storage 内部做拆解，外层的影响只在 §2.13 里点名。

---

## 1. 一句话结论

> Store 层"重"在 **15 个具体耦合点**——其中 5 个是 Lance/DuckDB-extension 特有的设计哲学（无法绕开），10 个是当前实现的脚手架（可以收敛或抽象）。
>
> **6 个 quick win** 在 1 周内可落地、不破现有 API；**真 trait 化推荐分 5 phase 推进**，CapsuleStore 是第一个验证点（Phase 2，约 2 周）。在 Phase 2 失败前不要承诺多 backend 的支持时间表。

---

## 2. 十五个耦合点

按"换 backend 时需要重写多少代码"递减排列。每条都标了具体文件：行号，方便后续修改时对照。

### 2.1 单 backend 假设贯穿到组合层

`src/storage/store.rs:69-75` —— `Store` 是个**具体 struct**，硬绑两个 `Arc`：

```rust
pub struct Store {
    pub(crate) lance: Arc<LanceStore>,
    pub(crate) query: Arc<DuckDbQuery>,
}
```

没有 trait。所有 service / pipeline / worker / HTTP / 测试都拿 `Arc<Store>`。这是**最大单点**——换 backend 前必须先抽 trait 才能下手任何替换。

### 2.2 `lance_write_then_refresh!` 宏是隐式合同

`src/storage/store.rs:123-142`，**27 个调用点**。每次写都强制刷一遍 DuckDB 端的 snapshot cache。这条合同只对"Lance 写 + DuckDB 读同一目录"成立——

- Postgres 单引擎不需要
- 纯 Lance（不用 DuckDB extension）不需要
- SQLite / Redis 也都不需要

宏本身没有 trait 抽象。**新 backend 进来要么没法套这个宏，要么得给宏加 no-op 分支。**

### 2.3 DuckDB lance extension 是读层的全部基础

`src/storage/duckdb_query/mod.rs:127-128, 158-168`：

```sql
INSTALL lance; LOAD lance;
ATTACH '<path>' AS ns (TYPE LANCE);
```

整个 88 方法读半边都靠这层。包括两个核心：

- `lance_fts('ns.main.<tbl>', 'content', ?, k => ?)` —— BM25 入口
- `lance_vector_search('ns.main.<tbl>', 'embedding', [...], k => ?)` —— ANN 入口

最戳眼的是 `hybrid_candidates`（`src/storage/duckdb_query/capability_capsules.rs:715-892`）：**一段 SQL 把 BM25 + ANN + RRF + tenant filter + status filter + LIMIT 全融在一起**，包括 `WITH fts CTE ... vec CTE ... FULL OUTER JOIN ... fused`，SQL 178 行，绑了至少 9 个参数。

换到 Postgres 整段必须重写（pg_trgm + pgvector + RRF in SQL）；换到外部 ANN 服务整段必须**拆成 Rust-side compose**。

### 2.4 12 张表的 Arrow Schema 嵌在 Rust 源码里

`src/storage/lance_store/mod.rs` 12 个 `fn *_schema() -> arrow_schema::Schema`：

| 行 | 表名 |
|---|---|
| 261 | `capability_capsules` |
| 383 | `sessions` |
| 405 | `episodes` |
| 448 | `feedback_events` |
| 537 | `capability_capsule_embeddings` |
| 635 | `conversation_message_embeddings` |
| 731 | `embedding_jobs` |
| 870 | `transcript_embedding_jobs` |
| 985 | `graph_edges` |
| 1057 | `entities` |
| 1125 | `entity_aliases` |
| 1163 | `conversation_messages` |

每张表配一对 `*_to_record_batch` / `record_batch_to_*` builder/parser。这是 Lance/Arrow **特有**的 schema 表达方式。Postgres 这层等价物是 `CREATE TABLE`，SQLite 类似，KV 后端则根本不需要 schema。**换 backend = 把这 12 对 builder/parser 全部重写一遍 + 各自定义自己的 schema 表达方式**。

### 2.5 ANN / FTS 索引由 Lance backend 内置

`src/storage/lance_store/mod.rs:177-178, 221-247` —— FTS 索引（`lance::index::scalar::FtsIndexBuilder`）在 `open()` 时调 `ensure_fts_index` 自动建。

⚠️ **关于 HNSW sidecar 的 stale 文档**：`CLAUDE.md` 和老 README 还在写"usearch HNSW sidecar + `<MEM_DB_PATH>.usearch`"——**那条路径已经不在代码里了**（仅剩 `StorageError::VectorIndex(String)` 一个 enum variant 和 config 里几个 vestigial env var）。Lance 0.27 把 ANN 内置了，整个 sidecar 逻辑被删掉过。

实质后果：**ANN 算法选择权完全在 Lance 手里**——换 backend 要么自带 ANN，要么得在 trait 里把 `top_k_vector_search(query, k)` 抽出来当 backend 的责任。同理 BM25。

### 2.6 DuckDB 单连接 mutex

`src/storage/duckdb_query/mod.rs:88` —— `conn: Arc<Mutex<Connection>>`。所有读请求都**串行**走这一把锁。每次读都是 `tokio::task::spawn_blocking` 包一层 sync `duckdb-rs` 调用。

这是 DuckDB 单 writer + `duckdb-rs` 1.x 没有 async 接口的产物。换到 Postgres：用 `sqlx` 连接池，没有 mutex；换到 SQLite：还是要 mutex（同样单 writer）；换到 Redis：根本不存在这个问题。

**真问题**：mem 的所有读吞吐受这把锁限制，包括纯 SELECT。

### 2.7 Embedding 队列状态机绑在 Lance UPDATE 上

`src/storage/lance_store/capability_capsules.rs:265-340` `claim_next_n_embedding_jobs`：optimistic claim 通过 `table.update().only_if("job_id=... AND status='pending'").column("status", "'processing'")`，依赖 `rows_updated` 返回。

`embedding_jobs` 跟 `transcript_embedding_jobs` 两张表的 `pending → processing → completed | failed | stale` 状态机也是 Store 的明面 API（`Store::claim_next_n_embedding_jobs` / `complete_embedding_job` / `mark_embedding_job_stale` / `reschedule_embedding_job_failure` 等共约 10 个方法）。

**这套 API 形状把"队列做成一张普通表+UPDATE optimistic claim"这一选择固化了**。换 backend 要么各自实现这套 10 个方法，要么把队列抽象成 `dyn JobQueue` 单独一个 trait（见 §5.5）。

### 2.8 Graph 表读路径有两套

| 路径 | 文件 | 形态 |
|---|---|---|
| DuckDB 端 | `src/storage/duckdb_query/graph.rs:63-156` | Rust 端 BFS + 每跳一次 SQL |
| Lance native | `src/storage/lance_store/graph.rs:42-58` | Lance scan + Rust 端排序（"LanceDB has no ORDER BY"） |

两套都对 `(from_node_id, to_node_id, relation, valid_from, valid_to)` 这个 schema 写死。换 backend 至少得实现 BFS 那个。

### 2.9 Lance 自家的 SQL-like 过滤器 (`sql_quote`)

`src/storage/lance_store/*.rs` 大量 `table.query().only_if(format!("col = {}", sql_quote(...)))`。Lance 的 filter 长得像 SQL 但**不是**真 SQL（只支持部分谓词），也不支持参数化绑定。

这是 Lance 特有的——既不是 DuckDB SQL，也不是真的 SQL injection-safe 范式。换 backend 要么各自拼自己后端的过滤语法，要么定义一个 mem 自己的查询 IR（"like sqlx Query DSL"）。

### 2.10 Lazy-create vs eager-create 表

`src/storage/lance_store/mod.rs:156-167`：10 张表在 `open()` 里 `ensure_*_table` 立即建；**两张 embedding 表**（`capability_capsule_embeddings`、`conversation_message_embeddings`）lazy-create on first upsert，因为 dim 是 embedding provider 决定的。

`hybrid_candidates` 里专门有重试逻辑 `is_capability_capsule_embeddings_missing` 来兜底"表还不存在"的情形。这是 Lance schemaless-but-typed 哲学的产物——Postgres 没法这么干（dim 改了得 migration），KV 后端更不在乎。

### 2.11 Snapshot 缓存绕路是设计的一半

`src/storage/store.rs:24-38` —— 这块设计的核心是"lance DuckDB extension 在第一次查询后会 cache dataset version；DETACH/ATTACH 都清不了 cache；只有新开 `Connection::open_in_memory()` 才能看见后续写"。

整个 `DuckDbQuery::refresh()`（`src/storage/duckdb_query/mod.rs:158-179`）都是为这件事服务的。它**每次写之后丢掉整个 in-process DuckDB 连接重开**，约 100 ms 一次。

换 backend 时这部分**完全消失**。但目前所有写方法都假设它存在——这是耦合。

### 2.12 `spawn_blocking` 桥接进了每个调用形状

`src/storage/duckdb_query/mod.rs:186-209` `spawn_blocking_storage` / `spawn_blocking_graph` helper。**每个**DuckDB 读方法的 body 都是 `spawn_blocking_storage(move || { ... })`，因为 `duckdb-rs` 是 sync。Lance Rust API 是 async-native，没这个开销。

新 backend 是 async-native 的话（sqlx、tokio-postgres）这层就没必要；保留它意味着调用形状被 sync-bridge 框死。

### 2.13 Pipeline / service / worker 全用具体 `&Store` / `Arc<Store>`

| 文件:行 | 类型 |
|---|---|
| `src/service/capability_capsule_service.rs:83` | `store: Arc<Store>`（27 处 `self.store.<method>`）|
| `src/service/transcript_service.rs:79` | `store: Arc<Store>` |
| `src/service/entity_service.rs:38` | `store: Arc<Store>` |
| `src/pipeline/retrieve.rs:76` | `graph: &Store`（参数）|
| `src/pipeline/session.rs:66` | `repo: &Store`（参数）|
| `src/worker/*.rs` (5 个 worker) | `Arc<Store>` |

**约 100+ 个调用点**。任何 trait 抽象都得在这里做一次机械替换。代价是定的，但是机械活——只有 §2.1 抽 trait 之后才能开始做。

### 2.14 enum ↔ snake_case JSON 串

| 文件 | helper |
|---|---|
| `src/storage/duckdb_query/mod.rs:250-261` | `enum_to_text` / `parse_enum` |
| `src/storage/lance_store/mod.rs:1366-1380` | `enum_to_str` / `enum_from_str` |

两个独立 impl 维护同一个 `serde_json::Value::String` round-trip 协议。`CapabilityCapsuleStatus`、`CapabilityCapsuleType`、`Scope`、`Visibility`、`EntityKind` 都靠这个。

倒不算大耦合，但说明 **domain 类型已经隐含了"我会被序列化成 snake_case JSON 字符串存"的契约**。Postgres backend 可以用 ENUM 类型；要么得引入一层 domain ↔ wire 的显式转换。

### 2.15 跨表组合查询用 SQL JOIN，写半边没有等价物

`src/storage/store.rs:983` `context_window_for_block`：拉一个 block 周围的同 session blocks——`src/storage/duckdb_query/transcripts.rs` 用 SQL `WHERE session_id = ? AND created_at BETWEEN ... ORDER BY ...`。Lance native API 不支持 JOIN，所以**这条路径只能走 DuckDB**。

如果 backend trait 要把"读半边"完全独立于 DuckDB，这种跨表组合得在 Rust 层做（多次 fetch + 组合），但每个 backend 都得实现，没有"SQL 自动帮你"的捷径。

---

## 3. 最小可行的 backend trait 边界

### 3.1 九个 sub-trait 的分组

| Sub-trait | 方法数 | 形状 | 难度 |
|---|---|---|---|
| `CapsuleStore` | ~20 | insert / get / list (by tenant, scope, idempotency, hash, version chain) / accept_pending / reject_pending / supersede / apply_feedback | 容易（纯 CRUD） |
| `CapsuleSearchStore` | ~5 | hybrid_candidates / search_candidates / bm25_candidates / semantic_search / recent_active | **难**（绑 `lance_fts` + `lance_vector_search`） |
| `EmbeddingJobStore` | ~10 | enqueue / claim_next_n / complete / stale / fail / reschedule（capsule+transcript 各一套） | 中等（状态机 + optimistic claim） |
| `EmbeddingVectorStore` | ~5 | upsert / delete / get / lazy-create-table | 中等（dim provider-dependent） |
| `GraphStore` | ~10 | neighbors / neighbors_within (BFS) / kg_timeline / sync_edges / add_edge_direct / invalidate / graph_stats | 容易（schema 清晰） |
| `TranscriptStore` | ~15 | create_conversation_message(s) / get_by_session{,_paged} / context_window / anchor_session_candidates / bm25_transcript_candidates / semantic_search_transcripts / range queries | **难**（跨表 JOIN + BM25 + ANN） |
| `EntityRegistry` | ~5 | resolve_or_create / add_alias / get / lookup_alias / list_entities | 容易 |
| `SessionStore` / `EpisodeStore` | ~5 | touch / open / close / latest_active / list_successful_episodes | 容易 |
| `MaintenanceStore` | ~3 | apply_time_decay / vacuum_old_versions / auto_promote_candidates | 中等（vacuum 不是所有 backend 都该有，见 §7.5） |

### 3.2 聚合 trait 定义

```rust
pub trait Backend:
    CapsuleStore + CapsuleSearchStore + EmbeddingJobStore + EmbeddingVectorStore +
    GraphStore + TranscriptStore + EntityRegistry + SessionStore + MaintenanceStore +
    Send + Sync + 'static {}
```

每个 sub-trait 是 async + 统一 `Result<T, StorageError>` 返回。`StorageError` 加一个 `BackendError(Box<dyn Error + Send + Sync>)` 透传具体 backend 错误（caller 目前只 match `NotFound` vs 其他）。

### 3.3 五个边界设计取舍（每个都得先想清楚）

| # | 取舍 | 推荐 | 理由 |
|---|---|---|---|
| 1 | Search 输入输出形状 `(text, vec, k) → Vec<(Capsule, score)>` 还是分开 | **聚合到一个方法** | Rust 层做 RRF 在小 k 下很便宜；引擎内做能省 round trip 但收益有限 |
| 2 | Transaction 契约要不要 | **不要** | Lance 没事务，service 层已经容忍中间态；加 `transaction()` 让 Lance 假装能 commit/rollback 反而骗人 |
| 3 | Vector dim lazy-create 要不要暴露在 trait | **不暴露** | 统一 `ensure_capsule_embedding_dim(dim)` 一个方法；各 backend 自己决定是 lazy 建表（Lance）、ALTER 维度（pgvector）还是 no-op |
| 4 | `StorageError` 还是 backend 各自的 error | **统一 StorageError** | caller 只 match `NotFound` 和"其他"两类；具体细节走 `BackendError(Box<...>)` |
| 5 | DuckDB extension 路径保留在 trait 上吗 | **作为 LanceBackend 实现细节，trait 完全不感知** | `Backend::open(path, settings)` 是工厂，`LanceBackend::open` 内部继续起 LanceStore + DuckDbQuery |

---

## 4. Quick wins — 不动大架构、先做的解耦

每条 1-3 个 commit，不破任何现有测试，独立 ship。

| # | 现状 | 改法 | 收益 | 风险 | 状态 |
|---|---|---|---|---|---|
| QW-1 | `hybrid_candidates` 178 行 SQL 把 BM25+ANN+RRF 融在一起 | 拆成 `bm25_candidate_ids` / `ann_candidate_ids` / Rust 端 `rrf_merge` / `fetch_by_ids` 的 compose 路径 | 未来换 backend 时只需各自实现两个简单方法 | 多 round trip 真实存在（bench 测得 +14~29%），所以 **保留 fused-SQL 作为 LanceBackend default**，compose 作为 portable 参考路径 | ✅ — see commit ref in §4.1 below |
| QW-2 | `lance_store/*.rs` 写方法 filter 用 `sql_quote` 字符串拼接 | ~~lancedb 0.27 `update().only_if()` 接 bound params，改一遍~~ ⚠️ **deferred** — lancedb 0.27 expr API 仅支持读路径，写/删仍只能传字符串；sql_quote 输入全是内部字段，injection 风险实质为零；部分迁移制造两种风格。Phase 2 trait 抽离时定义 mem-internal predicate IR 更干净 | 去掉一个 SQL-ish 拼装层 | API gap：覆盖率 ~30% (42/140 callsite)；不值得引入 datafusion_expr 依赖 | ⚠️ **deferred** — see §4.2 + §7.2 |
| QW-3 | `decode_embedding_blob` / `f32_slice_to_blob` 在 `service::embedding_helpers` 但被 storage 调用 | 移到 `crate::embedding::wire`，明确"应用 ↔ 存储"wire format | wire 层显式，未来 backend 可选 native f32 数组而非 byte blob | 纯 refactor，无 | ✅ — see §4.3 |
| QW-4 | `config.rs:142-285` 大量 `MEM_VECTOR_INDEX_*` env + `#[allow(dead_code)]` 字段；CLAUDE.md 还讲 usearch sidecar | 删字段 + 删配置 + 修文档 | 减 cognitive load，对齐代码现状 | 失去 `MEM_TRANSCRIPT_OVERSAMPLE` 的 startup validation（live read 已自带 fallback） | ✅ — see §4.4 |
| QW-5 | `pipeline/retrieve.rs:76 graph: &Store` 但实际只调几个 graph 方法（外加 `pipeline/session.rs:66 repo: &Store` 同形态） | 给 pipeline 定义 `trait GraphRead` + `trait SessionStore` 子集，pipeline 全部接 `&dyn _` | pipeline 层先 trait 化，是 Phase 2 的预演 | 无 | ✅ — see §4.5 |
| QW-6 | `lance_write_then_refresh!` 宏在 store.rs，27 调用点 | 改成 `Store::commit_write<F, T>(write_fn) -> T` 显式 method | method 可被 LanceBackend 覆盖、TestBackend no-op、Postgres 不实现；宏没法这么干 | 27 处机械替换 |

### 4.1 QW-1: 拆 `hybrid_candidates` 成 Rust-compose (✅ landed)

现状是 178 行 SQL inline 做 BM25+ANN+RRF。拆法：

```text
hybrid_candidates_compose(tenant, text, vec, k)
  ├─ bm25_candidate_ids(tenant, text, k*2)  → Vec<(id, rank_lex)>
  ├─ ann_candidate_ids(tenant, vec, k*2)    → Vec<(id, rank_sem)>
  ├─ ranking::rrf_merge(bm25, ann)          → Vec<(id, score)>  (Rust HashMap)
  └─ fetch_capability_capsules_by_ids(top_k_ids) + 重排
```

**Bench 结果** — `examples/hybrid_compose_vs_fused_bench.rs`，N=500 capsules，dim=64，M=30 iter / cell：

| k | compose mean | fused mean | Δ % | compose p99 | fused p99 |
|---|---|---|---|---|---|
| 10 | 68.41 ms | 60.01 ms | **+14.0%** | 112.35 ms | 96.19 ms |
| 50 | 90.96 ms | 73.39 ms | **+23.9%** | 152.21 ms | 101.16 ms |
| 100 | 118.23 ms | 91.85 ms | **+28.7%** | 169.53 ms | 122.56 ms |

**结论**：fused-SQL 一致快 14–29%，幅度随 k 增大。这不是噪声——多 1~2 次 DuckDB round trip + 缺少 SQL engine 的 join pushdown 是真实代价。

**降级方案 shipped**：
- `Store::hybrid_candidates` 仍走**fused-SQL**（性能保持，零回归），标记 `LANCE-SPECIFIC`
- 新增 `Store::hybrid_candidates_compose` 走 Rust 端组合路径，是 Phase 2 trait 抽离时其他 backend 的参考形态
- 新增的两个原子原语 `Store::bm25_candidate_ids` + `Store::ann_candidate_ids` + `pipeline::ranking::rrf_merge` **是 QW-1 的核心交付**——这些是 trait 抽象的 enabler，无论 Lance 默认走哪条路径都已经备齐

**关于 trait 形状**：Phase 2 抽 `CapsuleSearchStore` trait 时，`hybrid_candidates` 是 trait method；LanceBackend impl 直接调 fused-SQL（继承当前 perf）；其他 backend 用 compose 默认（或自家 fusion）。trait 的"边界"通过两个原语暴露，不通过 fused-SQL 的 178 行 SQL 暴露。

**对后续 Phase 的影响**：
- Phase 2 抽 trait 时 `hybrid_candidates_compose` 就是 default trait method body
- Phase 4 Postgres spike 时直接复用 compose 路径（实现 `bm25_candidate_ids` via pg_trgm，`ann_candidate_ids` via pgvector），不用从零写 fused SQL

### 4.2 QW-2: `sql_quote` → 参数化绑定 (⚠️ deferred, 2026-05-16)

§7.2 标的不确定性现在 resolved（见 §7.2 详细）。结论是 **lancedb 0.27 的 expr API 不覆盖写路径**：

| 操作 | 字符串 filter | `datafusion_expr::Expr` |
|---|---|---|
| `query().only_if()`（读） | ✅ | ✅ `only_if_expr(...)` |
| `UpdateBuilder::only_if()`（写） | ✅ | ❌ 不支持 |
| `Table::delete(predicate)`（删） | ✅ | ❌ 不支持 |

`sql_quote` 在 `src/storage/lance_store/` 的分布（实测 grep）：

| 文件 | 总用 | 写/删（必留 string） | 读（可换 expr） |
|---|---|---|---|
| `capability_capsules.rs` | 67 | ~50 | ~17 |
| `transcripts.rs` | 39 | ~30 | ~9 |
| `graph.rs` | 15 | ~9 | ~6 |
| `entities.rs` | 10 | ~3 | ~7 |
| `sessions.rs` | 7 | 6 | 1 |
| `episodes.rs` | 2 | 0 | 2 |
| **合计** | **140** | **~98 (70%)** | **~42 (30%)** |

**决策：deferred 不动代码**。原因：
1. **API 覆盖不全** —— 写/删 70% 的 callsite 必须保留 sql_quote，部分迁移制造两种风格让未来维护者犹豫该用哪个
2. **风险实质为零** —— sql_quote 的输入全是内部字段（`capsule_id` / `tenant` / `job_id` / 时间戳），不接用户输入；SQL injection 不是现实威胁
3. **依赖代价** —— 部分迁移要加 `datafusion_expr` 直接依赖（lancedb 是间接传过来的），换 30% callsite 的"epsilon-better 转义"不值
4. **Phase 2 有更干净的方案** —— trait 抽离时定义 mem-internal predicate IR（`Predicate::Eq(Column, Value)` 之类），各 backend 自己翻译；不依赖 lancedb 任何 expr 形态

**Phase 2 含义**：trait 上的 query/update/delete 方法暴露的是**结构化 predicate**，不是 raw string 或 datafusion_expr。LanceBackend impl 内部把 predicate 翻译成 lancedb 的 `only_if(String)` 或未来更宽的 expr 形态；其他 backend impl 翻译成自家 SQL / RESP / KV scan。

**追溯路径**：如果 lancedb 后续版本（≥0.30？）把 expr API 扩展到写/删，QW-2 可以从 deferred 升级回 actionable —— 那时整个 storage 层切到 expr 是干净的，不必先做部分迁移。盯 lancedb changelog 即可。

### 4.3 QW-3: embedding 编码搬出 storage (✅ landed)

新模块 `src/embedding/wire.rs` 暴露：

```rust
pub fn encode_f32_blob(values: &[f32]) -> Vec<u8>
pub fn decode_f32_blob(blob: &[u8], dim: usize) -> Result<Vec<f32>, &'static str>
```

**搬迁前后**：

| 函数 | 之前 | 现在 |
|---|---|---|
| encode (`f32_slice_to_blob` → `encode_f32_blob`) | `src/service/embedding_helpers.rs` | `src/embedding/wire.rs` |
| decode (`decode_embedding_blob` → `decode_f32_blob`) | `src/storage/lance_store/mod.rs` (`pub(super)`) | `src/embedding/wire.rs` (`pub`) |

7 个 callsite 迁移完毕：2 workers (`embedding_worker`, `transcript_embedding_worker`) + 1 bench example + 2 storage decode sites + 移除 service/embedding_helpers 与 lance_store/mod 的原定义。

**设计点**：
- decode 返回 `Result<Vec<f32>, &'static str>` 而非具体 error type——`'static` 字符串保持 wire 模块零依赖；callers 用 `.map_err(StorageError::InvalidData)` 自行包装
- 依赖方向从 `storage → service`（倒挂）改成 `application → embedding ← storage`（正常）
- 命名对称：`encode_f32_blob` / `decode_f32_blob`，比旧的 `f32_slice_to_blob` / `decode_embedding_blob` 更明显是 codec 对子
- 3 个单测：round-trip 保值（含 `INFINITY`）、空 vec、长度不匹配 reject

**未来工作**：当其他 backend 选择 native vector 类型（pgvector 直接吃 `Vec<f32>`、外部 ANN 服务用 HTTP/protobuf）时，wire 层显式存在意味着只需在 wire.rs 加新 codec，或在 backend impl 内直接旁路 wire 层。lance_store 现在仍走 blob 路径，将来如果 Lance 暴露更原生的 vector 写接口可以再砍。

### 4.4 QW-4: 清 usearch 残留 (✅ landed)

实际清掉的东西：

| 类别 | 数量 | 详情 |
|---|---|---|
| `EmbeddingSettings` 字段 | 5 | `vector_index_flush_every` / `vector_index_oversample` / `vector_index_use_legacy` / `transcript_vector_index_flush_every` / `transcript_search_oversample` |
| env 解析块 | 5 | 上面对应的 `MEM_VECTOR_INDEX_*` / `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY` / `MEM_TRANSCRIPT_OVERSAMPLE` |
| `ConfigError` 变体 | 4 | `InvalidVectorIndexFlushEvery` / `InvalidVectorIndexOversample` / `InvalidTranscriptVectorIndexFlushEvery` / `InvalidTranscriptOversample` |
| 单测 | 10 | `vector_index_settings_*` ×2 / `transcript_vector_index_flush_every_*` ×4 / `transcript_oversample_*` ×4 |
| dead-code 传播 | 1 处 | `src/app.rs:92-93` 把 `transcript_vector_index_flush_every` 赋给 `vector_index_flush_every`（两边都没消费） |

**Phase 0 第 2 条**已经清了所有 stale 描述（`AGENTS.md` / `README.md` / `docs/api-data-flow.md` / source comments），本 QW 跟着补一刀：

- `AGENTS.md` env 列表删除"vestigial"备注，改写为：`MEM_TRANSCRIPT_OVERSAMPLE` 单独列出（live read），legacy 那批写成"已删除"
- `README.md` env 表删 `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY` 行，改加 `MEM_TRANSCRIPT_OVERSAMPLE` 行

**关键 trade-off**：`MEM_TRANSCRIPT_OVERSAMPLE` 失去 startup validation —— 这个 env 仍然 live（`TranscriptService::search:285` 直接 `std::env::var`），但 invalid 值（非数字 / 0）现在 silently fall back to default 4 而非 mem serve 启动期报错。`transcript_service` 的 `.filter(|n| *n > 0).unwrap_or(4)` 兜底逻辑早就在那里——原 config parser 是冗余检查，删掉 = 一个 source of truth。

实际上这种"live env read, no config-side parse"是项目里的既有模式（`MEM_DB_PATH` / `MEM_BASE_URL` / `MEM_TENANT` / `MEM_MIN_SCORE` / `MEM_SESSION_IDLE_MINUTES` 都是这样），`MEM_TRANSCRIPT_OVERSAMPLE` 现在跟齐了。

### 4.5 QW-5: pipeline 用子 trait (✅ landed)

新模块 `src/pipeline/store_traits.rs` 定义两个 `#[async_trait]` 窄 trait：

```rust
pub trait GraphRead: Send + Sync {
    async fn related_capability_capsule_ids(&self, anchors: &[String])
        -> Result<Vec<String>, GraphError>;
}

pub trait SessionStore: Send + Sync {
    async fn latest_active_session(&self, tenant: &str, caller_agent: &str)
        -> Result<Option<Session>, StorageError>;
    async fn open_session(&self, session_id: &str, tenant: &str, caller_agent: &str, now: &str)
        -> Result<Session, StorageError>;
    async fn close_session(&self, session_id: &str, ended_at: &str)
        -> Result<(), StorageError>;
}

impl GraphRead   for Store { /* delegate */ }
impl SessionStore for Store { /* delegate */ }
```

签名替换：
- `pipeline::retrieve::rank_with_hybrid_and_graph(graph: &Store)` → `&dyn GraphRead`
- `pipeline::session::resolve_session(repo: &Store)` → `&dyn SessionStore`

Service 层 callsite (2 处 in `capability_capsule_service.rs`) 改 `&self.store` → `self.store.as_ref()`，因为 `Arc<Store>: !SessionStore`（trait 没给 Arc 加 blanket impl，避免 trait-on-wrapper 的脆弱性）。

**Scope 注**：doc 原文只点了 GraphRead；实际 pipeline 还有 session.rs 取 `&Store`，**同形态、不做完会留半截**。一并 trait 化把 pipeline 100% 解耦，避免 Phase 2 mechanical sweep 时还要回头补。

**验证**：4 个 `sessions_integration` 测试 + 81 个 `pipeline::` 单测全绿。`hybrid_search` 集成测试同样过——`&Store` → `&dyn GraphRead` 是隐式 deref coercion，运行时行为零变化。

**对 Phase 2 trait 抽离的意义**：
- 验证了"窄 trait + Store blanket impl + 调用方 `as_ref()`"这个 pattern 在该项目可行（不撞 Arc-of-trait 问题）
- 当 Phase 2 把同样的模式应用到 88 个 Store 方法上时，对应 ~100 个 callsite 大致按相同手法机械重写
- 任何新 backend 实现 `GraphRead` + `SessionStore` 后，pipeline 可不修改一行代码跑在新 backend 上

### 4.6 QW-6: 宏 → 显式 method

将 `lance_write_then_refresh!` 改为：

```rust
impl Store {
    async fn commit_write<F, T, Fut>(&self, f: F) -> Result<T, StorageError>
    where F: FnOnce(&LanceStore) -> Fut, Fut: Future<Output = Result<T, StorageError>>
    { ... }
}
```

将 27 个 call site 从 `lance_write_then_refresh!(self, self.lance.foo().await)` 改为 `self.commit_write(|lance| lance.foo()).await`。语义不变，但 trait 化时可以 override。

QW-1 ~ QW-6 全做完约 **1-2 周**。完全不破任何现有 API。

---

## 5. 中长期大改 — trait 化绕不开

### 5.1 LT-1: 整体 trait 抽象 + 全调用点替换

~100 个 call site 的机械替换：`Arc<Store>` → `Arc<dyn Backend>`，`&Store` → `&dyn Backend`。脏活累活但是直的。

**前置条件**：QW-1 + QW-5 完成（否则 search 和 pipeline 没法 trait 化）。
**预估**：2 周专心做完，包括所有测试。

### 5.2 LT-2: Schema 表达统一

12 张表的 Arrow Schema 函数 + Postgres `CREATE TABLE` 怎么共存？

| 方案 | 优点 | 缺点 |
|---|---|---|
| A. 每 backend 自带一套 schema 函数 | 简单，每 backend 内部自洽 | 12 张表字段重复定义，schema drift 风险高 |
| B. mem-internal schema IR（`TableSchema { name, fields: [Field { name, type, nullable, is_list }] }`），各 backend 翻译 | schema 单一来源 | 需要写 IR + 各 backend 翻译器 |

**推荐 B**，但**先做 LT-1**——LT-1 完成后才能看清楚 schema 真的需不需要这么抽象。

### 5.3 LT-3: Vector search backend 选择

| Backend | ANN 实现 |
|---|---|
| Lance | 原生 HNSW |
| Postgres | pgvector + `CREATE INDEX ... USING hnsw` |
| SQLite | 无原生 ANN（需 sqlite-vss / usearch sidecar） |
| KV | 外部 ANN 服务或 sidecar |

trait 边界推荐：**只暴露 `top_k_vector_candidates(table, vec, k) → Vec<(id, distance)>`**。算法选择属于 backend impl。caller 不知道下面跑 HNSW 还是 IVF。

### 5.4 LT-4: BM25 同理

| Backend | BM25 实现 |
|---|---|
| Lance | `lance_fts` (DuckDB extension 层) |
| Postgres | `tsvector` + GIN |
| SQLite | FTS5 |
| 通用 | Tantivy 直接库 |

抽到 **`top_k_bm25_candidates` 一个方法**，算法选择属于 backend。

### 5.5 LT-5: 队列模块独立

当前 `embedding_jobs` 是张普通 Lance 表 + optimistic claim。换 backend 时：

- Postgres：`FOR UPDATE SKIP LOCKED` 做 atomic claim
- Redis：`BLPOP` / Streams
- 直接 in-process：tokio mpsc

**推荐**：把 embedding 队列从 storage 层抽出来变成独立模块 `crate::queue`，定义 `trait JobQueue<Job>`，让 backend 各自实现。embedding worker 不再需要 Store 来调队列。

### 5.6 LT-6: 工厂 + 配置驱动

最后一步：`Backend::open(uri, config)` 工厂，env `MEM_BACKEND=lance|postgres|sqlite|test`。不复杂，但需要 LT-1 ~ LT-5 都到位才有意义。

---

## 6. 推荐演进路径

```text
Phase 0 (1 天)
├─ 落盘本报告 docs/backend-coupling.md            ✅ done
├─ 修 CLAUDE.md / README 关于 HNSW sidecar 的 stale 描述
└─ Store 88 个方法的 doc 加 "LanceDB-specific" / "portable" 标记

Phase 1 (1 周, QW-1 ~ QW-6) ── 不引入 breaking change
├─ hybrid_candidates 拆 Rust-compose                [QW-1]
├─ lance writer 参数化绑定                          [QW-2]
├─ embedding wire format 提到 crate::embedding      [QW-3]
├─ 删 usearch 残留配置                              [QW-4]
├─ pipeline 用 GraphRead 子 trait                   [QW-5]
└─ lance_write_then_refresh! 宏 → 显式 method       [QW-6]
   ↑ 不暴露任何 trait，但把 backend-specific 行为
   都收敛到清晰的边界

Phase 2 (2 周, 第一个真 trait) ── DECISION POINT
├─ 抽 CapsuleStore trait（最小 ~20 方法）
├─ Implement: LanceCapsuleStore (lift-and-shift 当前代码)
├─ Implement: InMemoryCapsuleStore（HashMap 后端，纯做测试）
├─ capability_capsule_service 改吃 Arc<dyn CapsuleStore>
└─ 所有 capsule CRUD 测试在两个 backend 下都过
   ↑ 这一步是**最关键的验证**：trait 形状对不对，
   一个真实的 alternate impl 能跑通就大致定型

Phase 3 (3 周, 扩展到剩余 sub-trait)
├─ 抽 GraphStore / EntityRegistry / SessionStore / MaintenanceStore (容易的几个)
├─ 抽 EmbeddingJobStore / EmbeddingVectorStore (中等)
├─ 抽 CapsuleSearchStore / TranscriptStore (难的，需要 QW-1 的 Rust-compose 做底子)
└─ LanceBackend 实现所有 sub-trait，全测试绿

Phase 4 (4-6 周, Postgres spike)
├─ 实现 PostgresCapsuleStore (最干净的子集先做)
├─ 真跑起来，看真实痛点
├─ trait 边界可能要调，但只调 1-2 个 sub-trait 不动主架构
└─ 决策: trait 形状定下来 vs 还要再迭代
   ↑ 第一个"两个 backend 同时跑"的实证

Phase 5 (持续, 收尾)
├─ DuckDbQuery / LanceStore 从 public API 隐藏
├─ MEM_BACKEND env 工厂
├─ Schema IR (LT-2, 如果 Phase 4 证明确有必要)
└─ 队列模块独立 (LT-5)
```

### 6.1 Phase 0 (1 天)

- ✅ 本报告落盘 — `e3f8707`
- ✅ 修 `CLAUDE.md` / `README.md` / `docs/api-data-flow.md` 关于 HNSW sidecar 的 stale 描述 + 源码 service 层 doc 同步 — `fca52fa`
- ✅ 给 `Store` 方法的 doc 注释加上 LANCE-SPECIFIC 标记 — module 注释声明 portable 是默认；只有真正绑 Lance 行为的 10 个方法（`claim_next_n_embedding_jobs` / `claim_next_n_transcript_embedding_jobs` / `upsert_capability_capsule_embedding` / `upsert_conversation_message_embedding` / `replace_pending_with_successor` / `apply_feedback` / `hybrid_candidates` / `semantic_search_transcripts` / `bm25_transcript_candidates` / `vacuum_old_versions`）被显式标记。这 10 个就是 Phase 2 trait 抽离时必须重新设计的方法。见本 commit

### 6.2 Phase 1 (1 周)

按 §4 QW-1 ~ QW-6 顺序做。每条独立 commit，独立 review。完成后 storage 层"backend-specific 边界"明显收窄，但**对外 API 完全不变**——即便 Phase 2 决定"算了不抽 trait 了"，Phase 1 的清理也都是干净收益。

### 6.3 Phase 2 (2 周) — DECISION POINT

抽 `CapsuleStore` trait（§3.1 第一行那个最小子集）+ 一个 in-memory HashMap backend。把 `capability_capsule_service` 改吃 `Arc<dyn CapsuleStore>`。

**验证标准**：所有现有 capsule CRUD 测试在 LanceCapsuleStore 和 InMemoryCapsuleStore 两个后端下都过；trait 不需要回头改 method 签名。

**如果验证失败**（trait 形状不对、抽象漏点多），**先回头改 §3.3 的边界取舍**，不要硬推 Phase 3。

### 6.4 Phase 3 (3 周)

CapsuleStore 验证通过后，并行扩展剩余 sub-trait：

- 容易的（GraphStore / EntityRegistry / SessionStore / MaintenanceStore）2 人天/个
- 中等的（EmbeddingJobStore / EmbeddingVectorStore）4 人天/个
- 难的（CapsuleSearchStore / TranscriptStore）1-2 周/个

每个 sub-trait 都在 LanceBackend 上验证通过才下一个。

### 6.5 Phase 4 (4-6 周) — Postgres spike

先做 `PostgresCapsuleStore`（最干净的子集），跑一个集成测试 suite 看痛点。可能要回头微调 1-2 个 sub-trait 的边界，但**不应该**动主架构。

如果发现要动主架构，说明 Phase 2 验证不够彻底——这一步前的预算要重新审视。

### 6.6 Phase 5 (持续)

- `DuckDbQuery` / `LanceStore` 从 public API 隐藏（变成 `pub(crate)`）
- `MEM_BACKEND=lance|postgres|...` env 工厂
- 落 Schema IR（LT-2），如果 Phase 4 证明确有必要
- 队列模块独立（LT-5）

---

## 7. 不确定性 — 报告里还没 100% 验证的判断

每条都是后续验证项，落实之前不应该当事实写进设计文档。

### 7.1 ~~QW-1 拆 `hybrid_candidates` 的真实性能开销~~ ✅ resolved (2026-05-16)

实测 `examples/hybrid_compose_vs_fused_bench.rs`：compose 比 fused-SQL 慢 **14% (k=10) / 24% (k=50) / 29% (k=100)**。幅度随 k 增长，原因是 compose 路径多 1~2 次 DuckDB round trip 且失去 SQL engine 的 join pushdown。**结论已落地在 §4.1**：保留 fused-SQL 作为 LanceBackend default、暴露 compose 路径作为 portable 参考，原子原语 (`bm25_candidate_ids` / `ann_candidate_ids` / `rrf_merge`) 单独 ship 作为 trait 抽离 enabler。

### 7.2 ~~lancedb `update().only_if()` 参数绑定完整性~~ ✅ resolved (2026-05-16)

实测 lancedb 0.27.2 源码（`/root/.cargo/registry/src/.../lancedb-0.27.2/src/`）：

- `src/query.rs:402` — `QueryBase::only_if(filter: impl AsRef<str>)` 字符串 filter
- `src/query.rs:424` — `QueryBase::only_if_expr(filter: datafusion_expr::Expr)` ✅ type-safe，但**仅读路径**
- `src/table/update.rs:43` — `UpdateBuilder::only_if(filter: impl Into<String>)` **字符串 only**
- `src/table.rs:646` — `Table::delete(predicate: &str)` **字符串 only**

也就是说写 `update()` / `delete()` API 在 lancedb 0.27 没有 expr 形态。**QW-2 落入"epsilon-better 转义"档**——只能换 30% 的 read 路径，写/删 70% 必须保留 sql_quote。结论已落地 §4.2：**deferred**，等 lancedb 把 expr API 扩展到写路径再重启，或在 Phase 2 trait 抽离时定义 mem-internal predicate IR 统一替代 sql_quote。

### 7.3 Postgres 端 RRF SQL 难度

LT-3 说"换 backend 算法选择属于 impl"——但 Postgres 做 BM25+ANN+RRF 一个 statement 完成的难度可能比估计的高，可能逼到必须 Rust-compose。这反过来对 QW-1 形成压力——**先做 QW-1 就是为了规避这个**。

### 7.4 `spawn_blocking` 是 DuckDB 单独的成本吗

未来 Postgres 用 sqlx 是 async-native，但**写半边**还是要锁 LanceStore 这把 mutex（concurrent writes 串行）。"spawn_blocking 是 DuckDB 单独的 cost" 只对读路径成立。

### 7.5 `vacuum` 等运维操作是否每个 backend 都该有

Postgres 不需要 vacuum（它有自己的 autovacuum）；SQLite 不需要这个含义的 vacuum。**MaintenanceStore trait 可能更应该是 capability 模式**（"如果 backend 支持 prune 就有 prune"），而不是统一接口。这一条 trait 边界设计要再想。

---

## 8. 时间戳与维护

### 版本

| 版本 | 日期 | 基线 commit | 变更 |
|---|---|---|---|
| v1 | 2026-05-15 | `4d46d7d` | 首版 |

### 维护规则

1. 任何 `src/storage/` 主路径的 PR（新增方法、改写、迁移）合并后，回头检查本文档对应章节是否还成立；不成立则同 commit 一起更新本文档。
2. 落地一项 QW / LT 后在 §4 / §5 表格里把 status 标 ✅ + 链 commit hash。
3. §7 的"不确定性"被验证后，从 §7 移到正文对应位置，并附 bench 数字 / 实测引用。
4. Phase 0 ~ 5 任何 phase 完成后在 §6 对应小节加 ✅ + 完成日期。

### 与其他文档的关系

- `docs/mempalace-diff-v2.md` — 横向：mem ↔ MemPalace MCP 表面层比较
- `docs/database-schema.md` — schema 字段级别（与本文 §2.4 相关）
- `CLAUDE.md` / `AGENTS.md` — 顶层指引（与本文 §2.5 / §4.4 相关，需修 stale 描述）
- `docs/ROADMAP.MD` — 项目级路线图（本文 §6 Phase 0/1 进入路线后写一条引用）
