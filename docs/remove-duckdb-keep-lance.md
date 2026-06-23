# 去掉 DuckDB、保留 Lance —— 路线 B 设计与执行计划

> Status: **设计已定，未实现** (2026-06-23)。决策方向：**去掉 DuckDB 读引擎，保留 Lance 做写/存，不切 Postgres，不放弃 local-first 单二进制**。
> 本篇是 `duckdb-read-path-strategy.md`（2026-06-15，"保留 DuckDB"的决策）的**反向新决策**——那篇结论是"DuckDB 留着、不值得替"，本篇在掌握"QW-1 已把融合拆成可移植 Rust 原语"+"现网真实量级只有十几万行"两个新事实后，重新判定 Route B 可行并给出执行计划。
> 仍是**设计存档**：落地前先做 §6 的 Phase-0 验证，尤其是 Tantivy 重建耗时微基准。

---

## 0. 范围与前置决策

| 维度 | 决定 |
|---|---|
| 去 DuckDB | ✅ 去掉 `src/storage/duckdb_query/` 整个读引擎 + `duckdb` crate |
| 留 Lance | ✅ Lance 继续做磁盘存储 + 写（LanceDB Rust API） |
| 切 Postgres | ❌ 不切（Postgres 后端保留为可选 backend，不动） |
| local-first 单二进制 | ✅ 守住，不引入外部服务依赖 |

被否的替代路线（见 `duckdb-read-path-strategy.md` §4 留档）：切 Postgres-only（放弃 Lance + local-first）、DataFusion 当骨干（重依赖 + 方言重写）。

---

## 1. 现状盘点（DuckDB 在本仓的真实面）

DuckDB **不是独立后端**，而是 Lance 之上的 **SQL 读引擎**：in-process `duckdb::Connection` 加载 `lance` core extension，`ATTACH` 磁盘上的 Lance datasets，对它们跑 SQL。写不走 DuckDB（走 LanceDB Rust API），唯一例外是 §5 的 2 个 decay 写。

直接依赖面：

| 位置 | 内容 | 量 |
|---|---|---|
| `Cargo.toml` | `duckdb = { version="1", features=["bundled"] }` | 静态编 duckdb 库（拖慢 CI 链接） |
| `src/storage/duckdb_query/` | 整个读层：`mod.rs`（连接构建 / `refresh` / `with_commit_retry` / `SET threads`）、`capability_capsules.rs`(2162)、`transcripts.rs`(1372)、`graph.rs`(1203)、`entities.rs`(360)、`decay.rs`(390，唯一 DuckDB 侧写)、`embedding_jobs.rs`(57) | **~5953 行 / 240 KB / 69 个 SELECT** |
| `src/storage/store.rs` | 组合 `query: Arc<DuckDbQuery>`；~30 个读方法 `self.query.*`；`mark_dirty` / `ensure_fresh` / `refresh` / `commit_lance_write` 机制 | 大 |
| `src/storage/types.rs` | `StorageError::DuckDb(#[from] duckdb::Error)` + `GraphError` 的 `From<duckdb::Error>` —— duckdb 类型泄漏进共享错误枚举 | 2 处 |
| `src/storage/mod.rs` | `pub(crate) mod duckdb_query` | 1 行 |
| `src/main.rs` + `MEM_DUCKDB_THREADS` | `max_blocking_threads(32)` + `SET threads=6`，皆为缓解 DuckDB 读锁/CPU 而加 | 资源缓解 |
| 测试 | `tests/lance_snapshot_visibility.rs`（快照-pin 探针，纯 duckdb）、`tests/transcript_recall.rs`、`tests/entity_registry.rs` 的 duckdb 部分 | — |
| examples/bench | `examples/lance_duckdb_poc.rs`、`examples/hybrid_sql_poc.rs`、`benches/hybrid_compose_vs_fused_bench.rs` | — |
| CI | `.github/workflows/ci.yml` 默认 `rust` job 编 lance+duckdb（为 duckdb 链接重量 drop debuginfo） | — |

