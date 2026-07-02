# mem 胶囊记忆自进化 —— `evolution_worker` 设计（平行工作线 ⑤）

日期：2026-06-11
状态：**Draft（纯设计，未实现）**
参照：EvoMap 动态映射框架 —— Matthe / Ringel / Skiera (2023)，工具箱论文 arXiv:2511.04611（*evomap: A Toolbox for Dynamic Mapping in Python*）
引用约定：实现提交用 `refs evolution-worker E#` / 完结用 `closes evolution-worker E#`（E# 见 §10 里程碑表）

---

## 0. TL;DR

把 active 胶囊看作语义空间里一张**活地图**上的点，常驻 `evolution_worker` 周期性联合布图（聚类 + 簇对齐 + 轨迹追踪），用 **6 个进化算子**（合并固化 / 泛化抽象 / 精化版本化 / 拆分 / 重权重定位 / Hebbian 连边）对记忆做结构性整理。自进化最大的病是**抖动**（jitter）——嵌入噪声和检索波动导致今天合并、明天拆开、簇标签反复横跳。治法移植 EvoMap 的两个核心思想：

1. **联合时间映射**：不独立看单期快照，跨 W 个周期共同估计每个胶囊/簇的"位置"，簇身份跨期对齐；
2. **自适应时间平滑正则**：离散化为**证据积累 + K 周期持续性闸 + 滞回**——任何进化操作只有当其驱动信号足够强**且跨 ≥K 个连续周期持续成立**才执行，证据不足时只累积不动作，信号消失则证据衰减。

落地分两期：**Phase 1 全程零 LLM**（地图层用本仓已有本地嵌入 + 聚类 + 轨迹 + 重权 + Hebbian + keep-longest 合并），**Phase 2 才加生成式合成**（泛化/精化的"写新内容"步骤），做成可插拔后端、默认 OFF、推荐默认走"推迟到 review 由 Claude 写"，保持常驻 worker 永远 LLM-free。

安全范式沿用本仓惯例：default-OFF、`dry_run` 一等公民、verbatim-safe（永不物理删源、永不改 `content`）、产新胶囊必写 lineage 血缘边、全程可审计可回滚。

---

## 1. 背景与问题

### 1.1 现状：生命周期齐了，结构性整理是空白

mem 的单胶囊生命周期已经闭环：feedback 调 `confidence`/`decay_score`、`decay_worker` 时间衰减、`auto_promote_worker` 转正、`idle_archive_worker` 闲置归档、`expires_at` 硬过期、O2 写时近邻标记。但这些全是**单胶囊局部规则**——没有任何机制做**跨胶囊的结构性整理**：

- `mem mine` 长期运行后，episodic 碎片（同一主题的十几条 experience/episode）只会堆积，不会沉淀成一条 semantic 级的通用结论；
- 近似重复靠 `dedup_worker`（默认 OFF）一刀切归档，没有"合并成更完整版本"的路径；
- 图边只进不退化（K9 potentiation 默认 OFF 且只增强不剪枝）；
- 一条胶囊内容跑题/过载时没有拆分机制。

这正是 §「oss-memory-diff」对照里 A-mem（memory evolution）、letta（sleep-time 整理）、graphiti（矛盾闭边）覆盖而 mem 缺失的那一块。

### 1.2 自进化的头号风险：抖动

天真实现（每周期独立聚类→立刻执行操作）必然抖动：

- **嵌入噪声**：重嵌入、chunk 切分变化、provider 升级都会让 cosine 在阈值附近摆动 → 同一对胶囊反复"该合并/不该合并"；
- **检索波动**：某周热点话题让一批胶囊共召回暴涨，下周归零 → Hebbian 边今天建明天废；
- **级联放大**：合并产生的新胶囊改变簇结构 → 触发新一轮误操作。

抖动的代价在 mem 里是不对称的：错过一次合并没成本（下周期再来），**错误执行一次合并/归档则污染 supersede 链与排序信号**。所以设计基调是：**宁可慢，不可抖**。

### 1.3 EvoMap 给的解法（及移植方式）

EvoMap（Matthe/Ringel/Skiera 2023；arXiv:2511.04611 是其 Python 工具箱）解决的是"对一组对象做跨期映射时，如何让地图既反映真实演化又不被采样噪声晃动"：

| EvoMap 原始机制 | 含义 | 移植到 mem（离散算子世界） |
|---|---|---|
| 联合时间映射（joint estimation across periods） | 所有期的位置一起估计，而非逐期独立降维 | 簇身份跨期对齐（与上期簇做 Jaccard 匹配），胶囊轨迹 = 跨期位置序列，而非每期重新发明簇 |
| 时间平滑正则（temporal smoothing penalty） | 惩罚对象跨期大幅移动，除非数据强支撑 | 进化操作不由单期观测触发；每个候选操作维护**证据分**，逐期累积 |
| **自适应**权重（per-object adaptive λ） | 数据少/噪声大的对象被更强地平滑 | 证据要求随对象可靠度自适应：低 `confidence`、嵌入新鲜（刚 re-embed）、簇 churn 高的胶囊需要更多周期的持续信号 |
| 防 jitter 的效果 | 点只有持续信号才移动 | **K 周期闸 + 滞回**：连续 ≥K 期证据达标才执行；执行阈值 > 撤销阈值，避免边界震荡 |

注意：EvoMap 的 MDS/t-SNE 降维是为了**可视化**；我们要的是它的**稳定性机制**而不是降维本身——mem 的"地图"直接活在原始嵌入空间（§3），不做 2D 投影。

---

## 2. 设计原则（安全范式）

全部沿用本仓既有先例，不发明新范式：

