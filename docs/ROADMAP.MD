# mem 改造路线图

> 本文件是 [`mempalace-diff.md`](./mempalace-diff.md) 抽取出来的执行面板，便于单独维护与外发。
> **新增 / 调整路线图项请先回 `mempalace-diff.md` 改对应章节，再同步更新本文件。** 设计原则、每项的论证、不做的事见原文档对应章节号（§5、§7、§8、§11、§12、§13、§14 等）。"现在差异快照"看 [§15](./mempalace-diff.md#15-当前差异快照2026-05-06)。
>
> **状态（2026-05-07）**：#1–#15 全部 ✅；#16–#20 待办（KG 入图入口拓宽，触发条件：实测 KG 稀薄，平均每条 memory 仅 ~3 条边，tags / code_refs / supersedes / transcript-session 都没入图。详见 [§5.1](./mempalace-diff.md#51-实测-kg-不够稠密2026-05-07-抽样)）。

## 路线图

| # | 层 | 项 | 价值 | 工作量 | 风险 | 触点 |
|---|---|---|---|---|---|---|
| 1 | ⚙️ | ✅ `embedding_jobs` dedupe 复核（单进程被 `Arc<Mutex<Connection>>` + 事务覆盖，仅修注释/文档）⚠️ 多进程并发写同一 `.duckdb` 是另一类问题，2026-05-06 FK 循环事故已沉淀为 mem incident memory + worker 三层防御（open-time / claim-time / tick last-resort sweep）| 🔴 修并发 bug | S（0.5h） | 低 | `storage/duckdb.rs`、`db/schema/002_embeddings.sql` |
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
| 14 | 🔍 | ✅ **Recall quality bench (transcripts)**（10-rung ablation, synthetic+real fixtures, oracle-rerank upper bound; closes spec `2026-05-03-transcript-recall-bench-design`） | 🟡 决策基础设施 | M（半天） | 低 | `src/pipeline/eval_metrics.rs`, `tests/bench/`, `tests/recall_bench.rs` |
| 15 | 🔍 | ✅ **MemPalace LongMemEval parity bench** (Rust port of `longmemeval_bench.py`; 3 rungs map to mempalace `raw / rooms / full`; output mirrors `results_*.json` shape; closes spec `2026-05-05-mempalace-bench-parity-design`) | 🟡 横向对比基础设施 | M（半天） | 低 | `src/pipeline/eval_metrics.rs`, `tests/bench/longmemeval*.rs`, `tests/mempalace_bench.rs` |
| 16 | 🔍 | 🚧 **`tags` → `entity(kind=tag)` + `tagged` edge**（沿用 #8 entity registry 模式；ingest 时把 `tags[]` 路由通过 `EntityRegistry::resolve_or_create`，`mem repair --rebuild-graph` 同步演进吸收历史 tag）。预期 KG 边密度 3–5×，"找同 tag 的 memory" / "找同类 bug" 直接图遍历。 | 🟠 入图入口扩展 | M（4h） | 低（沿现有模式） | `pipeline/ingest.rs::extract_graph_edge_drafts`、`service::resolve_drafts_to_edges`、`cli/repair.rs::compute_rebuild_graph_outcome` |
| 17 | 🔍 | 🚧 **`memories.supersedes_memory_id` → `supersedes` graph edge**（数据已在 `memories` 列上，只是从未写到 `graph_edges`。新建 memory→memory 边的同时关闭被 supersede 旧 memory 的活动边——本来 #4 启示就提到这条，但只实现了"关闭旧边"，没建"指向旧 memory 的 supersedes 边"）。 | 🟢 数据已有，只缺一条边 | S（2h） | 低 | `service::sync_memory`、`storage::graph_store.rs`、`cli/repair --rebuild-graph` |
| 18 | 🔍 | 🚧 **transcript session → memory edge `extracted_from_session`**（`mem mine` 抽出 memory 后，建一条 `entity:session:<id>` → `memory:<id>` 边。反查"这次会话产出了哪些 memory"一次图遍历，不再 SQL 回查 session_id 字段；同时为 §11 sessions 视图打底）。 | 🟢 transcripts ↔ memories 之间唯一桥梁 | S（3h） | 低 | `cli/mine.rs`（POST 成功后多发一条 entity 边 / 或服务端 ingest 自动建）、`domain/memory.rs::extract_graph_edge_drafts` |
| 19 | 🔍 | 🚧 **`code_refs[]` → `entity(kind=file)` + `mentions_file` edge**（路径归一化：相对仓库根 / 跨 repo 用 `repo:<r>:file:<path>` 复合 alias）。"查所有提到 `src/storage/duckdb.rs` 的 memory" 走图。 | 🟠 代码定位入图 | M（半天） | 中（路径归一化坑：trailing slash / 大小写 / OS 差异） | `domain/entity.rs` 加 `EntityKind::File`、`pipeline/ingest.rs`、`storage/entity_registry.rs` |
| 20 | 🔍 | 🚧（LATER）**内容自动抽取实体**（regex 起步：commit hash `[0-9a-f]{7,40}`、Rust 函数名 `fn ([a-z_]+)`、文件路径、错误码；进 entity registry 时打 `auto_extracted=true` 标，便于审计 / 关闭）。opt-in `MEM_EXTRACT_ENTITIES=1`，避免给所有 caller 强加噪声。 | 🟡 KG 自动化扩展，长期天花板提升 | L（1–2 天） | 高（误抽 / 噪声 / Verbatim 边界） | 新 `pipeline/extractors.rs`、`service/memory_service.rs::ingest` 加 hook、entity 加 `auto_extracted` 列 |

> **层（Layer）**：📦 = Verbatim 存储/输出纪律；🔍 = Structured 检索/排序/元数据；⚙️ = 基础设施 / 修 bug。
> **价值**：🔴 = 修 bug；🟠 = 架构升级；🟡 = 体验优化；🟢 = nice-to-have。
> **工作量**：S ≤ 4h，M = 0.5–2 天，L > 2 天。

> **批次执行历史**：
> - **批 A（修 bug）** ✅ sha2 迁移 + `embedding_jobs` dedupe 复核 / 注释澄清。
> - **批 B（Verbatim 纪律落地）** ✅ #9 + #6 + 文档化 — 把"设计原则"刻进代码。
> - **批 C（数据模型扩展）** ✅ #4 + #10 同期做，省一次 schema 迁移阵痛。
> - **批 D（检索现代化）** ✅ #2 + #3 + #5 → #11。HNSW 召回 + RRF + 三段式 Stage 2 拆分。
> - **批 E（生命周期 / 卫生）** ✅ #7 + #8（decay worker + entity registry）。
> - **批 F（无感集成）** ✅ #12（hooks + `mem mine` + `mem wake-up`）。
> - **批 G（评估基础设施）** ✅ #14 + #15（recall ablation bench + LongMemEval parity bench）。
> - **批 H（KG 入图入口拓宽）** 🚧 #16–#20。先做 #16 (tags) + #17 (supersedes) + #18 (transcript-session) 这三条 Quick Wins（合计 ~9h，复用现有 entity registry / repair 路径）；#19 (code_refs) 另起；#20 (内容抽取) 单独决策。

---

## 下一阶段

#1–#15 已完成；#16–#20 待办，触发条件按性质分两条：

- **本质形态差异** — `mempalace-diff §15.1` 列举（HTTP vs MCP / DuckDB vs Chroma / lifecycle vs verbatim）。这些**不会主动消除**；改变只能在哲学路线整体重定义时讨论。
- **新对齐项 / 实测 gap** — 当 mempalace 主仓有新设计或 mem 实测出新 gap 时：先回 `mempalace-diff.md` 加 / 改章节做论证，再回本文件追加新行号（#21+），同时更新 `mempalace-diff §15.2/15.3/15.4` 三张映射表保持一致。#16–#20 的论证沉在 [§5.1](./mempalace-diff.md#51-实测-kg-不够稠密2026-05-07-抽样)。

已知 incident TODO（不计入 ROADMAP，但留档）：
1. 找出 `embedding_jobs` orphan 行的产生路径（疑在某条 supersede / delete pipeline）
2. ~~修复 `migrate_content_hash_to_sha256` 在带子表行时的 FK 失败~~ ✅ `6af5a3e`（load/delete/restore dance）
3. 检测同一 `.duckdb` 文件多进程写入（advisory lock / pid file）

详见 mem incident memory `mem_019dfba4-9e08-71b2-a676-f0218c01f9b6`。

---

> **维护规则**：
> 1. 每完成一项，更新对应行的状态（✅ done / 🚧 in progress）。
> 2. git commit message 引用编号：`feat(retrieve): introduce usearch sidecar ANN (closes ROADMAP #2)`。
> 3. 新增 ROADMAP 行后，**同步更新 `mempalace-diff §15.2`（已吸收）/ `§15.3`（mem 独有）/ `§15.4`（不引入）三张映射表**——否则"现在差异快照"会与 ROADMAP 漂移。