**SQL 特征普查**（驱动 §3 分桶）：69 SELECT、21 GROUP BY、4 ROW_NUMBER、5 JOIN、1 WITH RECURSIVE、4 NOT EXISTS、11 `lance_vector_search`、9 `lance_fts`。graph-BFS 已是**迭代 Rust**（`neighbors_within`，≤3 跳）。

**现网真实量级（2026-06-23 实测 `/root/.mem/mem.duckdb/`）**：
- `conversation_messages.lance` ≈ **270 MB / ~139,020 个 block**（168 session）。
- `conversation_message_embeddings.lance` ≈ 246 MB。
- 全 `/root/.mem` ≈ 554 MB（含 hf 模型缓存）。
- **更正**：CLAUDE.md 里的「118M」是 **~118 MB 表体积，不是 1.18 亿行**。FTS 语料是**十几万短文档**，不是亿级——这是判定 Tantivy 可行的关键事实。

---

## 2. 读引擎选型

**拍板：原生 lancedb Rust API（`only_if` 过滤 + `nearest_to` ANN）+ Rust 侧融合/聚合；FTS 另起 Tantivy 自建倒排。DataFusion 不作骨干，仅留作个别聚合的局部逃生口。**

理由：

1. **"非 SQL 不可"的桶其实已经移植了**。RRF/window/JOIN 只活在 fused-SQL 快路径 `hybrid_candidates`（`ROW_NUMBER OVER` + `FULL OUTER JOIN` + `1/(60+rank)`）；其可移植等价物 **QW-1（2026-05-16）已落**：`bm25_candidate_ids` + `ann_candidate_ids` 出排序 id，`pipeline/retrieve.rs::sql_rrf` 在 Rust 重算 RRF，`hybrid_candidates_compose` 即 `store.rs` 注释标明的 "portable equivalent"。融合地基已在 Rust，无需查询引擎。
2. **其余 SELECT 化简后**= `only_if` 过滤扫描 + `nearest_to` ANN + `fetch_*_by_ids` 取整行（已有原生方法）+ 少量 Rust 聚合。lancedb 0.30 Rust API 全覆盖（已查证：`query().only_if(...).nearest_to(...).limit()`、`full_text_search`、`table::datafusion` adapter、`rerankers::rerank_hybrid`）。DataFusion 的 join/groupby/window 价值用不上。
3. **DataFusion 是重依赖 + 方言重写 + 集成风险**，与 local-first"轻"相悖。保留作针对性逃生口：个别 GROUP BY 密集统计（stats/taxonomy）若手写 Rust 聚合太丑，可局部用 lancedb 自带 `table::datafusion`，不必全局上。

一句话：**ANN 走 lance 原生（robust）、FTS 走 Tantivy（绕 bug）、融合/排序留 Rust（已就绪）、DataFusion 只当局部逃生口。**

---

## 3. 69 个 SELECT 按能力分桶 → 替代映射

| 桶 | 现状(DuckDB) | 新引擎替代 | 可达性 | 风险 |
|---|---|---|---|---|
| 普通过滤 | `WHERE … LIMIT` | `query().only_if("sql谓词").limit()` 原生 | ✅ 直接 | 低 |
| **向量 ANN**(11) | `lance_vector_search` | `query().nearest_to(v).nprobes().refine_factor()` 原生 | ✅ v0.1.7 已证 robust | 低 |
| **FTS**(9) | `lance_fts` | **Tantivy 倒排**，出排序 id | ✅ | 中（索引生命周期 + parity，见 §4） |
| **RRF/window/ROW_NUMBER**(4) | fused-SQL | 丢 fused 路，走 compose：两原语出 rank → `sql_rrf` Rust 融合（已存在） | ✅ 地基已就绪 | 中（14-29% 慢，见下） |
| **JOIN**(5) | 取整行 JOIN / RRF FULL OUTER JOIN | 先排名出 id，再 `fetch_*_by_ids` 取整行，Rust 拼 | ✅ fetch-by-ids 已有 | 低 |
| GROUP BY/聚合(21) | stats、taxonomy、transcript chunk 折叠(`MIN(_distance) GROUP BY block_id`) | 多数=Rust HashMap 聚合；chunk 折叠并进 ANN 后处理；实在重的局部用 `table::datafusion` | 🟡 多数易，stats/taxonomy 要手写 | 中 |
| 版本链去重 `NOT EXISTS`(4) | "排除有 active 后继" | `only_if` 谓词或取回后 Rust filter | ✅ | 低 |
| `WITH RECURSIVE`(1) | supersede 版本链 walk | Rust 迭代 walk（同 graph-BFS 形态）或反复 fetch-by-id | ✅ 仅一处 | 低 |
| graph-BFS | 已是迭代 Rust（`neighbors_within`） | 逐跳 `only_if` 邻居查 + Rust BFS | ✅ 几乎照搬 | 低 |

