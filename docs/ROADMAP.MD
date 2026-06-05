# mem 改造路线图

> 本文件是 mem 的改造执行面板：已落地的核心改造（#1–#20）+ 当前活跃的平行工作线。
>
> **平行工作线**（互不阻塞，各用各自的 close 引用）：
> - **② Backend 存储抽象** —— [`backend-coupling.md`](./backend-coupling.md) §6，Phase 0–5 已 shipped（DuckDbQuery / LanceStore 已收窄 `pub(crate)`）；引用 `closes backend-coupling §6 Phase N`
> - **③ 长内容召回** —— [`long-content-recall.md`](./long-content-recall.md)，chunked embeddings 消除长内容尾部静默截断，4 phase 全 ✅；引用 `refs long-content-recall ③ pN`
> - **④ OSS 记忆引擎对照** —— [`oss-memory-diff.md`](./oss-memory-diff.md)，参照 mem0 / agentmemory 沿 write→recall→feedback 主轴补审，产出 O1–O5（**O1 ✅ / O2 ✅ / O3 ✅ / O4 ✅**，O5 待办，见下「OSS 对照路线」表）；引用 `closes oss-memory-diff O#`
>
> **状态（2026-06-05）**：核心改造 #1–#19 全部 ✅，#20（内容抽实体）LATER；③ 长内容召回 4 phase 全 ✅；④ OSS 对照 **O1 ✅**（检索使用强化 + 衰减时钟重置，`808cb59`+`709c648`）/ **O2 ✅**（写时近邻去重 → PendingConfirmation + suspected_supersede 边，`b7b9528`+embedding worker）/ **O3 ✅**（capsule recall 多样化 / per-source 软配额，`pipeline/retrieve.rs`）/ **O4 ✅**（graph boost 按锚 fanout 反比衰减 `spread_decay`，`retrieve.rs`），O5 待办。Incident TODO 见文末（#2/#3/#4 已 ✅）。本文件「路线图」表的项用 `closes ROADMAP #N` 引用。

## 路线图