1. **default-OFF**：`MEM_EVOLUTION_ENABLED=1` 显式开启（先例：`dedup_worker` 的 `MEM_DEDUP_ENABLED`、K9 的 `MEM_EDGE_DYNAMICS_ENABLED`、O2 的 `MEM_INGEST_NEARDUP_ENABLED`）。
2. **dry-run 一等公民**：每个算子先有 dry-run 再有 live（先例：`idle_archive_sweep(…, dry_run)`、`dedup_worker` 的 `dry_run=true` 返回 would-be 列表、`POST /reviews/auto_promote {dry_run}`）。手动一击入口 `POST /reviews/evolution {dry_run}`。
3. **verbatim-safe**：📦 存储层规则不破——永不改任何既有胶囊的 `content`，永不物理删行。"消失"只有两种合法形态：supersede 链归档（可循 `version_chain` 回看）或 `Archived` 状态（可手动恢复）。
4. **lineage 必写**：所有产新胶囊的算子必须同步写血缘边（§8 谓词表），新胶囊→源胶囊全程可追；回滚 = 归档新胶囊 + 按血缘边恢复源状态（§11）。
5. **常驻进程 LLM-free**：Phase 1 零 LLM；Phase 2 生成式合成默认走 defer-to-review（§6.2），worker 本体任何 Phase 都不直接调大模型。
6. **分层声明**（两轴分层纪律）：本设计落在 🔍 索引/排序/生命周期层（簇、证据、边、confidence/decay）+ ⚙️ infra（worker/配置/HTTP 面）；📦 存储 schema 仅做增量（一张候选表 + 几个新边谓词），不动 `memories.content` 语义。

---

## 3. (A) 语义活地图层（map layer）

### 3.1 输入：复用既有嵌入，不算新向量

地图的点 = 某 tenant 全部 `Active` 胶囊，坐标 = `capability_capsule_embeddings` 里已有的向量（chunked 胶囊取多行，按 `capability_capsule_id` GROUP——与检索同款语义）。嵌入由既有 `embedding_worker` 异步产出（`EmbeddingProvider` trait，`src/embedding/provider.rs:13`；本地 `embed_anything` provider 即 `EmbeddingProviderKind::Local`，`src/config.rs:13`），**evolution_worker 自己永不调 embed**——没有嵌入的胶囊本期跳过（先例：`dedup_worker` 算法第 3 步同款处理）。

### 3.2 每周期：快照 → 聚类 → 跨期对齐

一次 sweep（单 tenant，沿用 `cooccurrence_worker::run(store: Arc<dyn Backend>, settings, tenant)` 的单租户 worker 形态）：

1. **快照**：拉 active 胶囊 id + 元数据 + 嵌入（capped at `scan_limit`，先例同 `dedup_worker`/`cooccurrence_worker`）。
2. **聚类**：嵌入空间内 union-find on pairwise cosine ≥ `cluster_threshold`（直接复用 `dedup_worker` 已实现的 union-find 骨架，阈值放宽——dedup 找的是镜像重复 ~0.95+（E2 起；曾 0.92），地图簇找的是同主题 ~0.80 量级）。MVP 用 union-find 够了；簇质量不满意再换 HDBSCAN（纯 Rust 实现存在，留为开放问题 §12）。
3. **跨期对齐**（联合映射的离散化）：本期每个簇与上期簇做成员 Jaccard 匹配，≥0.5 视为同一簇延续（继承 `cluster_id`），否则新簇。由此每个簇有了跨期身份，可计算：
   - **簇涌现**：新 `cluster_id` 连续 ≥K 期存在且规模 ≥`min_cluster_size`；
   - **簇稳定性**：成员 churn 率（对称差/并集）的滑动均值；
   - **胶囊轨迹**：胶囊的簇归属序列 + 同一胶囊跨期嵌入 cosine 漂移（捕捉 supersede/re-embed 引起的语义移动）。
4. **算子评估**：把观测喂给 §4 各算子的触发器，更新候选证据（§3.3），证据达标的候选进入执行（live）或报告（dry-run）。

### 3.3 防抖核心：持久化的证据积累 + K 周期闸 + 滞回

每个候选操作（如"簇 #17 的 5 条胶囊应合并"）是一条**持久化候选记录**——进程重启不清零证据。

```
E_t(op) = β · E_{t-1}(op) + s_t(op)        s_t ∈ {0,1}: 本期信号是否成立
执行条件: 连续达标期数 ≥ K  且  op 的参与方集合与首次提出时 Jaccard ≥ 0.8
撤销条件(滞回): 信号缺席期 E_t = β·E_{t-1} 衰减、连续计数清零；E_t < hysteresis(默认 0.5)
  才撤销候选——撤销地板远低于执行门槛，边界信号不会建了又撤反复横跳
自适应(EvoMap 的 per-object λ): 参与方含低 confidence(<0.5)、刚 re-embed(content_hash 变更
  未满 1 期)、或所在簇 churn > 0.3 的，K 上调(默认 +1)——噪声越大要求越久(E1 未实现，后续)
```

> **E1 实现注**（`src/evolution/map.rs`）：原稿执行条件含 `E_t ≥ K`，与本文自己的验收
> 标准矛盾——β=0.7 时连续 3 期信号 E₃=2.19 < 3，「3 期触发」永远不成立。E1 落地为：
> **执行只看连续达标期数 ≥ K**；E 只用于滞回撤销。中断后连续计数清零，必须重新攒满 K 期。

持久化载体两案取一（→ §8.2，**推荐 b**）：

- (a) 图边方案：复用 O2 `suspected_supersede` 先例，写 `suspected_merge` 等候选边，证据计数放边属性——优点零新表，缺点多方操作（5 条胶囊合并）要表达成边集，更新别扭；
- (b) **新 Lance 表 `evolution_candidates`**（类比 `embedding_jobs`）：一行一个候选操作，JSON 存参与方与参数，列存 `evidence`、`consecutive_cycles`、`status(pending|ready|executed|cancelled)`——单行原子更新，审计直观。

---

## 4. (B) 六个进化算子

总览（执行产物全部遵守 §2 安全范式）：