**两块最难的深挖**：

**window/RRF**：直接删掉 fused-SQL 路径（`hybrid_candidates` + 4 个 `ROW_NUMBER`/`FULL OUTER JOIN`），`Store::hybrid_candidates` 改为永远走 `hybrid_candidates_compose`。代价：bench 实测 fused 比 compose **快 14-29%**；缓解：两原语并发跑（ANN ‖ FTS），本地少并发下绝对延迟差通常 <10ms。收益：window/ROW_NUMBER/FULL OUTER JOIN 整桶消失。

**FTS**：见 §4。核心是 `lance-7.0 scanner.rs` ragged-batch bug——**lance core scanner 在"已索引段 + 未索引尾"合并时的 bug，非 DuckDB 特有**；v0.1.7 把 FTS 读改走 lancedb Rust（同 lance 7.0）撞了同一 bug、已回退。故"换 lance 原生 FTS 表函数"绕不过去 → 上 Tantivy。

---

## 4. FTS 落地选型（最难的一块）

### 4.1 三候选对照

| 维度 | ①Tantivy 自建 | ②等/推 lance 上游修 | ③折中:lance FTS + 写后强制 reindex |
|---|---|---|---|
| 实现成本/工期 | 中，~4-6d | 低(短期)/不可控 | 低，~2-3d |
| local-first | ✅ 纯 Rust 嵌入，进二进制 | ✅ | ✅ |
| 索引体积 | ~40-80 MB（待测） | 0 | 0 |
| 写放大 | 低（增量 commit 廉价） | 无 | **高**（O(N) per reindex） |
| 查询延迟 | 低且稳 | 命中 bug 时丢召回 | 索引新鲜时低，reindex 窗口抖动 |
| 崩溃/重启一致性 + 重建 | 需维护 Tantivy↔Lance 一致；最坏启动全量重建（~十几秒，待实测）；可持久化+补增量免全量 | ✅ 无额外状态 | ✅ 无额外状态，但"保持全覆盖"本身是持续重建 |
| parity 风险 | 低（RRF 吃 rank 不吃分，引擎分差被吸收） | 无 | 无 |
| 长期维护 | 中（多一套索引生命周期，但自掌控） | 低但被动（跟 lance 版本 + arrow major） | 中（reindex 调度 + soft-degrade 长期养） |
| **bug 复发/受制上游** | **❌ 彻底脱离** | **🔴 完全受制上游** | **🟠 永远潜伏 + 必须长期养 soft-degrade** |

### 4.2 推荐 + 触发改选

**选 ① Tantivy 自建**：(a) 唯一彻底脱离上游 bug 的方案（路线 B 初衷）；(b) 真实量级（~14 万文档）把"重建耗时"消解为十几秒级；(c) parity 风险最低（RRF 吃 rank）。

触发回退：
- → **③ 折中**：Phase-0 发现 Tantivy↔Lance 一致性维护过于易错，且 corpus 小到"每查询前 fresh reindex"可接受。
- → **② 收编回 lance FTS**：lance 上游发布**经验证修复 scanner、且 arrow-major 兼容**的版本时——删掉整个 Tantivy 模块是更简方案。**即便选 ①，也要持续跟踪 lance changelog，把"上游修好后拆 Tantivy"列为长期简化项。**

