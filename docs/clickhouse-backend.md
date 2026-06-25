# ClickHouse Backend Design Plan

> **For agentic workers:** phased plan, repo design-doc convention (cf. `postgres-backend.md` / `evolution-worker.md`). Each phase is TDD, three-gate-green, committed with `… (closes clickhouse-backend P#)`. **Default backend stays Lance (lance-native); ClickHouse is opt-in, behind the `clickhouse` cargo feature.** Default build pulls **zero** new deps and behaves identically.

**Goal:** Let `mem serve` run entirely on a ClickHouse instance — selected at runtime via `MEM_BACKEND=clickhouse` — as a third peer to Lance (default) and Postgres (opt-in). This is a **spike** in the same posture as the Postgres backend: scaffold the full 11-sub-trait surface, feature-gate it, mark it **UNVALIDATED until run against a real ClickHouse**, and inventory the pains — *not* a production-blessed backend on day one.

**Architecture:** A concrete `ClickHouseBackend` (a `clickhouse::Client` wrapper) implements all 11 storage sub-traits, so the existing blanket `impl<T> Backend for T where T: <11 sub-traits>` (`src/storage/backend.rs`) applies unchanged. `app::AppState::from_config` chooses between `Store` (Lance), `PostgresBackend` (opt-in) and `ClickHouseBackend` (opt-in) by `MEM_BACKEND`, upcasting any of them to `Arc<dyn Backend>` — the service/worker layer is already backend-agnostic. Semantic recall uses `Array(Float32)` columns + `cosineDistance()` (brute force baseline; experimental `vector_similarity` HNSW index for scale); lexical recall uses a token/ngram data-skipping or experimental inverted index as a *candidate* channel; the two fuse with the **same Rust-side RRF** the Lance path uses (`pipeline::ranking::rrf_merge`). Graph BFS reuses the same Rust iterative hop-by-hop walk.

