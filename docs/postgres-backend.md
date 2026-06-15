# Postgres Backend Implementation Plan

> **For agentic workers:** phased plan, repo design-doc convention (cf. `evolution-worker.md`). Each phase is TDD, three-gate-green, committed with `… (closes postgres-backend P#)`. Default backend stays Lance+DuckDB; Postgres is opt-in.

**Goal:** Make `mem serve` able to run entirely on a single Postgres instance — a real peer to the Lance+DuckDB backend, not a degraded one — selected at runtime, with Lance+DuckDB remaining the default.

**Architecture:** A concrete `PostgresBackend` (an `sqlx::PgPool` wrapper) implements all 11 storage sub-traits, so the existing blanket `impl<T> Backend for T` applies unchanged. `app::AppState::from_config` chooses between `Store` (Lance+DuckDB) and `PostgresBackend` by `MEM_BACKEND`, upcasting either to `Arc<dyn Backend>` — the rest of the service/worker layer is backend-agnostic already. Semantic search uses **pgvector** (`<=>` cosine + HNSW index); lexical search uses Postgres `tsvector`/GIN; the two fuse with the same RRF the Lance path uses. The whole module is behind the `postgres` cargo feature so the default build never pulls `sqlx`.

**Tech Stack:** Rust, `sqlx` 0.8 (runtime-tokio, tls-rustls, postgres, uuid), **pgvector** Postgres extension, Docker Postgres for integration tests.

参照：`backend-coupling.md` §6 / §6.5（Phase 4 spike 的设计笔记与 5 痛点），`evolution-worker.md`（phase doc 体例）。

---

## 0. 现状与缺口

- ✅ `src/storage/postgres_capsule_store.rs`（791 行）实现了 **1/11** 子 trait：`CapsuleStore`（insert/get/list/feedback/stats/taxonomy/…），**从未跑过真实 PG**。
- ✅ `migrations/postgres/0001_capsule_store.sql`——仅 `capability_capsules` + `feedback_events` 两表。
- ✅ `tests/capsule_store_parity.rs`——同一组用例经 `Arc<dyn CapsuleStore>` 跑 Lance + InMemory；**Postgres 接进来即获得全套 CapsuleStore 验证**。
- ✅ `app.rs:115` 注释已预留"swap in a different backend"；装配点 = `store_concrete`。
- ❌ 缺：其余 10 个子 trait 的 PG 实现、`MEM_BACKEND` 选择、pgvector 接入、全表迁移、`mem serve` 跑 PG 的端到端验证。
- ⚠️ **Store 专有胶水**：`app.rs` 在 `store_concrete`（具体 `Store`）上直接调了 `set_transcript_job_provider` / `potentiate_edge`（worker clone）/ `bump_last_used_at` 等**非 Backend trait 方法**。换后端必须处理（P2 核心）。

## 1. 关键设计决策（已与用户确认）

1. **向量检索 = pgvector**（真 ANN，对等 Lance）。embeddings 列用 `vector(dim)`，建 HNSW 索引，距离用 `<=>`（cosine）。部署的 PG 实例需 `CREATE EXTENSION vector`。
2. **本轮做完整后端**：11 子 trait 全实现 + 选择 + 迁移 + 集成测试，`mem serve` 能真跑 PG。
3. **默认仍 Lance+DuckDB**：`MEM_BACKEND=lance`（默认）`| postgres`；`postgres` 时读 `MEM_POSTGRES_URL`（缺则 `DATABASE_URL`）。整模块在 `postgres` cargo feature 之后——默认构建不拉 sqlx。若 `MEM_BACKEND=postgres` 而二进制没编 `postgres` feature → 启动时清晰报错。
4. **schema 漂移沿用 spike 笔记**（0001 已定）：时间戳留 TEXT（20 位零填充 ms 串，对齐 trait `&str` 面）、枚举存 TEXT + CHECK（域枚举已是 snake_case 串）。新表沿用同口径，避免转换层。

## 2. 测试基建（贯穿所有 phase）

- **镜像**：`pgvector/pgvector:pg16`（内置 pgvector）。**拉取走 skopeo + `HTTPS_PROXY=http://172.31.169.108:2235`**（注意：与 git 用的 `42.192.60.90:31615` 是不同代理）：
  ```bash
  HTTPS_PROXY=http://172.31.169.108:2235 skopeo copy \
    docker://docker.io/pgvector/pgvector:pg16 \
    docker-daemon:pgvector/pgvector:pg16
  ```
- **起库**：`docker run -d --name mem-pg -e POSTGRES_PASSWORD=mem -e POSTGRES_DB=mem -p 5433:5432 pgvector/pgvector:pg16`，连接串 `postgres://postgres:mem@127.0.0.1:5433/mem`。
- **测试门控**：集成测试读 `MEM_TEST_POSTGRES_URL`——**未设则 skip**（`eprintln! + return`），所以默认 `cargo test`（无 PG）仍全绿、CI `rust` job 不受影响。设了才连真实 PG 跑。运行：`MEM_TEST_POSTGRES_URL=… cargo test --features postgres --test postgres_backend`。
- **parity 复用**：把 `capsule_store_parity.rs` 的 backend 列表扩展为可选第三项 `PostgresCapsuleStore`（仅当 `MEM_TEST_POSTGRES_URL` 设时纳入）。后续每个 trait 都加一份 parity 用例，PG 实现以"与 Lance 同行为"为验收。