---

## 5. 五阶段执行计划 + 不可逆门槛 + 估时

**Phase 0 —— 全部非破坏、可并行（DuckDB 仍是默认读路径）** ~5-8d
- (a) **抓 golden**：对现网/测试库跑全部读方法 dump DuckDB 结果做 parity fixture（见 §7）。
- (b) **起 Tantivy**：新增 `src/storage/fts/`（索引 + 双写 hook），先只写不读；先跑 §6 微基准定"启动重建 vs 持久化增量"。
- (c) **铺原生读**：`LanceStore` 上逐个加与 `DuckDbQuery::*` 对应的原生方法，behind `MEM_READ_ENGINE` 开关，默认仍 duckdb。
- ✅ **隐性前提已验证（2026-06-23）**：探针 `tests/lance_version_visibility.rs`（2/2 通过）实证 lancedb 0.30 的 `ReadConsistency` 语义——默认连接（`Manual`）下 warm reader 需 `checkout_latest()` 才见别的句柄的写；`read_consistency_interval(0)`（`Strong`）下每读自动见最新；新 `open_table` 总见最新。**结论：DuckDB 那套 `refresh/mark_dirty`（~100ms 整连接重建）可删**——读连接设 `read_consistency_interval(Duration::ZERO)` 即透明新鲜（每读一次 manifest 检查，廉价），或每读 `open_table`，或 warm 句柄 + dirty 标记 + `checkout_latest()`。**推荐 (a) Strong 连接**。详见 §8 #1。

**Phase 1 —— 逐桶切换 + 逐桶 parity（开关切 lance-native，随时可切回）** ~8-12d
- 顺序（易→难）：普通过滤 → ANN → fetch-by-ids/JOIN → 版本链/NOT EXISTS/RECURSIVE → graph-BFS → **FTS(Tantivy)** → **RRF compose 收尾** → transcripts（FTS+ANN+chunk 折叠）→ stats/taxonomy。
- 每桶过 golden parity 才推进。**全程可逆**。

**Phase 2 —— 🔴 不可逆门槛：DuckDB 侧写迁移** ~3-5d
- `decay.rs` 的 2 个 DuckDB-side `UPDATE`（`apply_time_decay` + `bump_last_used_at`，唯一的 DuckDB 写）改成 **lancedb Rust API 的 update/merge**，并重证 `with_commit_retry` 的 commit-race 处理在新写法下成立。
- **这是唯一不可逆门槛**：门槛前保持双引擎可逆，门槛后才删 DuckDB。

**Phase 3 —— 删除 + 瘦身** ~2-3d
- 删 `src/storage/duckdb_query/`（~6000 行）、`Store` 的 `query`/`refresh`/`mark_dirty`/`ensure_fresh`（Phase-0 已证实可删，见 §8 #1）、`types.rs` 的 duckdb `From`、`MEM_DUCKDB_THREADS`、`main.rs` 为 duckdb 加的资源缓解。
- `Cargo.toml` 删 `duckdb`；`arrow-array` 视情况保留（lance 仍需）。

**Phase 4 —— CI / 测试 / 示例** ~2-3d
- 删/改 `tests/lance_snapshot_visibility.rs`、`transcript_recall.rs`/`entity_registry.rs` 的 duckdb 部分；删 `examples/lance_duckdb_poc.rs`/`hybrid_sql_poc.rs`、`benches/hybrid_compose_vs_fused_bench.rs`；CI 默认 job 去 duckdb 链接特化、加 parity job。

**总估时 ≈ 20-30 人日**（单人；FTS/Tantivy + transcripts 是大头）。Phase 0–1 可与日常并行、随时可逆；**Phase 2 是唯一不可逆门槛**。

---

## 6. Tantivy 重建耗时微基准设计（Phase-0 必做）

**形态**：一次性 `examples/fts_bench.rs`（或 `/tmp` 独立小 crate），`cargo run --release` 跑完即删，不提交。临时把 `tantivy` + `tantivy-jieba`/`cang-jie` 加进 `[dev-dependencies]`（测完撤）。只读 `/root/.mem/mem.duckdb/conversation_messages.lance`。