**Tech Stack:** Rust, the [`clickhouse`](https://crates.io/crates/clickhouse) crate (HTTP/native protocol, `serde`-derive row mapping, async), a Docker ClickHouse container for integration tests. No extension installs (vector/FTS are core or experimental ClickHouse features behind settings).

参照：`backend-coupling.md` §6 / §6.5（trait 抽离史 + 5 痛点），`postgres-backend.md`（同型 opt-in 后端 spike 的体例与 scaffold-without-validation 范式），`docs/remove-duckdb-keep-lance.md`（route-B 后读写都 lance-native 的现状）。

---

## 1. 背景与定位

**ClickHouse 后端解决什么**

- **OLAP / 分析规模的语料**：transcript 归档（`conversation_messages` + embeddings）天然是 append-heavy、列式扫描友好。一个百万级对话块的 mem 实例，ClickHouse 的列存 + 压缩 + 并行扫描在「全表过滤 / 聚合 / 大范围 range」上远超 Lance 单机。
- **已有 ClickHouse 基建的团队**：和 Postgres backend 同理——想把 mem 的记忆/转录塞进既有 OLAP 仓、跟其它数据一起 join/分析，而不是再单独运维一套存储。
- **大批量回填**：`mem mine` 批量灌历史 transcript 时，ClickHouse native block insert 吞吐极高。

**ClickHouse 后端【不】解决什么**

- **不替代 Lance 默认**：小型单机、低延迟点查点写仍是 Lance 的主场。ClickHouse 不是「drop-in 低延迟点写存储」——它的强项是批量 append + 列扫描，弱项恰是 mem 生命周期里高频的**逐行 UPDATE**（decay / last_used_at / status / supersede / 软删）。
- **不是单机零运维**：和 Lance 的「零外部服务」不同,ClickHouse 要一个 running server(单机 `clickhouse-server` 也行,但终究是个进程)。
- **首版不进生产**：与 Postgres spike 一致——scaffold + feature-gate + 标 UNVALIDATED;真实性能/正确性以 §7 的 validation 清单跑过为准。

**定位一句话**：ClickHouse 是给「转录归档已经 OLAP 化、或想把 mem 落进既有 CH 仓」的 opt-in 选择;default 用户(Lance)完全无感、零变化。

---

## 2. 插拔机制：Backend 伞 trait + blanket impl

mem 的存储插拔不靠工厂 if/else,靠一个**伞 supertrait + blanket impl**(`src/storage/backend.rs`):

```rust
pub trait Backend:
    CapsuleStore + CapsuleSearchStore + EmbeddingJobStore + EmbeddingVectorStore
    + GraphStore + TranscriptStore + EntityRegistry + SessionStore
    + MaintenanceStore + MineCursorStore + EvolutionCandidateStore
    + Send + Sync + 'static {}

impl<T> Backend for T where
    T: CapsuleStore + CapsuleSearchStore + … + EvolutionCandidateStore + Send + Sync + 'static {}
```

`Backend` 自身**没有任何方法**——它纯粹是 11 个子 trait 的别名。任何把这 11 个全 impl 了的具体类型(今天 `Store`=Lance;`PostgresBackend`;明天 `ClickHouseBackend`)**自动**就是 `Backend`,无需手写 `impl Backend`。service/worker 层统一持 `Arc<dyn Backend>`,对后端类型无感。

**要实现的 11 个子 trait(及当前 trait 方法面)** — 文件均在 `src/storage/*.rs`:

| # | 子 trait | 文件 | 方法数¹ | ClickHouse 实现要点 |
|---|---|---|---|---|
| 1 | `CapsuleStore` | `capsule_store.rs` | 20 | 胶囊 CRUD + 生命周期(insert/get/list/accept/reject/supersede/feedback/delete/stats/taxonomy)。update-heavy → §4(a) ReplacingMergeTree |
| 2 | `CapsuleSearchStore` | `capsule_search_store.rs` | 9 | 混合召回(bm25+ann+RRF)、`search_candidates` lifecycle pool、`recent_active`、fetch-by-ids、version 链 |
| 3 | `EmbeddingJobStore` | `embedding_job_store.rs` | 18 | 嵌入任务队列 claim/complete/fail/stale。`claim_next_n` 见 §4(b)/痛点队列 |
| 4 | `EmbeddingVectorStore` | `embedding_vector_store.rs` | 8 | capsule + conversation 向量 upsert/get/delete(含 chunk 多行) |
| 5 | `GraphStore` | `graph_store.rs` | 14 | `graph_edges` 双时态(valid_from/valid_to)、`neighbors_within` BFS、tunnels、stats |
| 6 | `TranscriptStore` | `transcript_store.rs` | 12 | 转录 CRUD + session/range 读 + bm25/semantic 候选 |
| 7 | `EntityRegistry` | `entity_registry.rs` | 5 | `resolve_or_create` / `add_alias` / `lookup_alias` / `get_entity` / `list_entities` |
| 8 | `SessionStore` | `session_store.rs` | 6 | session touch/open/close + 最近活跃 + episodes 列 |
| 9 | `MaintenanceStore` | `maintenance_store.rs` | 6 | vacuum/索引维护。CH 无 Lance manifest 概念 → no-op + `OPTIMIZE TABLE … FINAL` 语义说明 |
| 10 | `MineCursorStore` | `mine_cursor_store.rs` | 2 | `mem mine` 游标 upsert/get |
| 11 | `EvolutionCandidateStore` | `evolution_candidate_store.rs` | 2 | 自进化候选 upsert/list |

> ¹ 方法数 = 当前各 trait 体内 `async fn` 声明数(parent 从源码核对)。`postgres-backend.md` §P5 给出的旧估值(GraphStore 28 / TranscriptStore 24 / …)是更早 trait 面或不同计数口径,**以本表当前数为准**;总计约 **102 个方法**要 impl。验收口径:每个 trait 一份 parity 测试,「与 Lance 同行为」为通过线。

**Store 专有胶水(与 Postgres spike 同坑)**:`app.rs` 在具体 `Store` 上直接调了若干**非 `Backend` trait 方法**(如 `set_transcript_job_provider` / `bump_last_used_at` / `potentiate_edge`)。换后端时同样要处理(P2):倾向把这些提升为 trait 方法(CH 给等价实现),而非 app.rs 按枚举分支。

---

## 3. 文件级改动清单(照 Postgres 的 ~6 处)

| 改动点 | Postgres 现状 | ClickHouse 照搬 |
|---|---|---|
| `Cargo.toml` | `[features] postgres = ["dep:sqlx", …]` + optional `sqlx`/pgvector | `clickhouse = ["dep:clickhouse"]` feature + `clickhouse = { version = "…", optional = true }`。默认构建不拉 |
| `src/config.rs` | `BackendKind { Lance, Postgres }` + `parse_backend`(读 `MEM_BACKEND`)+ `MEM_POSTGRES_URL` | 加 `BackendKind::Clickhouse` + `parse_backend` 多一分支(`"clickhouse"`)+ 读 `MEM_CLICKHOUSE_URL`。非法值/缺 URL/未编 feature → 启动清晰报错 |
| 新模块 `src/storage/clickhouse_store/` | `postgres_store/{mod,backend,capsule_store,…}.rs` | `clickhouse_store/{mod,backend,capsule_store,search,embedding_vector,graph,transcript,…}.rs`:`ClickHouseBackend` 持 `clickhouse::Client`,逐文件 impl 各子 trait,blanket impl 自动生效 |
| `src/storage/mod.rs` | `#[cfg(feature="postgres")] mod postgres_store;` | `#[cfg(feature="clickhouse")] mod clickhouse_store;` + re-export `ClickHouseBackend` |
| `src/app.rs` | `from_config` 按 `BackendKind` 分支建 `Arc<dyn Backend>`(lance / postgres) | 多一分支:`Clickhouse => Arc::new(ClickHouseBackend::connect(url).await?)`。胶水方法同 P2 决策 |
| `migrations/clickhouse/` + CI + tests | `migrations/postgres/000X_*.sql` + 可选 CI `postgres` job + `tests/postgres_backend.rs`(门控 `MEM_TEST_POSTGRES_URL`) | `migrations/clickhouse/000X_*.sql`(纯 DDL,启动期 `ensure` 或测试期 apply)+ 镜像 CI `clickhouse` job + `tests/clickhouse_backend.rs`(门控 `MEM_TEST_CLICKHOUSE_URL`,未设 skip) |

**关键不变量**:默认 `cargo build` / `cargo test`(无 `--features clickhouse`)行为**完全不变**——新代码全在 `#[cfg(feature="clickhouse")]` 之后,`clickhouse` crate 只在 feature 开时进依赖图。

---

## 4. ClickHouse 特有设计决策(核心 —— 与 Postgres 的本质差异)

mem 的存储是 **update-heavy 生命周期**(decay 每小时刷 `decay_score`、检索刷 `last_used_at`、review 改 `status`、supersede 写版本链、`incorrect`→软删 Archived)。ClickHouse 是 **OLAP / append-optimized**:逐行 `UPDATE`/`DELETE` 是**异步 mutation**(`ALTER TABLE … UPDATE/DELETE`),重写整个 part、延迟以秒~分计,**绝不能放在热路径**。这是 CH 后端的核心张力,下面逐条定调。

### (a) 用版本化 insert + ReplacingMergeTree 取代逐行 UPDATE —— **首选方案**

**结论:所有「更新」建模成带 `row_version` 的重新 insert,引擎用 `ReplacingMergeTree(row_version)`,读时取每个 PK 的最新版本。** 这把 mem 的 update-heavy 生命周期翻译成 CH 最擅长的 **append-only insert**,彻底绕开热路径 mutation。

- **引擎**:`ReplacingMergeTree(row_version)`,`ORDER BY (tenant, capability_capsule_id)`(= 去重键)。同一胶囊每次状态变化(decay/status/last_used/feedback)= insert 一行新版本,`row_version` 单调递增(用写入时刻 ms,或专门的 UInt64 计数)。
- **读取**:取最新版本两条路 ——
  - `SELECT … FROM t FINAL WHERE …`:`FINAL` 让 CH 读时合并、只返回每键最新行。简单,但有 merge-on-read 成本(§8)。
  - 或 `argMax(col, row_version) … GROUP BY pk`:把「取最新」写进聚合,避开 `FINAL` 全表语义,常更快。**推荐读热路径用 argMax 模式,跨表 join/调试用 `FINAL`。**
- **软删**:用 `is_deleted UInt8` 列(ReplacingMergeTree 的 `is_deleted` 参数,CH 23.x+)或状态列 `status='Archived'`——Archived 走和别的状态变更一样的版本化 insert,**永不物理删**(契合 mem 的 verbatim/可逆纪律)。
- **为什么不用 CollapsingMergeTree**:Collapsing 需要成对的 `sign=+1/-1` 行来「抵消」旧状态,调用方要先读旧行再写一对——对 mem 这种「我只知道新状态」的任意字段更新很别扭。ReplacingMergeTree(version) 只需写新行 + 一个递增版本,语义直观。`VersionedCollapsingMergeTree` 是退路,不首选。
- **物理回收**:旧版本行靠后台 merge 自然合并掉;偶尔 `OPTIMIZE TABLE … FINAL` 强制(`MaintenanceStore` 的 CH 实现,见 §9-trait)——这是 CH 版的「vacuum」,语义≠Lance manifest 修剪,文档注明。

### (b) 逐行 UPDATE/DELETE = 异步 mutation —— 仅用于罕见/管理操作

`ALTER TABLE … UPDATE/DELETE` 留给**非热路径**:`delete_capability_capsule_hard`(admin 硬删)、schema 级修复。热路径的 decay sweep / status 流转 / last_used 全走 (a) 的版本化 insert。嵌入任务队列的 claim(高频更新 `status='processing'`)也建模成版本化 insert(insert 一行 `status=processing,row_version++`,读时 argMax)——避免 mutation。

### (c) 无真事务 → 原子性契约(直接对接 Postgres spike Pain #4)

ClickHouse **没有跨语句事务**(单 insert 的 block 是原子的,跨表/跨语句不是)。`replace_pending_with_successor`(archive 旧 + insert 新)和 `apply_feedback`(写 `feedback_events` + 改胶囊行)在 CH 下是**两次独立 insert**,中间 crash 可能留中间态。

这正好命中 Pain #4 的既定决策:trait doc 已在这两个方法上**显式 spec「NOT atomic across backends」**——caller 必须按「可能观察到部分状态」写。CH 后端**直接依赖这个契约**,无需发明事务模拟(本仓明确拒绝在应用层模拟 rollback)。**本设计不要求改 trait;CH 是「Pain #4 契约为什么必须是非原子」的第三个实证**:Lance 无事务、CH 无事务,只有 Postgres 有——把契约定成最弱后端(非原子)是对的。

### (d) 向量 ANN

- **基线(parity-safe)**:`embedding Array(Float32)` 列 + `cosineDistance(embedding, :query)` 暴力扫 + `ORDER BY dist ASC LIMIT k`。对小表(capsule 嵌入)和 Lance 的 flat-scan 行为一致,直接可比。
- **规模(experimental)**:CH 24.x+ 的 `vector_similarity` 索引(USearch-backed HNSW,`SET allow_experimental_vector_similarity_index=1`)给大表(`conversation_message_embeddings`)上近似 ANN。**标 experimental**:不同 CH 版本可用性/语义有差异,validation 阶段实测。
- **dim 与列**:embedding dim provider-dependent(默认 1024)。`Array(Float32)` 不强制固定长度,但建索引时要;与 Lance lazy-create 对齐——首次 upsert 时按 provider dim 建表/索引,换 provider 改 dim 需重建(文档注明)。
- **chunk 折叠**:一条消息可能 N 个 chunk 向量(多行同 `message_block_id`)。CH 侧 `GROUP BY message_block_id` 取 `min(dist)`(= Lance 的 chunk-collapse MIN `_distance`)。

### (e) FTS / BM25 parity —— 最难的一块

ClickHouse **没有 BM25 评分**。可用的是:

- `tokenbf_v1` / `ngrambf_v1` **data-skipping 索引**:只做**预过滤**(「这个 part 可能含 token」),**不产生 rank 分数**。
- 实验性**倒排/全文索引**(`full_text` / `inverted`,`SET allow_experimental_full_text_index=1`,各版本名字/能力不同):给候选生成,仍非 Tantivy 的 BM25 分。
- **中文**:CH 的分词不做 jieba——和 Lance 的 jieba-Tantivy 分词器有本质 gap;CJK 召回会弱。

**结论**:CH 的 FTS 只能当**粗粒度词法候选通道**,无法逐分复刻 Tantivy/lance BM25。因此:

1. 用 token/ngram 或实验性倒排索引**生成词法候选 id 集**;若需要 rank,在 **Rust 侧**对候选算一个 BM25-ish 排序(或直接用 CH 返回顺序),再喂统一的 `rrf_merge`。
2. **parity 只能软验**:复用现有 FTS golden 的**软口径**(overlap@10 ≥ 0.8 + golden ⊆ ch,见 `tests/parity_golden.rs` 的 `assert_golden_soft`),**不**做逐分相等。把 CJK/分词差异记为**已知 parity gap**,语义侧(向量 ANN)兜底,不阻塞 scaffold。

### (f) 图 BFS —— 照搬 Rust 逐跳

`neighbors_within` 等多跳查询**不**用 SQL 递归(CH 的递归 CTE 支持有限/实验性)。**照搬 Lance/route-B 的做法**:Rust 侧迭代 BFS,每跳一条 `SELECT … WHERE from_node_id IN (…) AND valid_to IS NULL`(或点时 `valid_to > as_of`),`MAX_HOPS_CAP=3`。和 Postgres 用 `WITH RECURSIVE` 不同——CH 这里反而和 Lance 同构,实现可大量复用 `lance_store/graph.rs` 的 Rust BFS 形态。

### (g) 批量写 —— CH native block insert

`insert_capability_capsules` / `create_conversation_messages` / `mem mine` 批量回填走 **CH 原生 block insert**(`clickhouse` crate 的 `insert()` + 多行 `write`),一次网络往返灌一个 block——CH 吞吐最高的路径,天然契合 Pain #2(批量 insert 各后端自己优化)。

---

## 5. 复用 Postgres spike 的 5 痛点在 ClickHouse 下如何体现

`backend-coupling.md` §6.5 的 5 痛点(均已在 Lance/Postgres 下 resolved),在 CH 下逐条:

| Pain | Postgres 下 | ClickHouse 下如何体现/处理 |
|---|---|---|
| **#1 `version: u64`↔i64** | 已把 domain `version` 改 `i64`(根治) | 不复现:`version` 现在是 `i64` → CH `Int64`,直接映射。**更重要的连带纪律**:mem 所有时间戳是 **20 位零填充 ms 字符串**(对齐 trait `&str` 面)→ CH 一律 **`String`**,**不**用 `DateTime`/`DateTime64`(否则要在边界转换、且破坏 `&str` 契约)。lexicographic 比较 = 时间序,range/cursor 直接可用 |
| **#2 批量 insert** | trait 无真批量路径,各后端自己优化 | CH = native block insert(§4g),本就是 CH 强项;比 Postgres 的多值 INSERT 更顺 |
| **#3 动态 SET COALESCE** | `apply_feedback` 用 `SET col=COALESCE($N,col)` 合并 4 个 SQL variant | CH mutation **也不支持 COALESCE 式部分更新**,且热路径不该用 mutation。CH 解法是 §4(a):状态变更 = **整行版本化 insert**(把当前行读出、改要改的字段、连同未变字段一起 insert 新版本)→ 根本不需要「部分 SET」。这等价于 mem decay sweep 已经在 Lance 上做的「两条 WHERE 互斥 UPDATE」拆分思路——CH 这里是「读改写整行版本」 |
| **#4 原子性契约** | trait doc spec「NOT atomic」,Postgres 可用真事务但 caller 不依赖 | CH **无事务**,`replace_pending_with_successor`/`apply_feedback` 是两次 insert,**直接落在非原子契约上**(§4c)。CH 是这条契约「必须最弱」的第三实证,无需改 trait |
| **#5 `FeedbackSummary.auto_promoted` 槽** | 已加 `auto_promoted: u64` 字段 + 3 后端 aggregator 路由 | 不复现结构问题,但 CH `feedback_summary` 的聚合实现**必须照样路由** `auto_promoted` 这第 6/7 种 kind(`countIf(feedback_kind='auto_promoted')` 等),并加一份 parity test 验证与 Lance 计数一致 |

---

## 6. DDL 草案(以现有 lance schema 列为准)

时间戳列一律 `String`(20 位零填充 ms 串);枚举列 `LowCardinality(String)`;向量 `Array(Float32)`;字符串列表(evidence/code_refs/tags/topics/steps/member_ids…)`Array(String)`。生命周期表用 `ReplacingMergeTree(row_version)`,纯 append/不可变表用 `MergeTree`,队列表见 §4(b)。`row_version UInt64` 是每张 update-heavy 表新增的版本列(写入 ms 或递增计数)。

```sql
-- 胶囊主表(update-heavy:status/decay/last_used/supersede 全版本化 insert)
CREATE TABLE capability_capsules (
  capability_capsule_id String, tenant String,
  capability_capsule_type LowCardinality(String), status LowCardinality(String),
  scope LowCardinality(String), visibility LowCardinality(String),
  version Int64, summary String, content String,
  evidence Array(String), code_refs Array(String),
  project String, repo String, module String, task_type String,
  tags Array(String), topics Array(String),
  confidence Float32, decay_score Float32,
  content_hash String, idempotency_key String, session_id String,
  supersedes_capability_capsule_id String, source_agent String,
  created_at String, updated_at String, last_validated_at String,
  last_used_at String, last_recalled_at String, expires_at String,
  row_version UInt64
) ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, capability_capsule_id);
-- 读最新:argMax(col,row_version) … GROUP BY tenant,capability_capsule_id  (热路径)
-- 或 SELECT … FROM capability_capsules FINAL  (调试/跨表)

-- 反馈事件(append-only,不可变 → 普通 MergeTree)
CREATE TABLE feedback_events (
  feedback_id String, capability_capsule_id String,
  feedback_kind LowCardinality(String), created_at String, note String
) ENGINE = MergeTree ORDER BY (capability_capsule_id, created_at);

-- 胶囊嵌入(dim provider-dependent;ANN = cosineDistance 暴力 / 实验性 vector_similarity)
CREATE TABLE capability_capsule_embeddings (
  capability_capsule_id String, tenant String,
  embedding_model LowCardinality(String), embedding_dim Int32,
  embedding Array(Float32), content_hash String,
  source_updated_at String, created_at String, updated_at String,
  row_version UInt64
) ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, capability_capsule_id, content_hash);
-- 可选: INDEX ann embedding TYPE vector_similarity('hnsw','cosineDistance') GRANULARITY 1 (experimental)

-- 转录归档(append-heavy,OLAP 主场 → MergeTree;按 session+时序排)
CREATE TABLE conversation_messages (
  message_block_id String, session_id String, tenant String,
  caller_agent String, transcript_path String,
  line_number Int64, block_index Int64, message_uuid String,
  role LowCardinality(String), block_type LowCardinality(String),
  content String, tool_name String, tool_use_id String,
  embed_eligible UInt8, created_at String, meta_json String
) ENGINE = MergeTree
ORDER BY (tenant, session_id, created_at, line_number, block_index);
-- 可选词法预过滤: INDEX content_tok content TYPE tokenbf_v1(...) GRANULARITY 1

-- 转录嵌入(同胶囊嵌入策略)
CREATE TABLE conversation_message_embeddings (
  message_block_id String, tenant String,
  embedding_model LowCardinality(String), embedding_dim Int32,
  embedding Array(Float32), content_hash String,
  source_updated_at String, created_at String, updated_at String,
  row_version UInt64
) ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, message_block_id, content_hash);

-- 嵌入任务队列(claim/complete 高频更新 → 版本化 insert,读 argMax 取最新 status)
CREATE TABLE embedding_jobs (
  job_id String, tenant String, capability_capsule_id String,
  target_content_hash String, provider LowCardinality(String),
  status LowCardinality(String), attempt_count Int64, last_error String,
  available_at String, created_at String, updated_at String,
  row_version UInt64
) ENGINE = ReplacingMergeTree(row_version) ORDER BY (job_id);
-- transcript_embedding_jobs 同构(message_block_id 替 capability_capsule_id)

-- 图边(双时态:close = 写新版本把 valid_to 填上)
CREATE TABLE graph_edges (
  from_node_id String, to_node_id String, relation LowCardinality(String),
  valid_from String, valid_to String, confidence Float32,
  extractor LowCardinality(String), strength Float32, stability Float32,
  last_activated String, access_count Int64, row_version UInt64
) ENGINE = ReplacingMergeTree(row_version)
ORDER BY (from_node_id, relation, to_node_id, valid_from);
-- BFS 逐跳: WHERE from_node_id IN (…) AND valid_to = ''  (空串=活动边)

-- 实体注册表
CREATE TABLE entities (
  entity_id String, tenant String, canonical_name String,
  kind LowCardinality(String), created_at String
) ENGINE = MergeTree ORDER BY (tenant, entity_id);
CREATE TABLE entity_aliases (
  tenant String, alias_text String, entity_id String, created_at String
) ENGINE = ReplacingMergeTree  -- alias→entity 可重绑;PK 去重
ORDER BY (tenant, alias_text);

-- 会话(touch/close 更新 → 版本化)
CREATE TABLE sessions (
  session_id String, tenant String, caller_agent String,
  started_at String, last_seen_at String, ended_at String,
  goal String, memory_count Int64, row_version UInt64
) ENGINE = ReplacingMergeTree(row_version) ORDER BY (session_id);

-- episodes(append-only)
CREATE TABLE episodes (
  episode_id String, tenant String, goal String,
  steps Array(String), outcome String, evidence Array(String),
  scope LowCardinality(String), visibility LowCardinality(String),
  project String, repo String, module String, tags Array(String),
  source_agent String, idempotency_key String,
  created_at String, updated_at String, workflow_candidate UInt8
) ENGINE = MergeTree ORDER BY (tenant, episode_id);

-- mine 游标(高频 upsert → 版本化)
CREATE TABLE mine_cursors (
  transcript_path String, last_line_number Int64, updated_at String, row_version UInt64
) ENGINE = ReplacingMergeTree(row_version) ORDER BY (transcript_path);

-- 自进化候选(累积 evidence → 版本化)
CREATE TABLE evolution_candidates (
  candidate_id String, tenant String, op_kind LowCardinality(String),
  member_ids Array(String), params String, evidence Float32,
  consecutive_cycles Int64, status LowCardinality(String),
  first_proposed_at String, last_signal_at String, executed_at String,
  result_capsule_ids Array(String), row_version UInt64
) ENGINE = ReplacingMergeTree(row_version) ORDER BY (candidate_id);
```

> **覆盖**:14 张表 —— capability_capsules、feedback_events、capability_capsule_embeddings、conversation_messages、conversation_message_embeddings、embedding_jobs、transcript_embedding_jobs(注释带过)、graph_edges、entities、entity_aliases、sessions、episodes、mine_cursors、evolution_candidates。列集均取自当前 `lance_store/*.rs` 的 `*_schema()`。`active edge = valid_to IS NULL` 在 CH 用空串 `''` 表达(`String` 列无 NULL 习惯;或用 `Nullable(String)`,P5 定夺并与 BFS 谓词一致)。

---

## 7. 落地路线:scaffold-without-validation 范式

和 Postgres spike 同范式 —— **先 scaffold 全套、feature-gate、默认零变化、显式标 UNVALIDATED、列 pain inventory**,再按清单升 validated。

### 阶段拆分(P1–P6,镜像 postgres-backend.md)

- **P1 — 测试基建 + CapsuleStore scaffold**:`tests/clickhouse_backend.rs`(读 `MEM_TEST_CLICKHOUSE_URL`,未设 skip)+ apply `migrations/clickhouse/0001`(capability_capsules + feedback_events)+ `ClickHouseBackend` 的 `CapsuleStore` impl(版本化 insert + argMax 读)。复用 `capsule_store_parity.rs` 的 13 场景跑 CH。验收:CH parity 绿;默认 `cargo test`(无 feature)回归绿。
- **P2 — `BackendKind::Clickhouse` 选择 + 装配 + Store 胶水**:config.rs / app.rs 分支;其余 10 trait 先 `unimplemented!()` 占位让 `impl Backend` 编译过。验收:`MEM_BACKEND=clickhouse MEM_CLICKHOUSE_URL=… mem serve` 能启、`/health` ok;默认 Lance 零变化。
- **P3 — `EmbeddingVectorStore` + 嵌入表**:`Array(Float32)` + `cosineDistance`;chunk GROUP BY 折叠。验收:ANN 最近邻 parity(topk 顺序/折叠)绿。
- **P4 — `CapsuleSearchStore` 混合召回**:词法候选(token/ngram 或实验倒排)+ 向量候选 → **Rust 侧 RRF**(复用 `pipeline::ranking::rrf_merge`)。验收:召回**软 parity**(overlap@10≥0.8;FTS 差异记为已知 gap)。
- **P5 — 其余 8 trait**:Graph(Rust BFS)/Transcript/EmbeddingJob(版本化 claim)/Entity/Session/Maintenance(`OPTIMIZE … FINAL`/no-op)/MineCursor/Evolution,逐 trait TDD+parity+commit。
- **P6 — 端到端 + CI + 文档**:`tests/clickhouse_backend.rs` 跑 ingest→search→feedback→graph→transcript 全链路;CI `clickhouse` job(`services: clickhouse/clickhouse-server` + `cargo test --features clickhouse`,门控同 P1);README「Storage backends」加第三列;`backend-coupling.md` 标 CH spike;CLAUDE.md 加 `MEM_CLICKHOUSE_URL`。

### scaffold→validated 清单

- [ ] Docker:`clickhouse/clickhouse-server`(单机)起在 `127.0.0.1:8123`(HTTP)/`9000`(native);连接串 `MEM_CLICKHOUSE_URL`。
- [ ] 集成测试 `tests/clickhouse_backend.rs` 门控 `MEM_TEST_CLICKHOUSE_URL`(未设 skip → 默认 CI 绿不受影响)。可选 testcontainers 起临时 CH。
- [ ] parity 测试套件:复用 `tests/parity_golden.rs` 的**同一批 golden**(DuckDb 侧冻结的),CH 输出对非-FTS 桶走 exact、FTS/ANN 桶走 soft(overlap@10≥0.8)。
- [ ] CI job 镜像 postgres job:`services: clickhouse` + `cargo clippy --features clickhouse --all-targets -D warnings` + `cargo test --features clickhouse --test clickhouse_backend`。
- [ ] pain inventory 落 `backend-coupling.md`:CH 实测后把 §4/§5 的每条「未验证假设」标 ✅/⚠️(尤其 FINAL/argMax 性能、experimental 索引可用性、mutation 延迟)。

---

## 8. 风险与未决

- **性能未实测**:整份文档**从未在真实 ClickHouse 上跑过**(同 Postgres spike 初版)。所有引擎选型/读写形态是设计假设,validation 阶段可能推翻。
- **FINAL / argMax 读成本**:ReplacingMergeTree 的「读时取最新」有 merge-on-read 开销;高频点查胶囊(get_capability_capsule)是否够快是**头号未决**。缓解:合理 `ORDER BY`、`do_not_merge_across_partitions_select_final`、或对超热点查走主键 + LIMIT 1 + argMax。若点查太慢,CH 可能根本不适合 capsule 主表(只适合 transcript OLAP 侧)——这是 spike 要回答的核心问题。
- **mutation 延迟**:任何落到 `ALTER … UPDATE/DELETE` 的路径(硬删/修复)延迟以秒~分计;必须确保热路径零 mutation(§4a/b)。
- **parity gap(FTS/ANN)**:CH 无 BM25、无 jieba,词法召回对 CJK 弱,**逐分 parity 不可能**——只能软验 + 语义兜底,记为已知差异(§4e)。experimental `vector_similarity` 各版本能力不一,可能 fallback 到暴力 cosineDistance。
- **`clickhouse` rust crate 成熟度**:HTTP/native 协议覆盖、`serde` 行映射、async/连接池、错误面 vs `sqlx` 的成熟度待评估;可能要在 storage 层补绑定体操(类比 Pain #3)。
- **`active edge = valid_to IS NULL`**:CH `String` 列无 NULL 惯例,用空串还是 `Nullable(String)` 影响所有图谓词与 BFS,P5 统一定夺。
- **从不进生产**:与 Postgres backend 一致——本计划交付的是**可编译、feature-gated、parity-可验**的 scaffold,生产化是 validation 跑通 + 性能达标之后的独立工作。

---

## 9. 里程碑表

| P# | 交付 | 验收 |
|---|---|---|
| P1 | 测试基建 + CapsuleStore scaffold(版本化 insert + argMax)| CapsuleStore parity 绿;默认 cargo test 回归绿 |
| P2 | `MEM_BACKEND=clickhouse` 选择 + 装配 + 胶水 | `mem serve` 启到 CH、/health ok;默认 Lance 零变化 |
| P3 | EmbeddingVectorStore(Array(Float32)+cosineDistance)| ANN 最近邻 parity 绿 |
| P4 | 混合 CapsuleSearchStore(词法候选 + 向量 + Rust RRF)| 召回软 parity(overlap@10≥0.8)绿 |
| P5 | 其余 8 trait(graph Rust-BFS / transcript / jobs / entity / session / maint / cursor / evolution)| 各自 parity 绿,逐 trait commit |
| P6 | 端到端 mem serve on CH + CI + 文档 | e2e 全链路绿;Lance 回归绿;pain inventory 落 backend-coupling.md |

---

## 10. P1 实测 pain inventory(scaffold landed 2026-06-25)

P1(CapsuleStore + 测试基建)已落地:`cargo build` / `clippy --all-targets` 默认绿 + `--features clickhouse` build/clippy 绿 + 默认 `cargo test` 回归绿(36 binary / 0 失败);CapsuleStore 全 impl(`accept_pending`/`reject_pending` 走 trait 默认实现,故 18 个非默认方法体);仍 **UNVALIDATED**(无真实 ClickHouse)。实测撞到的 pain:

1. **RowBinary enum 映射**:`clickhouse` 0.15 的 RowBinary 把 serde enum 映射成与 `LowCardinality(String)` 不符的 repr → 每个枚举列改存 `String`,经 `serde_json` 转换(多一层;`enum_to_str` / `enum_from_str` helper)。
2. **无 `Nullable`(§6 设计)**:`Some("")` 与 `None` 不可区分 → 约定空串 = `None`(`opt()` helper),`''` round-trip 读成 `None`。坐实未决点 #2。
3. **Pain #4 原子性**:`apply_feedback` / `replace_pending_with_successor` 是两次独立 insert,CH 无事务 → 契约保持「NOT atomic」,**无需改 trait**(CH 是这条契约「必须最弱」的第三实证)。
4. **Pain #3(COALESCE / 动态 SET)**:被 §4(a)「读-改-整行版本化重插」消解——根本没有部分 `SET` 要表达。
5. **`row_version` = 墙钟 ms**:同一 ms 内两次写在 ReplacingMergeTree 下会 tie(per-process `AtomicU64` 可加固;未跑过,理论风险)。
6. **`parse_backend` 元组扩容**(2→3,加 `clickhouse_url`)波及 5 处调用含 4 个测试——小,但「config 形状即契约」。
7. **app.rs 无法 erase 成 `Arc<dyn Backend>`**(P1 只 impl 了 CapsuleStore)→ Clickhouse arm 两个 cfg 都 `return Err`;具体坐实伞 trait 的「全 11 或全无」耦合(完整装配 = P2)。
8. **删除级联 P1 只覆盖 2 表**(`capability_capsules` + `feedback_events`);embedding/job 卫星表的级联待其 schema 落地(P3)。
9. **读形态用 `FINAL`(非 argMax)**:P1 为简单正确统一用 `… FINAL`;`argMax(…) GROUP BY pk` 热路径优化注明留后(validation 测出 FINAL 太慢再切)。

### P2(2026-06-25):完整 Backend 装配 + 10 trait stub

P2 让 `ClickHouseBackend` impl 全 11 子 trait → blanket `impl<T> Backend for T` 成立 → 能 erase 成 `Arc<dyn Backend>`,`app::from_config` 真正 connect + `apply_migrations` + 装配。其余 10 trait 的方法体先 `unimplemented!("clickhouse-backend P#")` 占位(`src/storage/clickhouse_store/stubs.rs`,79 个必需方法)。门禁:默认 fmt+clippy+test 绿 + `--features clickhouse` build/clippy 绿。**仍 UNVALIDATED**:`mem serve` 能启到 CH、CapsuleStore 路径外的读写会 panic,直到 P3-P5 填实。

**10 个 stub 的 phase 标注:**

| 子 trait | 方法数(stub)| 真正实现 phase |
|---|---|---|
| `EmbeddingVectorStore` | 8 | **P3**(Array(Float32) + cosineDistance)|
| `CapsuleSearchStore` | 9 | **P4**(lexical + 向量 + Rust RRF)|
| `GraphStore` | 14 | **P5** |
| `TranscriptStore` | 12 | **P5** |
| `EmbeddingJobStore` | 18 | **P5** |
| `EntityRegistry` | 5 | **P5** |
| `SessionStore` | 6 | **P5** |
| `MaintenanceStore` | 3(必需)| **P5** |
| `MineCursorStore` | 2 | **P5** |
| `EvolutionCandidateStore` | 2 | **P5** |

**胶水方法处理(决策):** 沿用 Postgres arm 的「装配处按 backend 能力分支」模式,不提升为 trait 方法。CH arm **跳过** potentiation / last_used 两个 worker(它们调 `Store::potentiate_edge`/`bump_last_used_at`,是 lance-Store 级优化、非正确性);`set_transcript_job_provider` **推迟到 P5**(transcript 写本身是 stub,现在调它无意义,故不为它造 inherent stub);capsule 服务仍拿一个 `capsule_used_tx`、其 receiver 立即 drop(事件静默丢弃,同 Postgres)。

**P2 新 pain:**
10. **stub 必须精确匹配 trait 签名** —— 79 个方法逐字抄签名(含 `#[allow(clippy::too_many_arguments)]` 的 6 个宽参方法、`GraphError` vs `StorageError` 两种错误面);幸而一次编过(`unimplemented!()` 返 `!`,return type 不约束 body)。
11. **`MaintenanceStore` 的 3 个默认方法**(`vacuum_old_versions` / `ensure_query_indexes` / `rebuild_query_indexes`)**不要 stub** —— 它们的 trait 默认就是 non-Lance 的零-stats no-op(正确行为),stub 成 `unimplemented!()` 反而会让 vacuum worker panic。只 stub 3 个必需方法。
12. **`unimplemented!()` stub 是编译期完整、运行期会 panic** —— P2 交付的是「能 erase 成 Backend、`mem serve` 能启」,**不是**运行可用;CapsuleStore 外任何操作 abort。这是 scaffold 的预期状态(无真实 CH)。
13. **`from_config` 走到 connect 分支无法静态测** —— `apply_migrations` 需要活 CH;只能单测 `parse_backend`(`MEM_BACKEND=clickhouse`/`ch` + `MEM_CLICKHOUSE_URL` → `(Clickhouse, None, Some(url))`,缺 URL 报错,全词与 `ch` 别名都覆盖)。connect-分支的端到端验证留给 P6 的 `MEM_TEST_CLICKHOUSE_URL` 门控测试。

### P3(2026-06-25):EmbeddingVectorStore 实现 + migration 0002

P3 把 `EmbeddingVectorStore`(8 方法)从 stub 升为真实现(`src/storage/clickhouse_store/embedding.rs`),建嵌入表(`migrations/clickhouse/0002_embeddings.sql`)。stubs.rs 降到 9 个 trait(P4 search + P5 其余 8)。门禁:默认 fmt+clippy+test 绿 + `--features clickhouse` build/clippy 绿。仍 **UNVALIDATED**。

**关键设计决策:**
- **`chunk_index UInt32` 判别列**:一条 message 可有 N 个 chunk 向量(多行同 id)。`ORDER BY (tenant, id, chunk_index)` 让 N 行在 `ReplacingMergeTree` 下**不被折叠**成一行(否则同 `(tenant,id)` 键只剩一行)。一次 upsert 的 N 行共享一个 `row_version`(= 一代)。
- **blob↔Vec<f32>**:单条 upsert 收 `embedding_blob: &[u8]`(native-endian f32,见 `crate::embedding::wire`),用 `decode_f32_blob(blob, dim)` 解成 `Vec<f32>`;chunks 变体直接收 `&[Vec<f32>]`。CH 行结构 `embedding: Vec<f32>` ⇄ `Array(Float32)`(`clickhouse` 0.15 的 `Vec<T>`⇄`Array(T)` 映射,与 `ChCapsuleRow` 的 `Vec<String>`⇄`Array(String)` 同理,已验证编译)。
- **读取**:`get_*_vector`/`get_*_row` 走 `ORDER BY row_version DESC LIMIT 1`(胶囊侧单行)。chunk-set 的完整读(GROUP BY id 取 min 距离)是 P5 search 的事。
- **delete**:`ALTER TABLE … DELETE WHERE id = ?`(异步 mutation)。嵌入是派生数据、delete 罕见(胶囊硬删/重嵌),所以这里用 mutation 可接受——与热路径的生命周期写(版本化 insert,§4a)区别对待。
- **`embedding_dim` 用 `Int64`**(非 §6 草案的 `Int32`)对齐 trait 的 `i64` 参数 + lance schema 的 `Int64`。

**P3 新 pain:**
14. **`apply_migrations` 的注释跳过 bug(已修)**:原实现 `split(';')` 后 `trimmed.starts_with("--")` 整条跳过——但带前导 `--` 注释的语句,trimmed 以注释开头 → **整个 CREATE 被跳过**。gates 不跑 migration(无 CH)故没暴露。改成**逐行剥 `--` 注释行**再判空,并把 0001/0002 排进有序 `include_str!` 列表。
15. **「delete-once-then-insert」语义在 append 模型下的偏差**:lance 的 chunks upsert 是「先删该 id 所有行,再插 N 行」。CH 走纯 append(不在 upsert 路径上 mutation),代价:(a) 空 `vectors` upsert **不清旧行**(trait 说空=no-op,可接受);(b) chunk 数缩小(N<M)时旧的 M-N 行**物理残留**,靠读时 `row_version DESC LIMIT 1` / 后续 `OPTIMIZE`/TTL 清。记为 scaffold caveat,validation 阶段定夺是否要 per-upsert 的 `ALTER DELETE`。
16. **delete 是异步 mutation → 测试不能断言「删后立即不存在」**:`delete_*_embedding` 测试只断言不报错;删后缺失检查要等 mutation 落地,留给 validation。
17. **trait 不对称**:胶囊侧有 `get_row`/`get_vector`,conversation 侧只有 upsert+delete(无 get)。conversation chunk 测试只能验「upsert/delete 不报错」,完整 chunk-survival 验证靠 P5 search parity。