| # | 算子 | 一句话 | Phase 1 | Phase 2 | lineage 边 |
|---|---|---|---|---|---|
| ① | merge 合并固化 | 近重复簇收敛为一条 canonical | ✅ keep-longest | 生成式合成版 | `merged_into` |
| ② | ★generalize 泛化抽象 | N 条 episodic → 1 条 semantic 通则 | 检测 + 入 review | 生成（默认 defer-to-review） | `generalizes` |
| ③ | refine 精化版本化 | 矛盾/过时但高价值 → 修订新版本 | 检测 + 入 review | 生成（默认 defer-to-review） | supersede 链 + `refined_from` |
| ④ | split 拆分 | 一条胶囊载多主题 → 拆 N 条 | 检测 + 入 review | 生成（默认 defer-to-review） | `split_from` |
| ⑤ | reweight 重权重定位 | 按簇信号调 confidence/decay/topics | ✅ 全量 | — | 无新胶囊（feedback_events 留痕） |
| ⑥ | Hebbian 连边 | 共召回成边、弱边退役 | ✅ 全量 | — | `co_recalled_with` |

### ① merge —— 合并固化

- **触发**：同簇内 pairwise cosine ≥ `merge_threshold`（默认 0.88，介于地图簇 0.80 与 O2 近重复 0.92 之间）的子团，且成员同 `(tenant, project)`、无互相 supersede 关系、证据过 K 周期闸。`Preference`/`Workflow` 守护型胶囊**排除**（先例：auto-promote 同款排除）。
- **Phase 1 行为（keep-longest）**：选 `content` 最长者为 canonical（平局取最老 `created_at`——完全沿用 `dedup_worker` 的选择函数），其余成员退出活跃池并写血缘边。
  > **E1 实现注**（`src/worker/evolution_worker.rs::execute_merge`）：原稿设想走 supersede 链，但 `supersedes_capability_capsule_id` 是**单值指针**（挂在取代者身上），N 个 loser 合并到一个 canonical 在该字段上不可表达。E1 落地为：loser 经 `set_capsule_status → Archived`（通用状态原语，行保留可恢复——刻意**不用** dedup 的 `FeedbackKind::Incorrect` 路径，那个语义是"错了"），lineage 由 `merged_into` 边承载（loser→canonical，`extractor="evolution"`）。**`merged_into` 边就是 merge 的血缘机制**，Phase 2 合成新胶囊时同样如此。dedup 与 merge 的职权重叠仍按 §12 处理。
- **Phase 2 行为**：合成后端（§6.2）把成员内容合成一条新胶囊（type 沿用成员多数派），新胶囊为 canonical，全部成员 supersede 到它。
- **lineage**：每个被并成员 → canonical 写 `merged_into` 边（supersede 链之外再显式留图证据，供 `kg_*` 工具溯源）。

### ② ★generalize —— 泛化抽象（episodic→semantic，本设计的灵魂算子）

- **触发**：稳定簇（连续 ≥K 期、churn < 0.2）内 ≥`generalize_min_n`（默认 4）条 `experience`/`episode`/`diary` 型胶囊，共享 ≥2 个实体（经 `EntityRegistry` 解析的 `topics` 交集——复用 `cooccurrence_worker` 第 2 步的实体集构造）或共享谓词（`fact_check` 的 `RelationshipTriple` 抽取面），且簇内平均 `confidence` ≥ 0.6。
- **语义要点**：泛化**不取代**具体经验——源胶囊保持 `Active`（个例在调试场景仍是最准的证据），新 semantic 胶囊与源并存；这与 ① 的 supersede 语义根本不同。
- **Phase 1 行为**：只检测。产出一条 `PendingConfirmation` 状态的**占位候选**进 review 队列（`capability_capsule_list_pending_review` 既有面直接可见），`content` 为结构化原料清单（成员 id + 各自 summary + 共享实体），**不含任何生成文本**。reviewer（人或交互中的 Claude）用既有 `review_edit_accept` 写入真正的通则内容并转正，或 `review_reject` 丢弃。
- **Phase 2 行为**：合成后端直接生成通则文本；`synthesis=review`（推荐默认）时行为与 Phase 1 相同——这是"Phase 1 行为即 Phase 2 的 review 后端"的设计闭合。
- **lineage**：新 semantic 胶囊 → 每个源写 `generalizes` 边。

### ③ refine —— 精化版本化

- **触发**：单胶囊同时满足——(i) 矛盾信号：`FactCheckService` 三元组冲突，或身上挂 `suspected_supersede` 边（O2 产物）持续未处理，或累计 `outdated` 反馈 ≥2；(ii) 价值信号：`last_recalled_at` 近 30 天内（高使用——值得修，不值得修的让 idle-archive/decay 自然处理）。
- **Phase 1**：检测 + 入 review（同 ② 的占位机制，原料 = 旧内容 + 冲突证据清单）。
- **Phase 2**：合成修订版，走标准 supersede 链（旧版自动从检索候选退出——`tests/version_chain_dedup.rs` 已验证的既有行为），另写 `refined_from` 边标注这是 evolution 产物而非人工 supersede。

### ④ split —— 拆分

- **触发**：胶囊的 **chunk 嵌入**（既有 `upsert_capability_capsule_embedding_chunks` 产出的多行向量）在地图上散布到 ≥2 个不同簇，且两簇 cosine 距离 > `split_threshold`，持续 K 期。这是 chunked embeddings（长内容召回工作线 ③）带来的免费检测信号——长胶囊跑题在向量层直接可见。
- **Phase 1**：检测 + 入 review（原料 = 按 chunk 归簇的切分建议）。
- **Phase 2**：按簇切分生成 N 条新胶囊，原胶囊被 N 条共同 supersede（链上多对一，`version_chain` 已支持），各新胶囊写 `split_from` 边。

### ⑤ reweight —— 重权重定位（纯 Phase 1）