**此规模优先"直接测真值"而非外推**：D≈14 万小到可全 D 真建（几十秒），10×D 用复制 10 份（id 加后缀）真建，比线性外推可信。N=1万抽样只用于快速迭代分词器 A/B。

### 6.1 取数（避免全表载入）
- **D** = `table.count_rows(Some("embed_eligible = true"))`（原生带 filter 计数，不扫 content）。
- **B + 抽样**：`table.query().only_if("embed_eligible = true").select(Columns(["message_block_id","content"])).execute()` 流式迭代，`B += Σ content.len()`，蓄水池抽 N=1万 存 `/tmp/fts_sample.jsonl`；可选落全 D `/tmp/fts_full.jsonl`。
- 顺带：content 长度分布(p50/p99)、中文占比（非 ASCII 字节比，定分词器权重）。
- 注：磁盘 270M 含旧 fragment，`count_rows` 走逻辑行，拿到真 D。

### 6.2 临时 Tantivy 索引
schema：`content`(TEXT, indexed, WithFreqsAndPositions, 选定 tokenizer)、`message_block_id`(STRING, STORED — 命中返回的 id)、`tenant`(STRING/fast field — 过滤)。`embed_eligible` 不进 schema（取数已预过滤）。
writer heap：A/B 两档 `256MB` vs `1GB`，记峰值 RSS。
commit：全量重建=全部 `add_document` 后单次 `commit()` + `wait_merging_threads()`（含合并才是真值）；增量=每 1万 commit 一次（验回退方案）。

### 6.3 中文分词器 A/B（最该测）
| 档 | 接法 | 预期 |
|---|---|---|
| A. 默认 whitespace | 自带，不注册 | build 最快但中文召回基本废，作**速度上界基线** |
| B. `tantivy-jieba`(结巴) | `register("jieba", JiebaTokenizer)` + `content.set_tokenizer("jieba")` | 召回好、build 慢；**主候选，重点测 docs/sec** |
| C. `cang-jie`(CJK n-gram) | 同上注册 | 召回介于 A/B，build 比结巴快；B 太慢时备选 |

parity 量法：选 30-50 条真实查询（中文/中英混/英文标识符），在"现网 lance FTS(golden)"与"Tantivy 各分词器"上取 top-20 id，算 **overlap@10/@20** + **Kendall-τ**；因 RRF 吃 rank，门槛设软：**overlap@10 ≥ 0.8** 即足够。

### 6.4 指标 + 外推
四指标（每分词器 × heap 档）：**docs/sec**、**索引体积**(`du -sh`)、**查询 BM25 p50/p99**(固定查询集 ≥200 次，冷/热各一轮)、**峰值 RSS**。
- 全 D 真建拿 D 真值；复制 10×D 真建拿 10×D 真值。
- 线性拟合(1万/5万/全D 三点)只作交叉验证("build ≈ 固定开销 + 每文档成本×n"、"体积 ≈ k×B")；超线性以实测为准。
- **查询延迟不可线性外推**（随段数/倒排长度次线性），必须在真建的 D 和 10×D 索引上各自实测。

### 6.5 验收门槛 + 产出表 + 回退判据
产出表（每格填实测）：`分词器 | 规模(D/10×D) | build秒数 | docs/sec | 索引体积 | 峰值RSS | 查询p50 | 查询p99 | overlap@10`。

| 实测命中 | 结论 |
|---|---|
| 10×D 全量重建 **< 30s** 且 RSS 可接受 | ✅ **"启动全量重建"兜底可行**，Tantivy 可不持久化，最简 |
| 10×D 重建 **30s–2min** | 🟡 建议**持久化索引 + 启动只补增量**（按 `max(created_at)`/`message_block_id` 水位），避免每次重启卡 |
| 10×D 重建 **> 2min** 或 RSS 撑爆 或 jieba docs/sec 低到全 D 都 >30s | 🔴 **必须持久化 + 补增量 + 崩溃才全量重建**，且全量重建移后台 worker、不阻塞 serve 启动 |
| 任一分词器 **overlap@10 < 0.8** | ⚠️ parity 不达标：换分词器或调 BM25 参；都不行才回头重审 §4 选型 |
| 索引体积 > ~150MB | 🟡 评估磁盘，通常可接受，记进运维账 |