## 3. 阶段拆分（P1–P6）

### P1 — 测试基建 + 验证现有 CapsuleStore scaffold（去险第一步）

把"791 行未测试"变成"已测试可用"，并打通 `--features postgres` 的测试通路。

- **新增** `tests/postgres_backend.rs`：起点是 PG 连接 helper（读 `MEM_TEST_POSTGRES_URL`，未设 skip）+ 自动 `sqlx::migrate!("migrations/postgres")` + 实例化 `PostgresCapsuleStore`。
- **复用** `capsule_store_parity.rs` 的用例（insert/get round-trip、tenant 隔离、status 流转、idempotency dedup、feedback、stats…）跑在 PG 上。
- **修 scaffold bug**：spike 从没跑过，预期有 SQL/绑定/类型错；逐个红→绿修。
- **迁移机制**：决定用 `sqlx::migrate!`（编译期校验）还是启动期 `ensure`；P1 先用 `sqlx::migrate!` 跑测试库。
- **验收**：`MEM_TEST_POSTGRES_URL=… cargo test --features postgres` 下 CapsuleStore 全部 parity 用例绿；默认 `cargo test`（无 feature/无 PG）仍 529/0。
- **commit**：`test(postgres): validate CapsuleStore scaffold against real pg (closes postgres-backend P1)`。

### P2 — 后端选择 + 装配 + Store 胶水处理

让 `mem serve` 能按 `MEM_BACKEND` 启到 PG（即便此时多数读路径还 unimplemented，先证明装配链路通）。

- **`PostgresBackend` 骨架**（`src/storage/postgres/mod.rs`）：持 `PgPool`；现有 `postgres_capsule_store.rs` 的 `CapsuleStore` impl 移到 `src/storage/postgres/capsule_store.rs`，`impl ... for PostgresBackend`。其余 10 trait 先 `unimplemented!()` 占位（仅为让 `impl Backend` 通过编译——P3–P5 逐个填）。
- **`config.rs`**：`MEM_BACKEND`（`lance`|`postgres`，默认 lance）+ `MEM_POSTGRES_URL`。非法值/缺 URL 报错。
- **`app.rs`**：按 backend 分支构建 `Arc<dyn Backend>`。**Store 胶水**两条路任选（P2 定夺）：(a) 把 `set_transcript_job_provider`/`bump_last_used_at`/`potentiate_edge` 提升为 trait 方法（PG 给等价/no-op 实现）；(b) app.rs 用枚举 `BackendHandle { Lance(Arc<Store>), Postgres(Arc<PostgresBackend>) }` 分支调用。倾向 (a)（更干净，worker 不需要知道后端类型）。
- **验收**：`MEM_BACKEND=postgres MEM_POSTGRES_URL=… mem serve` 能启动、`/health` ok（即便 search 还报未实现）；`MEM_BACKEND=lance`（默认）行为零变化（回归 529/0）。
- **commit**：`feat(postgres): MEM_BACKEND selection + PostgresBackend assembly (closes postgres-backend P2)`。

### P3 — pgvector：EmbeddingVectorStore + 嵌入迁移

- **迁移** `migrations/postgres/0002_embeddings.sql`：`CREATE EXTENSION IF NOT EXISTS vector;` + `capability_capsule_embeddings` / `conversation_message_embeddings`（`embedding vector(dim)`、`content_hash`、chunked 多行同 id），HNSW 索引 `USING hnsw (embedding vector_cosine_ops)`。dim 由 provider 决定——迁移用占位 dim 或建表时 `ALTER`；与 Lance 的 lazy-create 对齐（首次 upsert 建表/列）。
- **`src/storage/postgres/embedding_vector.rs`**：实现 16 个方法（upsert/get/delete capsule+chunk、conversation message 同款）。向量按 pgvector 文本/二进制绑定写入。
- **parity**：嵌入 upsert→get→cosine 最近邻，与 Lance 行为对齐（topk 顺序、chunk GROUP BY 去重）。
- **验收**：pgvector ANN 最近邻测试绿。**commit** `feat(postgres): pgvector EmbeddingVectorStore (closes postgres-backend P3)`。

### P4 — CapsuleSearchStore：混合召回（pgvector ANN + tsvector BM25 + RRF）