| # | 层 | 项 | 价值 | 工作量 | 风险 | 触点 |
|---|---|---|---|---|---|---|
| 1 | ⚙️ | ✅ `embedding_jobs` dedupe 复核（单进程被 `Arc<Mutex<Connection>>` + 事务覆盖，仅修注释/文档）⚠️ 多进程并发写同一 `.duckdb` 是另一类问题，2026-05-06 FK 循环事故已沉淀为 mem incident memory + worker 三层防御（open-time / claim-time / tick last-resort sweep）| 🔴 修并发 bug | S（0.5h） | 低 | `storage/duckdb.rs`、`db/schema/002_embeddings.sql` |
| 2 | 🔍 | ✅ 引入 `usearch` sidecar ANN（消除 `semantic_search_memories` 的 `limit 2000` 隐式截断）。**注**：sidecar 在 2026-05 大重构里被 LanceDB 0.27 native ANN 全替换（backend-coupling QW-4），`usearch` 依赖已删，`vector_index.rs` 不复存在。原 #2 解决的"隐式截断"问题已永久消除 | 🟠 性能基础设施 + 🔴 修隐式正确性边界 | M（1–2 天） | 中（需要 repair 路径） | ~~`Cargo.toml`、`storage/`、新增 `vector_index.rs`~~（均已删） |
| 3 | ⚙️ | ✅ HNSW 健康度自检 + `mem repair --check` / `--rebuild` 子命令（JSON 输出可选）。**注**：随 #2 sidecar 一并下架——`mem repair` 子命令已删，`src/cli/repair.rs` 不复存在，LanceDB native ANN 不需要外部 repair 路径 | 🟠 配套 #2 | S（4h） | 低 | ~~`src/cli/repair.rs`、`src/storage/vector_index_diagnose.rs`~~（均已删） |
| 4 | 🔍 | ✅ 图边时序化（valid_from/to）+ 全量迁到 DuckDB（删 IndraDB 依赖与 GraphStore trait）| 🟠 表达力 | L（实际 ~1.5 天，scope 比原 M 大）| 中 | `domain/memory.rs`、`storage/graph_store.rs`、`db/schema/003_graph.sql`、`pipeline/ingest.rs` |
| 5 | 🔍 | ✅ 检索分数归一化 / 两路 RRF（lex+sem，`MEM_RANKER=legacy` 一档兜底）| 🟡 排序质量 | S（3h） | 低 | `pipeline/retrieve.rs` |
| 6 | 📦 | ✅ `compress_text` 改 token 计数（tiktoken-rs::o200k_base，CJK 不再按词裸奔截断）| 🟡 输出 verbatim 纪律 | S（2h） | 低 | `pipeline/compress.rs` |
| 7 | 🔍 | ✅ `decay_score` 后台衰减 worker（`f7a6c87` feat(service): add decay_worker for time-based memory decay；`62b2662` fix(decay): rewrite decay_worker SQL for DuckDB and ms-string timestamps） | 🟡 生命周期闭环 | S（3h） | 低 | `service/`、新增 `decay_worker.rs` |
| 8 | 🔍 | ✅ **Entity registry**（`entities` + `entity_aliases`；`MemoryRecord.topics`；ingest auto-promotes via `extract_graph_edge_drafts` + `resolve_drafts_to_edges`；4 HTTP routes `/entities`；`mem repair --rebuild-graph`；`8f8d2af..68395a0`） | 🟢 数据卫生 | M（半天） | 中（迁移） | `domain/entity.rs`、`storage/entity_registry.rs`、`db/schema/008_entity_registry.sql`、`src/http/entities.rs`、`src/cli/repair.rs` |
| 9 | 📦 | ✅ **Verbatim 守护**（caller 显式提供 `summary` 时校验 `summary != content`；AGENTS.md / README.md 已写） | 🟢 哲学一致性 | S（1h） | 低 | `pipeline/ingest.rs`、`AGENTS.md`、`README.md` |
| 10 | 🔍 | ✅ **Sessions**（schema + auto-bucket；HTTP 端点 / `episodes.session_id` / DELETE 软删延后） | 🟠 表达力 | M（半天） | 中（schema 迁移）| 新增 `sessions` 表 + `memories.session_id` + auto-bucket on ingest |
| 11 | 🔍 | ✅ **检索流水线 Stage 2 拆分**（lifecycle 加性层抽出共享 helper；Stage 3 / 响应形状变更延后） | 🟠 架构升级 | M（1 天 → 实际 ~2h） | 中（依赖 #2 + #5） | `pipeline/retrieve.rs` 重构、`pipeline/compress.rs` 输出层调整 |
| 12 | 🔍 | ✅ **Claude Code / Codex 无感集成包**（Stop/PreCompact/SessionStart hooks + `mem mine` 离线 miner + `mem wake-up`；`138eb2e` feat(integration): Claude Code seamless integration、`fd07564` feat(hooks)、`7c4860c` feat(mine)、`a013a77` feat(wake-up)、`d374b86` feat(transcripts): mem mine writes every block to /transcripts/messages） | 🟠 用户体验代际升级 | M（1.5 天） | 中（hook 脚本跨平台、跨 agent runtime） | 新增 `hooks/`、`src/cli/{mine,wake_up}.rs`、`README.md` 集成章节 |
| 14 | 🔍 | ✅ **Recall quality bench (transcripts)**（10-rung ablation, synthetic+real fixtures, oracle-rerank upper bound; closes spec `2026-05-03-transcript-recall-bench-design`） | 🟡 决策基础设施 | M（半天） | 低 | `src/pipeline/eval_metrics.rs`, `tests/bench/`, `tests/recall_bench.rs` |
| 15 | 🔍 | ✅ **MemPalace LongMemEval parity bench** (Rust port of `longmemeval_bench.py`; 3 rungs map to mempalace `raw / rooms / full`; output mirrors `results_*.json` shape; closes spec `2026-05-05-mempalace-bench-parity-design`) | 🟡 横向对比基础设施 | M（半天） | 低 | `src/pipeline/eval_metrics.rs`, `tests/bench/longmemeval*.rs`, `tests/mempalace_bench.rs` |
| 16 | 🔍 | ✅ **`tags` → `entity(kind=tag)` + `tagged` edge**（沿用 #8 entity registry 模式；ingest 时把 `tags[]` 路由通过 `EntityRegistry::resolve_or_create`）。预期 KG 边密度 3–5×，"找同 tag 的 memory" / "找同类 bug" 直接图遍历。新增 `EntityKind::Tag` variant；`contradicts:` 前缀的 tag 跳过（历史 artifact，不是真 tag）。历史数据回溯（吸收已有 capsules 的 tags）暂未做，需要时再写一次性脚本。 | 🟠 入图入口扩展 | M（实际 ~2h） | 低（沿现有模式） | `domain/entity.rs::EntityKind`、`pipeline/ingest.rs::extract_graph_edge_drafts`、`service::resolve_drafts_to_edges` |
| 17 | 🔍 | ✅ **`memories.supersedes_memory_id` → `supersedes` graph edge**（数据已在 `memories` 列上）。**审计发现已 shipped**——`extract_graph_edge_drafts` lines 236-246 emit `ToNodeKind::LiteralMemory` with relation `supersedes` when `supersedes_capability_capsule_id` is set；wired through `edit_and_accept_pending`；unit test `extract_graph_edge_drafts_emits_literal_memory_for_supersedes` 覆盖。原 ROADMAP 文本是陈年误判。 | 🟢 数据已有，只缺一条边 | S（已落地） | 低 | `pipeline/ingest.rs` |
| 18 | 🔍 | ✅ **transcript session → memory edge `extracted_from`**（每次 ingest 都 auto-bucket 进 session 并落一条 `memory:<id> --extracted_from--> session:<sid>` 边）。方向选 memory→session（不是 ROADMAP 原文的 session→memory）让 `close_edges_for_capability_capsule` 在 hard-delete 时自动 close 边，避免悬挂 edge。Session_id bypass entity registry——session_id 已是 canonical UUIDv7，无 alias 化需求，所以引入 `ToNodeKind::LiteralSession` 直写 `session:<id>` 节点。`mem mine` 路径无改动：ingest 的 auto-bucketing 已经把 session_id 设上去。 | 🟢 transcripts ↔ memories 之间唯一桥梁 | S（实际 ~1.5h） | 低 | `pipeline/ingest.rs`（`ToNodeKind::LiteralSession`、`session_node_id`、draft extraction）、`service::resolve_drafts_to_edges` |
| 19 | 🔍 | ✅ **`code_refs[]` → `entity(kind=file)` + `mentions_file` edge**。新增 `EntityKind::File` + `normalize_file_ref` (剥 `:line` 后缀、剥 trailing `/`、保留 Windows 盘符) + `file_alias` 复合 (`<repo>:<path>` 或 bare path)。重复 ref 在 draft 层不去重——`sync_memory_edges` 的 `(from, to, relation)` idempotency check 收口。大小写归一化故意没做（Linux/macOS 大小写敏感，跨 OS 走 normalization 反而错）。 | 🟠 代码定位入图 | M（实际 ~1.5h） | 低（路径归一化只做无歧义的部分） | `domain/entity.rs::EntityKind`、`pipeline/ingest.rs`（`normalize_file_ref` / `file_alias` / `file_node_id`）、`service::resolve_drafts_to_edges` |
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
> - **批 H（KG 入图入口拓宽）** ✅ #16 + #17 + #18 + #19。实际 ~5h（vs 估 ~13h），#17 审计发现早已 shipped；#19 路径归一化故意只做无歧义的部分（剥 `:line` / `/`，保留 Windows 盘符 + 大小写）。`#20` (内容抽取) 单独决策——风险高，需要先定 Verbatim 边界。