**门槛卡在"10×D 重建是否 < 30s"。jieba 的 docs/sec 是唯一可能把结论推向"必须持久化增量"的变量，务必先单测它。**

---

## 7. parity 回归（拿 DuckDB 当 golden）

- **快照法**：Phase 0 写 `xtask`/测试，固定一组 tenant + 查询（覆盖每桶：过滤/ANN/FTS/hybrid/stats/taxonomy/graph/transcript），用**当前 DuckDB 引擎** dump 结果到 `tests/golden/*.json`（capsule_id 序列、rrf 序、stats 数值、graph 邻居集）。
- **双跑差分**：同 fixture 用 lance-native 引擎重跑，断言：
  - **检索类（集合/顺序）**：比有序 id 列表；FTS/RRF 因引擎差异允许"集合一致 + Kendall-τ ≥ 阈值 / overlap@10 ≥ 0.8"的软断言。
  - **精确类（stats/taxonomy/graph/version-chain）**：逐值/逐集合 exact。
- **开关并跑**：`MEM_READ_ENGINE=duckdb|lance` 让同进程两路跑，parity 测试同时调两路当场 diff——既是验收闸，也是 Phase 1 逐桶推进的门。
- **transcript 大表**：额外抓真实库慢查询/边界（空结果、强 filter、chunk 多 embedding）做回归，防 chunk 折叠（`MIN GROUP BY`）迁到 Rust 后丢/重。

---

## 8. 关键未决 / 待实测清单

### 已解除
1. ✅ **lance 写后可见性**（原 Phase-0 阻断项，2026-06-23 验毕）：探针 `tests/lance_version_visibility.rs`（2/2）实证——`Manual`（默认）需 `checkout_latest()`、`Strong`（`read_consistency_interval(0)`）透明见新、新 `open_table` 总见新。**`refresh/mark_dirty` 可删**；推荐读连接用 `read_consistency_interval(Duration::ZERO)`，除非实测每读 manifest 检查延迟可观才退到「warm 句柄 + dirty + `checkout_latest()`」。该探针同时是 CI 守护：lancedb 升级若改 `ReadConsistency` 语义，断言翻、强制复审。

### 待实测 / 未决
2. 🔴 **decay 写迁移**（不可逆门槛，Phase 2）：`apply_time_decay`/`bump_last_used_at` 改 lancedb Rust update，重证 `with_commit_retry` 的 commit-race 在新写法下成立。
3. 🟠 **jieba docs/sec**（§6 主测变量）：中文分词可能把 Tantivy build 拉高 2-5×，决定「启动重建 vs 持久化增量」。
4. 🟠 **transcript 全量重建耗时**（§6）：现网 ~14 万 block，预判十几秒级，但需按 §6.5 实测 10×D、卡「< 30s」门槛。
5. 🟠 **Tantivy↔Lance 一致性**：双写 hook 的崩溃/重启一致；启动重建 vs 持久化增量由 §6 实测定。
6. 🟡 **RRF compose 退化**：去 fused-SQL 损 14-29% 延迟，本地少并发下绝对值小，需回归确认。
7. 🟡 **上游修复后简化**：长期跟踪 lance changelog，scanner 修好则拆 Tantivy 收敛回单 FTS。

---

## 附：相关文档

- `docs/duckdb-read-path-strategy.md` —— 前序"保留 DuckDB"决策（本篇反向），含 lance 快照-pin 探针与 §3 资源缓解。
- `docs/backend-coupling.md` —— storage 层 backend trait 演进。
- `docs/postgres-backend.md` —— 被否的 Postgres-only 路线参考。
- QW-1（commit/胶囊 2026-05-16）—— `hybrid_candidates` 拆 portable 原语 + bench-driven 降级，本计划 RRF compose 的地基。