- **触发与动作**（全部有界、低频、走可审计通道）：
  - 簇稳定 + 高召回（簇内 `last_used_at` 活跃占比 > 0.5）→ 簇成员 `confidence` 微升（+0.02/周期，上限 0.9）——**通过既有 `feedback_events` 通道写**（新增系统侧 `kind`，或复用 `applies_here` 加 `note="evolution:cluster_stability"`，取舍见 §12），保证与人工反馈同一审计流；
  - 孤点（连续 K 期不属于任何簇）且零召回 → `decay_score` 微增（+0.05/周期）——加速其滑向 idle-archive，**不直接归档**（归档仍是 `idle_archive_worker` 的职权，evolution 只调信号，单一职责）;
  - **重定位**：胶囊的 `topics` 与所在簇主流实体集偏离（交集为空且簇稳定）→ 产出 topics 修订建议进 review（Phase 1 不直接改 topics——它是 caller-supplied 输入，改写需要人确认）。

### ⑥ Hebbian 连边（纯 Phase 1）

本仓已有两层 Hebbian 基建，本算子只补缺口：

- **已有**：K9 `potentiation_worker`（retrieve 端共访问事件 → channel → `Store::potentiate_edge` 增强**既有边**权重，默认 OFF）；K10 `cooccurrence_worker`（实体↔实体 `cooccurs_with` 边）。
- **缺口 1 —— 胶囊↔胶囊共召回成边**：K9 只增强已存在的边，不创造新边。本算子消费同一事件流（或直接读 `last_used_worker` 的批次——同批 `bump_last_used_at` 的 id 集合即一次共召回），对共召回 ≥`min_corecall` 次（跨 ≥K 期）的胶囊对写 `co_recalled_with` 边（`extractor="evolution"`，先例：cooccurrence 的 `extractor="cooccurrence"`）。新边进入 O4 graph-boost 的 1-hop 扩展，直接抬升检索连带召回。
- **缺口 2 —— 弱边退役（potentiation 的逆操作）**：`co_recalled_with` 边连续 `prune_idle_cycles` 期无共访问事件 → 置 `valid_to`（时序图既有失效语义，**不删行**——点时查询 `neighbors_within(…, as_of)` 仍可回看历史）。只退役 evolution 自产的边，**永不动** caller 边与 `user_tunnel:` 边。

---

## 5. (C) 驱动信号与适应度

每个算子的触发器从同一张信号面取数。全部信号都已存在或由 §3 地图层新增：

| 信号 | 来源（已核实） | 喂给算子 |
|---|---|---|
| 召回频率 / 新近度 | `last_used_at` / `last_recalled_at` 列（`src/domain/capability_capsule.rs:242` 起），`bump_last_used_at`（`src/storage/lance_store/decay.rs:120`），`last_used_worker` 批次 | ③④⑤⑥ |
| 共召回 / 共访问 | K9 事件 channel（`src/worker/potentiation_worker.rs`）、`cooccurs_with` 边 | ⑥ |
| 显式反馈 | `confidence`（useful/applies_here 累积）、`feedback_events` 审计行、`FeedbackKind::Incorrect→Archived` | ②③⑤ |
| 矛盾检测 | `FactCheckService` / `RelationshipTriple`（`src/service/fact_check_service.rs`）、O2 `suspected_supersede` 边（`flag_if_near_duplicate`，`src/worker/embedding_worker.rs`） | ③ |
| 簇密度 / 稳定性 / 轨迹 | **新增**：§3 地图层（churn、Jaccard 对齐、嵌入漂移） | ①②④⑤ |
| 衰减 / 闲置 / 过期 | `decay_score`（`apply_time_decay`，`src/worker/decay_worker.rs:20`；`decay_delta`，`src/domain/capability_capsule.rs:123`）、`idle_archive_sweep`、`expires_at`（`is_expired`，`src/pipeline/retrieve.rs:75`） | ⑤ 及全体的"不碰垂死者"前置过滤 |

**适应度的形态**：不搞单一全局 F(c) 标量——六个算子各有触发器，共享的是 §3.3 的同一套证据积分/K 闸/滞回机制。全局标量适应度是过度设计：信号语义异质（矛盾≠低召回≠簇游离），压成一个数反而丢失"该用哪个算子"的信息。

**与遗忘的关系**：遗忘**不是**本 worker 的算子——decay/idle-archive/expiry 三件套已经是完善的遗忘机制。evolution 与它们是单向协作：⑤ 调信号加速/减缓既有遗忘通道，且所有算子用"垂死过滤"（`decay_score > 0.8` 或已过期者不参与任何进化——别在将死的记忆上浪费合成预算）。

---

## 6. (D) 两期落地

### 6.1 Phase 1 —— 零 LLM，全功能地图 + 三个全量算子

能跑的完整闭环，不接任何大模型：

- 地图层全量（§3）：本地嵌入（`EmbeddingProviderKind::Local` = embed_anything，已是默认 provider 路线）+ union-find 聚类 + 跨期对齐 + 证据机制；
- ① merge 的 keep-longest 形态（无生成，纯选择 + supersede 链）；
- ⑤ reweight、⑥ Hebbian 全量;
- ②③④ 的**检测 + 入 review** 形态——结构化原料进 `PendingConfirmation` 队列，内容由 review 端补全。

Phase 1 的验收基线：worker 进程的依赖面 = 现状（嵌入已在 `embedding_worker` 异步管线里），新增计算只有聚类（O(n²) cosine within scan_limit——`dedup_worker` 同量级，已被接受）。

### 6.2 Phase 2 —— 生成式合成，可插拔、默认 OFF

新增 trait（⚙️ 层，形态类比 `EmbeddingProvider`）：

```text
SynthesisBackend::synthesize(op: SynthesisTask) -> SynthesizedContent
  op ∈ { Merge{sources}, Generalize{sources, shared_entities},
         Refine{source, conflicts}, Split{source, chunk_plan} }
```

三个实现，`MEM_EVOLUTION_SYNTHESIS` 选择，**默认 `off`**：