---

## OSS 对照路线（O 系列）

> 来源 [`oss-memory-diff.md`](./oss-memory-diff.md)（④，2026-06-05）。参照 mem0 / agentmemory 沿 **write → recall → feedback** 主轴补审，每项论证 / 触点 / 风险见原文 §5。**O1 ✅（`808cb59`+`709c648`）/ O2 ✅（`b7b9528`+worker）/ O3 ✅（`retrieve.rs`）/ O4 ✅（`retrieve.rs`），O5 待办。** close 引用用 `closes oss-memory-diff O#`。

| # | 层 | 项 | 价值 | 工作量 | 风险 | 触点 |
|---|---|---|---|---|---|---|
| O1 | 🔍 | ✅（P0，`808cb59`+`709c648`）**检索使用强化 + 衰减时钟重置**。加 `last_used_at` 列；`search` 把真正写进输出的 capsule 经 channel 异步交给 always-on `last_used_worker` bump（"used"语义，非"retrieved"）；`decay_worker` 锚改"最后一次触碰"（用过锚 `last_used_at`、没用过锚 `updated_at`），用过即重置衰减时钟。**落地偏离**（lance UPDATE 限制，以代码为权威，详见 [`oss-memory-diff.md`](./oss-memory-diff.md) §O1「已落地」）：bump 在 service 层而非 `compress.rs` 内；decay 因 lance 不支持 SET 里 `COALESCE`/多值 `IN` 拆成两条 WHERE 互斥 UPDATE；retrieve freshness bonus 标 optional 未做。 | 🟠 直击最大结构性弱点：ranking 在无显式回调时自主生长 | M | 中（bump 异步、读路径外；攒批 coalesce 收口写放大） | capsule schema、`worker/last_used_worker.rs`、`worker/decay_worker.rs`+`duckdb_query/decay.rs`、`service/capability_capsule_service.rs` |
| O2 | 🔍 | ✅（P1，`b7b9528`+worker）**write 时近邻去重 / 矛盾检测**。`content_hash` 仅完全一致去重，同义改写落新行。落地：embedding worker 嵌完新 `Active` capsule 后做 semantic top-k 近邻，cosine 超阈值（默认 0.92）则 `set_capsule_status(.., PendingConfirmation)` + 写 `suspected_supersede` 图边（不自动覆盖，守 verbatim）。**落地偏离**（详见 [`oss-memory-diff.md`](./oss-memory-diff.md) §O2「已落地」）：标注用图边而非 tag（lance 改不了 List 列）；检查在异步 worker 非 ingest 同步路。opt-in `MEM_INGEST_NEARDUP_ENABLED`。 | 🟠 预防膨胀与矛盾并存 | M | 低-中（阈值保守，异步、读路径外） | `worker/embedding_worker.rs`、`storage/*`（`set_capsule_status`）、复用 hybrid 近邻 + review 队列 |
| O3 | 🔍 | ✅（P1，`retrieve.rs`）**capsule recall 多样化（per-source cap）**。落地：`finalize` floor 过滤后调 `diversify_by_source`——source=`session_id`（无则自身 id），每 source 在 head 至多 `cap`（默认 3），超额按原排名挪尾部不丢（软上限，compress 按 budget 仍纳）。同 supersede 链那路 `hybrid_candidates` SQL 早已版本链去重，故只治"同 session 批量霸占头部"。选轻量配额非 MMR。默认开 `MEM_RECALL_PER_SOURCE_CAP`（=0 关）。借 agentmemory `max 3 per session`。 | 🟡 头部去冗 | S | 低（软上限，不足回填） | `pipeline/retrieve.rs::finalize` |
| O4 | 🔍 | ✅（P2，`retrieve.rs`）**graph boost 按 degree 衰减**。落地：`compute_graph_boosts` 加 `spread_decay(degree)=1/(1+0.001·(degree-1)²)`,`boost=round(GRAPH_BOOST·spread·strength)`,degree=锚 entity 的 capsule fanout。略多一处:dynamics-OFF 路从扁平批量 `related_capability_capsule_ids` 改成逐 anchor `neighbors_within`(才有 degree),local-first 规模下开销可忽略。degree≤1 不衰减。 | 🟡 抑制高扇出节点过度 boost | S | 极小 | `pipeline/retrieve.rs::compute_graph_boosts` |
| O5 | 📦/⚙️ | ⬜（P2）**ingest secret 脱敏**。现状 transcript 整段 verbatim 存，verbatim 抬高密钥泄露面。改法（两层守 verbatim）：原文 access-control 下原样保留，仅在索引/输出层 redact 高置信密钥模式（`sk-`、AWS AKIA、私钥块、`<private>`）；不就地改写 `content`。借 agentmemory privacy filter。 | 🟡 降低泄露面 | M（先定密钥白名单边界） | 中（误 redact 风险，与 verbatim 边界强相关，落地前单独定调） | index/输出侧（`pipeline/compress.rs`、嵌入前文本），不动落盘 content |