- **迁移** `0003_search.sql`：`capability_capsules` 加 `content_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', content)) STORED` + GIN 索引（BM25-equivalent；`simple` 配置避免英文 stemmer 吃掉中文/标识符——与 Lance 的 tantivy `simple`-ish 对齐，细节 P4 核）。
- **`src/storage/postgres/capsule_search.rs`**：18 个方法。`search_candidates` 走 pgvector `<=>` topk ∪ tsvector `@@` rank topk，两路 RRF（镜像 `pipeline/retrieve.rs` 的归一化/RRF，复用 `MEM_RANKER` 档）。pool_bound / guidance 豁免 / per-source cap 等保持与 Lance 同语义。
- **parity**：同一组 capsule + query，PG 与 Lance 的 top 命中集合一致（顺序允许小差异，命中集合不许差）。
- **验收**：混合召回 parity 绿。**commit** `feat(postgres): hybrid CapsuleSearchStore via pgvector + tsvector (closes postgres-backend P4)`。

### P5 — 其余 8 个 trait

逐个 TDD + parity，每个独立 commit：

- **GraphStore**（28）：`graph_edges` 表（`valid_from`/`valid_to`）；`neighbors_within` BFS 用**递归 CTE**（`WITH RECURSIVE`，`MAX_HOPS_CAP=3`）；时点查询 `valid_to IS NULL OR valid_to > as_of`；`invalidate_edge` / `close_edges_for_capability_capsule` / `graph_stats`。
- **TranscriptStore**（24）：`conversation_messages` + range/session 读；搜索侧若需嵌入则复用 P3 的 conversation embeddings。
- **EmbeddingJobStore**（队列）：`claim_next_n` 用 `SELECT … FOR UPDATE SKIP LOCKED`（PG 原生队列原语，比 DuckDB 干净）。
- **EntityRegistry**（10）/ **SessionStore**（12）/ **MineCursorStore**（4）/ **EvolutionCandidateStore**（4）：直 CRUD，UPSERT 用 `ON CONFLICT`。
- **MaintenanceStore**（9）：Lance 的 vacuum/manifest 在 PG 无对应——实现为 `ANALYZE` / no-op + 文档说明语义差异。
- **迁移** `0004_graph_transcripts_misc.sql` 等覆盖上述表。
- 每个 trait 一个 commit：`feat(postgres): <Trait> impl (closes postgres-backend P5.<n>)`。

### P6 — 端到端 + 文档

- **`tests/postgres_backend.rs`** 端到端：起 PG → `AppState::from_config(MEM_BACKEND=postgres)` → 经 HTTP 路由跑 ingest→search→feedback→graph→transcript 全链路（镜像 `search_api.rs` 的形态）。
- **CI**（可选）：加一个 `postgres` job——`services: postgres:pgvector` + `cargo test --features postgres`，门控同 P1。
- **文档**：README「Storage backends」节（lance 默认 / postgres 选项 + pgvector 前提 + 连接串）；`backend-coupling.md` §6.5 标 Phase 4 完成；`CLAUDE.md` 加 `MEM_BACKEND` / `MEM_POSTGRES_URL`。
- **验收**：`mem serve` 在 PG 上端到端绿；默认 Lance 路径回归全绿。**commit** `feat(postgres): end-to-end mem serve on postgres + docs (closes postgres-backend P6)`。

## 4. 风险与边界

- **dim 与 pgvector 列**：embedding 维度 provider-dependent（默认 1024）。pgvector 列必须固定 dim——P3 用 lazy-create（首次 upsert 时按 provider dim 建表/列），与 Lance 同策略；换 provider 改 dim 需重建表（文档注明）。
- **`simple` 文本配置**：PG 默认 `english` tsvector 会 stem，吃掉中文/代码标识符。用 `simple` 配置；中文分词 PG 原生弱（无 jieba），P4 评估是否够用，不够则语义侧（pgvector）兜底——记为已知差异，不阻塞。
- **schema lockstep**：本仓纪律是 Lance schema 改动要 schema fn + record_batch + parser 三处同步；PG 侧对应纪律 = 迁移 SQL + sqlx 绑定 + 行解析三处同步。每个 trait 的 parser 以 `CapabilityCapsuleRecord` 等域类型为准，跨后端复用同一反序列化路径（0001 的设计意图）。
- **不改默认行为**：每个 phase 验收都包含"默认 Lance 路径 529/0 回归"，确保 Postgres 工作零侵入既有用户。

## 5. 里程碑表

| P# | 交付 | 验收 |
|---|---|---|
| P1 | 测试基建 + CapsuleStore scaffold 真实 PG 验证 | parity（CapsuleStore）绿；默认 cargo test 529/0 |
| P2 | MEM_BACKEND 选择 + PostgresBackend 装配 + 胶水 | `mem serve` 启到 PG、/health ok；默认路径零变化 |
| P3 | pgvector EmbeddingVectorStore + 迁移 | ANN 最近邻 parity 绿 |
| P4 | 混合 CapsuleSearchStore（pgvector+tsvector+RRF） | 召回 parity 命中集合一致 |
| P5 | 其余 8 trait（graph/transcript/jobs/entity/session/maint/cursor/evolution） | 各自 parity 绿，逐 trait commit |
| P6 | 端到端 mem serve on PG + CI + 文档 | e2e 全链路绿；Lance 回归绿 |