| 取值 | 实现 | 说明 |
|---|---|---|
| `off`（默认） | 无 | ②③④ 保持 Phase 1 的检测+review 形态 |
| `review`（**推荐开启值**） | 不调任何模型 | 与 Phase 1 检测形态同构：合成任务挂 review 队列，由交互中的 Claude / 人用 `review_edit_accept` 写内容。**worker 全程 LLM-free**，合成质量 = 前台大模型质量，零额外密钥/进程 |
| `local` | 本地小模型（如 GGUF 经 llama.cpp binding） | 全离线自动合成；引入推理依赖与质量风险，仅在 review 通道吞吐不够时考虑 |
| `api` | 外接 API（OpenAI 兼容面，复用嵌入侧已有的 OpenAI 配置形态） | 质量最高；违背 local-first 默认，必须显式配置 |

`review` 后端是设计的闭合点：**Phase 1 的行为就是 Phase 2 的 review 后端**——两期之间没有迁移成本，只有能力解锁。生成产物无论哪个后端，落库路径唯一：占位胶囊 → review 面（accept / edit_accept / reject）→ 标准 supersede/lineage 写入。自动后端（local/api）产物也强制过 `PendingConfirmation`，**永不直接 Active**（先例：O2 near-dup 同款"标记待审，verbatim-safe 不自动合并"哲学）。

---

## 7. (E) 复用清单（已用 codegraph 核实的真实符号）

| 原语 | 符号 | 位置 | 在本设计中的角色 |
|---|---|---|---|
| Backend supertrait | `Backend`（10 子 trait） | `src/storage/backend.rs:32` | worker 持 `Arc<dyn Backend>`（与全部既有 worker 同款） |
| 嵌入 provider | `EmbeddingProvider` / `embed_batch` / `arc_embedding_provider` / `EmbeddingProviderKind` | `src/embedding/provider.rs:13` / `:34` / `src/embedding/instance.rs:10` / `src/config.rs:13` | 地图坐标来源（只读已有向量，不调用 embed） |
| 胶囊嵌入读写 | `EmbeddingVectorStore`（含 `upsert_capability_capsule_embedding_chunks`、`get_capability_capsule_embedding_vector`） | `src/storage/embedding_vector_store.rs:22` | 拉向量；chunk 多行 = ④ split 的检测信号 |
| 图边 schema + 写 | `graph_edges_schema` / `sync_memory_edges` / `add_edge_direct` / `invalidate_edge` / `close_edges_for_capability_capsule` | `src/storage/lance_store/mod.rs:1138` / `src/storage/lance_store/graph.rs` | lineage 边、⑥ 新边、弱边置 `valid_to` |
| 图读 / 点时查询 | `neighbors_within(node, max_hops, as_of)`（`MAX_HOPS_CAP=3`）、`graph_stats` | `src/storage/lance_store/graph.rs` | 回滚溯源、审计 |
| supersede / 版本链 | `capability_capsule_supersede`、`supersedes_capability_capsule_id`、`version: i64`、`version_chain` | `src/mcp/server.rs:1120`、`src/domain/capability_capsule.rs:182/:205/:426`、`tests/version_chain_dedup.rs` | ①③④ 的取代写路径（检索自动排旧版，已有测试覆盖） |
| 近重复聚类骨架 | `dedup_worker`（union-find + keep-longest + dry_run，`MEM_DEDUP_ENABLED` 默认 OFF） | `src/worker/dedup_worker.rs` | ① 的算法地基；merge 落地后 dedup 降级为镜像重复专用（§12） |
| 实体共现 | `cooccurrence_worker::run/sweep_once`、`CooccurrenceSettings`、`cooccurs_with` 边 | `src/worker/cooccurrence_worker.rs:52/:77`、`src/config.rs:293` | ② 共享实体集构造；⑥ 的同族先例 |
| Hebbian 增强 | K9 `potentiation_worker`、`Store::potentiate_edge`、`MEM_EDGE_DYNAMICS_ENABLED` | `src/worker/potentiation_worker.rs` | ⑥ 事件流复用；本算子补"建新边 + 剪弱边" |
| 跨项目隧道 | `topic_tunnel_worker`（`user_tunnel:topic:` 边） | `src/worker/topic_tunnel_worker.rs` | 边命名/extractor 先例；其 `user_tunnel:` 边在 ⑥ 剪枝豁免名单 |
| 矛盾检测 | `FactCheckService` / `RelationshipTriple` / `POST /fact_check` | `src/service/fact_check_service.rs`、`src/http/fact_check.rs:15` | ③ 触发信号 |
| 衰减 | `start_decay_worker` / `apply_time_decay` / `DECAY_RATE_PER_DAY` / `decay_delta` | `src/worker/decay_worker.rs:11/:20/:8`、`src/storage/lance_store/decay.rs:32`、`src/domain/capability_capsule.rs:123` | ⑤ 的作用对象；垂死过滤 |
| 检索强化时间戳 | `last_used_at` / **`last_recalled_at`**（idle 判定专用）/ `bump_last_used_at` / `last_used_worker` | `src/domain/capability_capsule.rs:237–248`、`src/storage/lance_store/decay.rs:120`、`src/worker/last_used_worker.rs` | 召回频率信号；⑥ 共召回批次来源 |
| 闲置归档 | `idle_archive_sweep(…, dry_run)` / `POST /reviews/idle_archive` / `idle_archive_worker` | `src/service/capability_capsule_service.rs:970`、`src/http/review.rs:21` | 遗忘协同（⑤ 只调信号不归档）；dry-run/HTTP 面先例 |
| 硬过期 | `expires_at` / `is_expired` | `src/pipeline/retrieve.rs:75`、`tests/expiry.rs` | 垂死过滤 |
| 写时近邻标记（O2） | `flag_if_near_duplicate` / `suspected_supersede` 边 / `MEM_INGEST_NEARDUP_*` | `src/worker/embedding_worker.rs` | ③ 触发信号；候选边谓词命名先例 |
| review 面 | `capability_capsule_list_pending_review` / `review_accept` / `review_edit_accept` / `review_reject`、`POST /reviews/auto_promote {dry_run}` | `src/mcp/server.rs`、`src/http/review.rs` | ②③④ 占位候选的全部人机界面（零新 UI） |
| 转正 sweep | `auto_promote_worker`（`Preference`/`Workflow` 排除先例） | `src/worker/auto_promote_worker.rs` | ① 的守护型排除规则同源 |