---

## 下一阶段

核心改造 #1–#19 全部 ✅；**③ 长内容召回** 4 phase 全 ✅；剩余待办：

- **#20** — 内容自动抽取实体（opt-in regex），风险高、需先定 Verbatim 边界，LATER。
- **跨 scope tunnel 提升** — scope 内实体共现边已落地 `af71bd3`（`src/worker/cooccurrence_worker.rs`，默认 OFF）；下一步「共现 → 跨 scope tunnel 提升」可评估质量后再做。
- **③ 长内容召回**（chunked embeddings）— 独立 workstream，4 phase 全 ✅（capsule + transcript 两条管线都已切片嵌入），详见 [`long-content-recall.md`](./long-content-recall.md)。剩：存量长内容回填（靠 `embeddings_rebuild` 重嵌，未自动化）。
- **④ OSS 对照（O1–O5）** — 参照 mem0 / agentmemory，**O1 ✅**（检索使用强化 + 衰减时钟重置，`808cb59`+`709c648`）/ **O2 ✅**（写时近邻去重 → PendingConfirmation + suspected_supersede 边，`b7b9528`+embedding worker）/ **O3 ✅**（capsule recall 多样化 / per-source 软配额，`pipeline/retrieve.rs`）/ **O4 ✅**（graph boost 按锚 fanout 反比衰减，`retrieve.rs`），**O5 待办**，详见上「OSS 对照路线」表 + [`oss-memory-diff.md`](./oss-memory-diff.md)。O1 让生命周期在 agent 不回调时也能自主生长（`search` 命中即异步回写 `last_used_at`）；O2 在写入侧防膨胀（同义改写不再静默落新行，落审而非自动合并，守 verbatim）；O3 在读出侧防一源霸榜（同 session 批量不再霸占头部）；O4 抑制高扇出锚节点一刀切 boost（`repo:`/`project:` 摊薄，窄实体保留）。
- **召回质量 bench** — #14/#15（recall ablation + LongMemEval parity）已在 `4df527b` **删除**（依赖旧 usearch HNSW，迁 Lance native ANN + Store API 时不可迁移）；`bench-fixtures/` 已空。要对 ③ chunking / O 系列等检索改动做量化评估需**重建**（移植 longmemeval harness 到新 API + 数据集）。`src/pipeline/eval_metrics.rs` 的指标函数（recall@k / ndcg / mrr）仍在、可复用。
- **不做**：多语言 embedding（默认 `Qwen/Qwen3-Embedding-0.6B` 已覆盖多语言）；spellcheck / llm_refine / dialect / repair。

