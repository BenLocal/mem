# mem 改造路线图

> 本文件是 [`mempalace-diff.md`](./mempalace-diff.md) 抽取出来的执行面板，便于单独维护与外发。
> **新增 / 调整路线图项请先回 `mempalace-diff.md` 改对应章节，再同步更新本文件。** 设计原则、每项的论证、不做的事见原文档对应章节号（§5、§7、§8、§11、§12、§13 等）。

## 路线图

| # | 层 | 项 | 价值 | 工作量 | 风险 | 触点 |
|---|---|---|---|---|---|---|
| 1 | ⚙️ | ✅ `embedding_jobs` dedupe 复核（实际已被 Mutex + 事务覆盖，仅修注释/文档）| 🔴 修并发 bug | S（0.5h） | 低 | `storage/duckdb.rs`、`db/schema/002_embeddings.sql` |
| 2 | 🔍 | ✅ 引入 `usearch` sidecar ANN（消除 `semantic_search_memories` 的 `limit 2000` 隐式截断）| 🟠 性能基础设施 + 🔴 修隐式正确性边界 | M（1–2 天） | 中（需要 repair 路径） | `Cargo.toml`、`storage/`、新增 `vector_index.rs` |
| 3 | ⚙️ | ✅ HNSW 健康度自检 + repair 子命令（`mem repair --check` / `--rebuild`，JSON 输出可选）| 🟠 配套 #2 | S（4h） | 低 | `src/cli/repair.rs`、`src/storage/vector_index_diagnose.rs` |
| 4 | 🔍 | ✅ 图边时序化（valid_from/to）+ 全量迁到 DuckDB（删 IndraDB 依赖与 GraphStore trait）| 🟠 表达力 | L（实际 ~1.5 天，scope 比原 M 大）| 中 | `domain/memory.rs`、`storage/graph_store.rs`、`db/schema/003_graph.sql`、`pipeline/ingest.rs` |
| 5 | 🔍 | ✅ 检索分数归一化 / 两路 RRF（lex+sem，`MEM_RANKER=legacy` 一档兜底）| 🟡 排序质量 | S（3h） | 低 | `pipeline/retrieve.rs` |
| 6 | 📦 | ✅ `compress_text` 改 token 计数（tiktoken-rs::o200k_base，CJK 不再按词裸奔截断）| 🟡 输出 verbatim 纪律 | S（2h） | 低 | `pipeline/compress.rs` |
| 7 | 🔍 | ✅ `decay_score` 后台衰减 worker（`f7a6c87` feat(service): add decay_worker for time-based memory decay；`62b2662` fix(decay): rewrite decay_worker SQL for DuckDB and ms-string timestamps） | 🟡 生命周期闭环 | S（3h） | 低 | `service/`、新增 `decay_worker.rs` |
| 8 | 🔍 | ✅ **Entity registry**（`entities` + `entity_aliases`；`MemoryRecord.topics`；ingest auto-promotes via `extract_graph_edge_drafts` + `resolve_drafts_to_edges`；4 HTTP routes `/entities`；`mem repair --rebuild-graph`；`8f8d2af..68395a0`） | 🟢 数据卫生 | M（半天） | 中（迁移） | `domain/entity.rs`、`storage/entity_registry.rs`、`db/schema/008_entity_registry.sql`、`src/http/entities.rs`、`src/cli/repair.rs` |
| 9 | 📦 | ✅ **Verbatim 守护**（caller 显式提供 `summary` 时校验 `summary != content`；AGENTS.md / README.md 已写） | 🟢 哲学一致性 | S（1h） | 低 | `pipeline/ingest.rs`、`AGENTS.md`、`README.md` |
| 10 | 🔍 | ✅ **Sessions**（schema + auto-bucket；HTTP 端点 / `episodes.session_id` / DELETE 软删延后） | 🟠 表达力 | M（半天） | 中（schema 迁移）| 新增 `sessions` 表 + `memories.session_id` + auto-bucket on ingest，详见 [§11](./mempalace-diff.md#11-sessions对齐-mempalace-的-room但和-episodes-正交) |
| 11 | 🔍 | ✅ **检索流水线 Stage 2 拆分**（lifecycle 加性层抽出共享 helper；Stage 3 / 响应形状变更延后） | 🟠 架构升级 | M（1 天 → 实际 ~2h，按 §12 line 513 降级路径） | 中（依赖 #2 + #5） | `pipeline/retrieve.rs` 重构、`pipeline/compress.rs` 输出层调整，详见 [§12](./mempalace-diff.md#12-检索流水线三段式重构取-mem-之结构化借-mempalace-之分层) |
| 12 | 🔍 | ✅ **Claude Code / Codex 无感集成包**（Stop/PreCompact/SessionStart hooks + `mem mine` 离线 miner + `mem wake-up`；`138eb2e` feat(integration): Claude Code seamless integration、`fd07564` feat(hooks)、`7c4860c` feat(mine)、`a013a77` feat(wake-up)、`d374b86` feat(transcripts): mem mine writes every block to /transcripts/messages） | 🟠 用户体验代际升级 | M（1.5 天） | 中（hook 脚本跨平台、跨 agent runtime） | 新增 `hooks/`、`src/cli/{mine,wake_up}.rs`、`README.md` 集成章节，详见 [§13](./mempalace-diff.md#13-claude-code--codex-无感集成包让用户毫无察觉成为产品特性) |

> **层（Layer）**：📦 = Verbatim 存储/输出纪律；🔍 = Structured 检索/排序/元数据；⚙️ = 基础设施 / 修 bug。
> **价值**：🔴 = 修 bug；🟠 = 架构升级；🟡 = 体验优化；🟢 = nice-to-have。
> **工作量**：S ≤ 4h，M = 0.5–2 天，L > 2 天。

> **建议批次**：
> - **批 A（修 bug）** ✅ 完成（sha2 迁移 + `embedding_jobs` dedupe 复核 / 注释澄清）。
> - **批 B（Verbatim 纪律落地，3h）** #9 + #6 + 文档化 — 把"设计原则"刻进代码。
> - **批 C（数据模型扩展，1 天）** #4 + #10 同期做，省一次 schema 迁移阵痛。
> - **批 D（检索现代化，2–3 天）** #2 + #3 + #5 → **#11**。前三项是 #11 的前置条件（先有 HNSW 召回 + RRF，再做三段式重构）。
> - **批 E（生命周期 / 卫生，半天–1 天）** #7 + #8。
> - **批 F（无感集成，1.5 天）** **#12**。前置依赖批 C（#10 Sessions），因为 wake-up 的 essential story 必须按 session 切片才不混乱。

---

> 维护建议：每完成一项，更新对应行的状态（✅ done / 🚧 in progress），并在 git commit message 里引用本文件中的编号（例如 `feat(retrieve): introduce usearch sidecar ANN (closes ROADMAP #2)`）。