---

## 8. 数据模型增量

### 8.1 新边谓词（`graph_edges` 表零 schema 变更，纯新谓词值）

| 谓词 | 方向 | 写入时机 | 备注 |
|---|---|---|---|
| `merged_into` | 被并成员 → canonical | ① 执行 | supersede 链外的显式图证据 |
| `generalizes` | semantic 新胶囊 → 各源 | ② 执行 | 源保持 Active |
| `refined_from` | 新版本 → 旧版本 | ③ 执行 | 与 supersede 并行存在，标注 evolution 来源 |
| `split_from` | 各新胶囊 → 原胶囊 | ④ 执行 | |
| `co_recalled_with` | 胶囊 ↔ 胶囊（sorted id 定向，先例 K10） | ⑥ 执行 | `extractor="evolution"`；可被剪枝（置 `valid_to`） |

### 8.2 新表 `evolution_candidates`（Lance 表，类比 `embedding_jobs`）

```sql
id TEXT PRIMARY KEY,           -- uuidv7
tenant TEXT NOT NULL,
op_kind TEXT NOT NULL,         -- merge|generalize|refine|split|reweight|hebbian
member_ids TEXT NOT NULL,      -- JSON array，首次提出时的参与方
params TEXT NOT NULL,          -- JSON，算子参数(阈值快照、簇 id、切分计划…)
evidence REAL NOT NULL,        -- E_t
consecutive_cycles INTEGER NOT NULL,
status TEXT NOT NULL,          -- pending|ready|executed|cancelled
first_proposed_at TEXT, last_signal_at TEXT, executed_at TEXT,
result_capsule_ids TEXT        -- JSON，执行产物(回滚入口)
```

不加任何胶囊列——`version`/`supersedes_capability_capsule_id`/`last_recalled_at`/`expires_at` 已够用。簇快照不落盘（每期重算 + 候选行里存簇 id 引用即可；落盘簇历史是过度持久化）。

---

## 9. 配置面（`EvolutionSettings`，形态类比 `CooccurrenceSettings`/`DedupSettings`）

| env | 默认 | 说明 |
|---|---|---|
| `MEM_EVOLUTION_ENABLED` | **OFF** | 总开关 |
| `MEM_EVOLUTION_INTERVAL_SECS` | 86400（日级） | 周期即"期"；进化是慢过程，别学 embedding worker 的秒级 |
| `MEM_EVOLUTION_K_CYCLES` | 3 | K 周期闸（自适应场景 +1） |
| `MEM_EVOLUTION_EVIDENCE_DECAY` | 0.7 | β |
| `MEM_EVOLUTION_HYSTERESIS` | 0.5 | 撤销阈值系数 |
| `MEM_EVOLUTION_CLUSTER_THRESHOLD` | 0.80 | 地图簇 cosine |
| `MEM_EVOLUTION_MERGE_THRESHOLD` | 0.88 | ① 子团 cosine |
| `MEM_EVOLUTION_SCAN_LIMIT` | 2000 | 先例同 dedup/cooccurrence |
| ~~`MEM_EVOLUTION_OPS`~~ | — | E1 取消此 env：①② 两算子随 sweep 恒启用（执行已被 K 闸 + enabled + dry_run 三重把守），算子位图等 ③④⑤⑥ 落地时再引入 |
| `MEM_EVOLUTION_SYNTHESIS` | `off` | `off\|review\|local\|api`（§6.2） |
| ~~`MEM_EVOLUTION_DRY_RUN`~~ | — | E1 取消此 env：dry-run 是 `POST /reviews/evolution {dry_run:true}` 的请求级语义，且为**完全零写入**（候选也不写——证据只在真实 sweep 中积累），比原稿「只写候选不执行」更保守 |

HTTP 手动一击：`POST /reviews/evolution {dry_run: bool, ops?: [...]}`（先例：`/reviews/auto_promote`、`/reviews/idle_archive`），返回本期观测 + 候选清单 + （live 时）执行结果。无效 env 值静默回默认（仓内惯例）。

---

## 10. (G) MVP 切片与里程碑

**MVP 主线 = ①合并 + ②泛化一条线 + dry-run 预览**——理由：① 是地基复用最多、风险最低的 live 算子；② 是价值最高的灵魂算子且 Phase 1 形态（检测+review）零生成风险；两者共用地图层，一条线打穿"观测→证据→执行→review→lineage→回滚"全部机制。