已知 incident TODO（不计入 ROADMAP，但留档）：
1. 找出 `embedding_jobs` orphan 行的产生路径（疑在某条 supersede / delete pipeline）
2. ~~修复 `migrate_content_hash_to_sha256` 在带子表行时的 FK 失败~~ ✅ `6af5a3e`（load/delete/restore dance）
3. ~~检测同一 `.duckdb` 文件多进程写入（advisory lock / pid file）~~ ✅ `1756033`（`storage::open_lock` — fs4 advisory lock at `Store::open`；sentinel `<path>.lock`；`MEM_OPEN_LOCK_DISABLED=1` 逃生口）
4. ~~**`accept_pending` 返回 HTTP 500 "column type mismatch"**~~ ✅ **已修复 `e0e0a5e`（fix(storage): tolerate UInt64 version column in legacy Lance dbs）并于本轮 2026-05-31 部署上线**——根因即遗留库 `version` 列是 UInt64、而 `record_batch_to_capability_capsules` 严格 downcast 到 Int64 失败（45c65f4 改 schema 但 `ensure_table` 不迁移老表）。以下为原始记录：2026-05-21 review session 发现：6 个 capsule 全部 accept 时报 500，但实际写入成功（status flip 到 active，confidence bump 到 0.6）。500 是 phantom——response-decode 阶段 `record_batch_to_capability_capsules` 的某个 column downcast 失败。具体哪个 column 之前不可知（错误信息无 column 名）。**部分修复**：`8c39c3a`(待提交) 给 `record_batch_to_capability_capsules` 的 `col` helper 加结构化 `tracing::error!`，下次 fire 时日志里能看到 column 名 + expected / actual arrow 类型。**完整 fix 仍待**：(a) 需要 mem serve 重启加载新二进制 才能下次发生时看到日志细节；(b) sibling parsers 还没加同样的诊断；(c) 真根因（schema 漂移？stale binary？）仍未定位

详见 mem incident memory `mem_019dfba4-9e08-71b2-a676-f0218c01f9b6`。

---

> **维护规则**：
> 1. 每完成一项，更新对应行的状态（✅ done / 🚧 in progress / ⬜ todo）。
> 2. git commit message 引用编号，区分来源：
>    - 本文件「路线图」项：`feat(retrieve): … (closes ROADMAP #N)`
>    - OSS 对照 O 项：`feat(retrieve): bump last_used_at on compress emit (closes oss-memory-diff O1)`
>    - 平行工作线 ②/③：`… (closes backend-coupling §6 Phase N)` / `… (refs long-content-recall ③ pN)`