| E# | 里程碑 | 内容 | 验收 |
|---|---|---|---|
| E1 ✅(2026-06-11 实现并验收，commit acf2ea2，含 ①② 执行路径) | 地图层+①②一条线 | 快照+聚类+跨期对齐+证据表落库；`POST /reviews/evolution {dry_run:true}` 输出簇报告与候选 | 集成测试：构造 3 期合成数据，簇对齐正确、K 闸生效（2 期信号不触发、3 期触发）、重启证据不丢 |
| **E1.5 ✅**(2026-06-12 落地并验收，commit e23a1b5，(a) 路线) | **解锁②泛化：共享主题信号**（前置依赖，§12.6.5） | 线上 265 条胶囊 `topics` 全空 → ② 结构性沉默。两条路：(a) **推荐·短期**——`detect_generalize` 的共享信号改 `topics ∪ tags`（lowercased 并集；实测 2026-06-11：255/265 条有 ≥2 个 tags 且值有语义如 rust/tools4a/dlhs/performance，立刻可触发，改动面只在 worker 检测函数 + 单测）；(b) **中期**——ingest 侧补 topics 抽取（实体/关键词启发式，零 LLM），绑 ROADMAP #20「内容抽实体」一起做，tags 路线作为其落地前的过渡。两路不互斥，(a) 先行。**落地注**：检测与执行共用 `member_themes`/`shared_themes` helper（同一份归一化逻辑，防提议/落库主题集不一致）；(b) 中期路线仍开放、绑 #20 | 验收（已过）：①线上副本 dry-run ② 恰产出 1 条合理提议（同 tags 簇）、无跨主题误聚；②tags-only / 大小写归一 / 不相交主题守卫三形态单测（`tests/evolution.rs`）；③零 LLM；④① merge 行为回归不变 |
| **E1.6 ✅**(2026-06-12 随立随修) | **auto-promote 绕过 review 闸**（漏洞修补） | 线上 `auto_promote` 默认 ON（age≥3d 的 pending experience 自动转正），而 ② 泛化占位胶囊正是 PendingConfirmation 的 experience——若 3 天内没人 review，**原料占位文会被自动转正进活跃池**，绕过 §6.2「产物强制过 review、永不直接 Active」的设计闸。修复：`auto_promote_worker::sweep_once` 按 `source_agent == EVOLUTION_SOURCE_AGENT`（evolution_worker 导出的共享常量，所有进化产物统一打标）过滤候选，Rust 侧过滤保证 dry-run 预览与真实路径永不分叉 | 验收（已过）：同龄同型双胶囊，evolution 来源的 preview/real 双路均不晋升、对照组正常晋升；原有 auto-promote 测试回归不变 |
| **E2 ✅**(2026-07-02 形式验收补齐；执行路径/滞回随 E1 已在跑，线上已真实执行过 merge) | ① merge live | keep-longest + `merged_into` 边 + 滞回（E1 已载）；本期补：`tests/evolution_merge.rs` 验收 + §11 回滚 + §12.1 决策落地（dedup 默认阈值 0.92→**0.95** 收窄为镜像专用，`config.rs::DedupSettings`） | 验收（已过）：合并后 `search_candidates` 只回 canonical（version_chain_dedup 断言形态）、loser 行保留 Archived；dry-run 与 live 集合逐 id 一致；回滚可还原——**实现注**：回滚做成 `POST /reviews/evolution/rollback`（service+HTTP）而非原稿的 CLI 一击，因为 `mem serve` 是数据集单写者，第二个直连 store 的写进程会打架；`evolution_worker::rollback_candidate` 按候选行反做（merge: loser→Active + `invalidate_edge` 关 `merged_into`；generalize: 占位→Archived + 关全部边），候选行留 `rolled_back` 墓碑 |
| **E3 ✅**(2026-07-02) | ② generalize（review 形态） | 稳定簇检测 + 占位候选进 pending review（E1 已载）；本期补 review 环闭合：reject→候选 `rejected` + 关占位边；edit_accept→血缘重写到 successor | 验收（已过，`tests/evolution_review.rs`）：list→edit_accept→新 semantic 胶囊 Active、源不动、**successor 重持有 4 条 `generalizes` 边**；reject→候选 `executed`→`rejected`（executed-history 抑制解除）且新占位 K 期内不进 review、K 期后闸门重开——**实现注**：「`generalizes` 边 accept 时写」的真实形态 = 提案时写在占位上（E1 行为保留）+ reject 时随胶囊关边 + edit_accept 时从占位 `evidence`（=源 id 清单）重写到新 id 的 successor；因 edit_accept 铸新 id，纯 accept-时-写无法覆盖占位被拒的审计需求。候选状态机扩为 pending/executed/cancelled/`rejected`/`rolled_back` |
| **E4 ✅**(2026-07-02) | ⑤ reweight + ⑥ Hebbian | feedback_events 通道调权；`co_recalled_with` 建边+剪枝 | 验收（已过，`tests/evolution_dynamics.rs`）：调权逐条落 feedback_events（新系统侧 kind `system_reweight_up`(+0.02, 0.9 发射侧封顶)/`system_reweight_decay`(+0.05)，§12.2 定夺——两个 typed 变体而非单个，metrics 分桶编译期强制；公共 feedback API 拒收系统 kind）；剪枝只动 `extractor="evolution"` 边、`user_tunnel:*`/caller 边豁免——**实现注**：①⑤走候选表但 **recurring**（执行后不进 `executed`，闸门开着每信号周期 +δ，静默照常滞回 cancel）；②⑥的共召回信号 = 同 `last_used_at` 批次戳（last_used_worker 每次 flush 同戳），候选 `params` 携带 batch_ts 做**新鲜度闸**（同戳重见 = 静默不累积）；③剪枝闲置基准保守取 max(边生辰, last_activated, 两端点各自 last_used_at)——无 per-pair 共访事件日志，单独活跃的端点保边不误剪；④corecall 豁免 executed-history 抑制（边被剪/回滚后可重赚）、rollback 支持（关边）；⑤「topics 重定位建议进 review」**deferred**——review 面缺「修改他胶囊」的 accept 动作类型，绑 E5 后评估；⑤ 的 §11 回滚 = note 反查 feedback_events 逆施加（recurring 无单一 executed 态可回滚） |
| **E5 ✅**(2026-07-02) | Phase 2 合成后端 | `SynthesisBackend` trait + `review` 后端（E1 已载；`SynthesisTask` 本期扩 `Refine`/`Split` 变体）+ ③④ 检测器 | 验收（已过，`tests/evolution_refine_split.rs`）：`synthesis=off→review` 切换产出逐字节相同的占位胶囊；③④ 全流程——**实现注**：①③ 冲突信号实现了两路（挂身 `suspected_supersede` 入边 ∨ 累计 `outdated`≥2），fact_check 三元组冲突路**未接**（无三元组语料可测，留待有真实 fact 语料后补）；②③ 价值门 = `last_recalled_at` 30 天窗口，**先过价值门再查 feedback**（把 per-capsule feedback 读限定在热集合）；③④ 占位原料守 verbatim 规则——引 id+summary+证据清单，**不拷源 content**（偏离 §4③「原料=旧内容」的字面，沿 ② 先例）；④ 分离度 = 簇内 union-find（`cluster_threshold` 同地图几何）分出 ≥2 组 **且** 全部跨组 chunk 对 cosine ≤ `split_threshold`（默认 0.5，`MEM_EVOLUTION_SPLIT_THRESHOLD`）——轻微内部漂移（如 45° ≈0.707）不拆；⑤ chunk 读为此新增 `EmbeddingVectorStore::get_capability_capsule_embedding_chunks`（lance/pg/ch 三后端），sweep 的地图向量 = 首 chunk（查询数不变）；⑥ ③④ 的 Phase 1 产物**不动源**——accept 后由 reviewer 显式 supersede/archive 源（「修改他胶囊」的 review 动作类型仍缺，同 ⑤ retopic 的 defer 原因）；edit_accept 血缘按占位 tag 分流（generalizes/refined_from/split_from）；rollback 三算子共享占位臂 |
| E6（LATER） | `local`/`api` 后端 | 自动合成，产物仍强制过 review | 需求出现再做 |

每个 E# 完成 → 回本表打 ✅ 并在 ROADMAP.MD「平行工作线」加 ⑤ 行（commit 引用 `closes evolution-worker E#`）。

---

## 11. 回滚与审计

- **审计**：候选表全生命周期留痕（proposed→ready→executed，含 evidence 轨迹）；执行产物 id 记 `result_capsule_ids`；⑤ 走 feedback_events；所有边带 `extractor="evolution"` 可一键圈出。
- **回滚单元 = 一条 executed 候选**：
  - ①③④：归档产物胶囊 + 按 `version_chain` 把被 supersede 成员的链头还原（supersede 是状态指针不是删除，逆操作良定义）+ `invalidate_edge` 关 lineage 边；
  - ②：归档 semantic 新胶囊 + 关 `generalizes` 边（源本来就没动过）；
  - ⑤：feedback_events 有逆向量（按 note 圈出后反向施加）；
  - ⑥：边置 `valid_to`（点时查询可回看）。
- ✅ `POST /reviews/evolution/rollback {tenant, candidate_id}`（E2 落地，2026-07-02）：直接做了 HTTP 面而没走「CLI 一击」过渡——`mem serve` 是 Lance 数据集的单写者，独立 CLI 进程直连 store 会与在跑的 serve 冲突。unknown / 非 executed 候选返回 400。
- **永不物理删**：任何路径都不删行、不删边、不改 content——回滚后的世界与执行前在检索语义上等价，在审计语义上多一段历史。

## 12. 风险与开放问题

1. **与 `dedup_worker` 的职权重叠**：~~① merge 落地后建议 dedup 收窄为 cosine ≥0.95 的镜像重复专用（或直接退役并入 ①），避免两个 worker 对同一对胶囊做出"归档 vs 合并"的竞争决策。定夺放 E2。~~ ✅ E2 定夺（2026-07-02）：**收窄不退役**——`DedupSettings` 默认阈值 0.92→0.95（`MEM_DEDUP_THRESHOLD` 仍可覆写），0.88–0.95 带交给 ① merge；退役是不可逆动作，观察一个周期 ① 覆盖良好后再评估。
2. **⑤ 的 feedback 通道污染**：~~用 `applies_here + note` 会混入人工反馈统计；新增系统侧 `FeedbackKind`（如 `system_reweight`）更干净但动 domain 枚举。倾向后者，E4 定夺。~~ ✅ E4 定夺（2026-07-02）：新增**两个** typed 系统侧变体 `SystemReweightUp` / `SystemReweightDecay`（单个 `system_reweight` 无法把 δ 语义留在 `FeedbackKind::{confidence,decay}_delta` 的单一真相点）；`FeedbackKind::is_system()` 让公共 feedback API（HTTP/MCP `submit_feedback`）统一拒收系统 kind（顺带把 `AutoPromoted`「never sent by submit_feedback」的文档承诺变成了强制）；metrics/`FeedbackSummary` 各得独立分桶（typed 路由 → 编译期强制补全）。
3. **聚类算法升级**：union-find 单链效应在大簇上会"桥接"不相干主题；HDBSCAN 纯 Rust 生态可选项待调研（E1 后评估簇质量再决定）。
4. **多租户**：沿用单租户 worker（env 指定）先例；多租户 fan-out 与 `auto_promote_worker` 一起将来统一解决。
5. **O(n²) 上界**：scan_limit=2000 下每期 ~2M 次 cosine，可接受；corpus 上 10k 后需要 ANN 预筛（Lance 已有 IVF_PQ，候选对生成可改走 `lance_vector_search` top-k）——记为 scale 路标，不在 MVP。
6.5. **②泛化在当前线上语料不可触发（E1 dry-run 实测，2026-06-11）**：live 实例 265 条胶囊的 `topics` **全部为空**——`mem mine` / hook ingest 路径从不填 `topics`，而 ② 的触发条件要求 ≥2 共享 topics，因此泛化算子在现有语料上永远沉默。两条出路：(a) ingest 侧补 topics 抽取（牵动 §「内容抽实体」ROADMAP #20）；(b) ② 的共享信号改用/兼容 `tags`、实体注册表或图边邻接。E2/E3 验收前必须解决，否则灵魂算子上线即休眠。另注：E1 的 episodic 集合实际为 `Experience|Episode`——`Diary` 被 `list_capability_capsules_for_tenant` 在存储层排除，不进地图。
6. **EvoMap 保真度**：本设计取其稳定性思想做离散移植，不是数值复刻（没有连续位置优化目标函数）。若将来要可视化"记忆地图"，evomap 工具箱的对齐 t-SNE 可作离线分析工具，与 worker 无耦合。
