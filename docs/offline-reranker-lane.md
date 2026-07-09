# 离线 Reranker Lane 设计提案（I2 落地形态）

> 立项日期 2026-07-09。承接 oss-memory-diff §10 I2 spike 裁决（commit `6eeee85`）：
> Qwen3-Reranker-0.6B 判别力极佳（相关 0.9999/0.9983 vs 无关 0.0001、难负例 0.0106），
> 但 CPU 单对 ~700ms 带宽瓶颈、candle q8 量化 prefill 反慢 12×、批量前向无 mask 入口
> ——交互精排死刑（决策线 1s），改判**离线 worker 侧**。本文是落地设计。

## 0. 一句话

把 cross-encoder 从「查询路径上的精排段」改判为「worker 侧的**关系质量算子**」：
用它复核/铸造胶囊间关系（边、supersede 提案、merge 候选），让图通道（I1 PPR）、
评审队列、evolution 聚类拿到比裸 cosine 高一个档次的证据——查询路径零延迟。

## 1. 设计原则（全部继承既有纪律）

- **不碰查询路径**：worker tick 内运行，700ms/对无压力；`golden_recall` 头条口径不动。
- **opt-in 默认关**：`MEM_RERANK_OFFLINE_ENABLED=1` 总开关，姿势同 O2/O7/H1。
- **review-gated**：一切「提案类」产物仍走 PendingConfirmation；边操作走既有
  bitemporal 原语（close + re-add，保历史），📦 存储 verbatim 永不改写。
- **零生成-LLM 默认不破**：reranker 是判别用途的打分模型（读 yes/no logits），
  与 Qwen3-Embedding 同性质；零新增 crate（candle 0.9.2 已在依赖树）。
- **模型不常驻**：按需加载（实测 1.5s）→ 本 tick 批处理 → drop。f32 常驻 +2.4GB RSS
  不可接受（线上基线 4.3GB）；tick 慢周期下加载成本可忽略。

## 2. 消费场景与生产现状（诚实盘点）

| 子 lane | 消费者 | 生产现状（2026-07-09 本实例） | 即时价值 |
|---|---|---|---|
| (c) evolution merge/generalize 候选复核 | evolution worker（`MEM_EVOLUTION_ENABLED=1`，K=5 日周期，**开着**） | merge 用纯 cosine 0.88 聚类，无语义复核 | ★ 唯一活消费者：低分否决防误合并 |
| (a) 语义边铸造 + 复核（喂 I1 PPR） | 图通道 `expand_graph` | `related_to` 边 **0 条**（H1 `MEM_INGEST_LINK_ENABLED` 未开；top_relations 加总=总边数可证） | ★★ 长期最大，但依赖「开边」决策 |
| (b) supersede 提案双向打分注记 | 评审队列（人工/agent review） | near-dup lane（`MEM_INGEST_NEARDUP_ENABLED`）未开；但 evolution 的 generalize/merge 提案在进队列 | ★ 评审信噪比（41 条积压是真实痛点） |

关键判断：**(a) 不必等 H1**——离线 lane 可以「铸造+复核一体」：
对活跃胶囊做 ANN 近邻候选（cosine ∈ [0.60, neardup 阈值)，比 H1 的 0.80 下探——
reranker 兜底精度，允许更低的召回下界），双向打分几何均值 ≥ 阈值才铸边，
`extractor="reranker_link"`、`confidence=P(yes)几何均值`。比「先铸弱边再复核」少一半写放大，
且存量 461 个活跃胶囊一次 backfill 就能给 PPR 喂上第一批高置信语义边。

## 3. 架构

### 3.1 `src/rerank/` 模块（Phase 1）

```
pub trait RerankProvider: Send + Sync {
    fn model(&self) -> &str;
    /// (query, document) 对 → P(yes) ∈ [0,1]，顺序对应输入
    fn score_pairs(&self, pairs: &[(String, String)]) -> Result<Vec<f32>, RerankError>;
}
```

- `candle_qwen3.rs`：spike 配方原样平移（mmap f32 safetensors、官方对话模板、
  末位 logits yes/no softmax、`clear_kv_cache` 隔离样本、串行循环——不做批量）。
- `fake.rs`：测试用（内容哈希决定分数，确定性）。
- 配置：`MEM_RERANK_MODEL_DIR`（显式本地目录，默认
  `~/.cache/huggingface/manual/Qwen3-Reranker-0.6B`；**不做运行时 HF 自动下载**——
  本环境 HF 直连不通，显式路径 + 启动日志提示缺权重即可）。
- 胶囊对的打分输入：`content` 截断到 ~1500 chars（模板+两侧 ≈ 800-1000 token，
  单对延迟随长度线性涨，需实测校准；截断只影响打分输入，不碰存储）。

### 3.2 `reranker_worker`（Phase 2）

独立 worker（姿势同 vacuum/evolution）：`MEM_RERANK_OFFLINE_ENABLED=1` 才 spawn，
`MEM_RERANK_INTERVAL_SECS`（默认 21600 = 6h），每 tick：

1. 收集本 tick 工作集（按子 lane 各自的谓词，见 §4），上限
   `MEM_RERANK_MAX_PAIRS_PER_TICK`（默认 200 对 ≈ 2.3 分钟 CPU）；
2. 工作集非空才加载模型；处理完 drop；
3. 每对写回后立即生效（无批间事务依赖，中断安全：下 tick 谓词自然跳过已处理项）。

### 3.3 写回原语

- 边铸造：`add_edge_direct`（已有）。
- 边失效：`invalidate_edge`（已有）。
- 「复核后更新 confidence」= close 旧边 + add 新边（bitemporal 干净，**零新 trait 方法**
  ——吸取 H2 教训：新增 CapsuleStore/GraphStore 方法 = 4 个实现要跟）。
- 提案注记：追加 evidence 行（供评审 UI/agent 阅读），复用 refine 占位的证据清单模式。

### 3.4 PPR 侧的边类档位（绑 Phase 3）

`reranker_link` 是第三类边：模型判别的语义关联，证据强度介于「相似铸造」与「事实边」之间。
I1 `edge_base_boost` / PPR 转移权重加一档：`RERANK_EDGE_BOOST = 8 × conf`
（sim=4×cos、fact=12 之间；ceiling 同理）。具体数值 Phase 3 用真实图人工抽查校准，
不预设为真理。

## 4. 阶段计划

| 阶段 | 内容 | 工作量 | 验收 |
|---|---|---|---|
| **P1** ✅ | `src/rerank/` trait + candle/fake provider + 单测（模板/打分/截断） | S-M | 单测全绿；spike 四对照在 candle provider 上复现分数 |
| **P2** ✅ | **(c) evolution merge 候选复核**（merge 执行前置闸：双向几何均值 < `MEM_RERANK_MERGE_FLOOR`(默认 0.5) 的候选否决）+ `/metrics` 计数 | M | fake provider 集成测试：低分候选被否决、高分放行；`rerank_*` 计数出现 |
| **P3** | **(a) 语义边铸造 backfill + 增量**（ANN 候选 → 双向打分 → `reranker_link` 边；PPR 加档位） | M | 真实实例 dogfood：铸边量/质量人工抽查；`expand_graph` 请求可见图变化 |
| **P4** | **(b) 提案打分注记**（suspected_supersede / generalize 占位追加双向分 + 非对称性提示） | S | 评审队列 UI/agent 可见注记 |

P1+P2 一批提交（P2 是 P1 的第一个真实消费者）；P3、P4 各自独立提交。
全程每阶段 `cargo fmt + clippy -D warnings` + 全量测试门。

**P1+P2 落地偏离（2026-07-09，以代码为权威）**：
1. 被否决的候选置 **`cancelled`** 而非本文初稿的「打回 pending」——pending 会让同一候选每个
   K 周期重复加载模型重复否决；`cancelled` 走既有的再提案抑制（op_kind+Jaccard），簇成员
   变化后的新候选不受抑制，语义正确且零浪费。
2. rerank 出错（模型缺失等）的姿势是 **fail-closed-HOLD**：不执行也不 cancel，候选保持
   pending 下个 sweep 重试并告警——operator 显式开了闸，绕过坏闸执行等于闸不存在；cancel
   则会封杀一个从未被模型打过分的候选。
3. P2 **没有独立 `reranker_worker`**——merge 闸直接内联在 evolution sweep 的执行分支里
   （`spawn_blocking` 包裹模型加载+打分，不占 executor），每日 K 周期 ~10 对量级无需独立
   tick。独立 worker 推迟到 P3（边 backfill 才真正需要自己的节奏）。

## 5. 预算

- 存量 backfill（P3）：461 活跃胶囊 × top-4 候选 ≈ ~1800 对 × 双向 ≈ **~42 分钟 CPU**，
  按 200 对/tick 分摊 ≈ 18 个 tick（6h 周期下 ~4.5 天自然完成；可调 interval 加速）。
- 增量稳态：每 tick 通常 < 20 对（新胶囊 × 候选数），秒级。
- evolution 复核（P2）：merge 候选每日 K 周期一批，量级 ~10 对/天，可忽略。

## 6. 观测

`/metrics` 新增（姿势同 O6d，choke point 计数）：
`rerank_pairs_total` / `rerank_edges_minted` / `rerank_edges_invalidated` /
`rerank_merges_vetoed` / `rerank_proposals_annotated`。

## 7. 风险与开放问题

1. **长文本延迟**：spike 只测了 ~120 token 对；1500-char 截断下单对可能到 2-4s。
   P1 单测补长度-延迟曲线，必要时降截断。
2. **reranker_link 边的档位数值**（§3.4）：8×conf 是拍的，P3 校准；PPR 在真实图上的
   收益本身仍属假设（LoCoMo 测不了，见 I1 负结果），dogfood 是唯一验收面。
3. **merge 否决的假阴性**：reranker 对「同一事实的两种写法」应给高分，但对
   「同主题不同事实」给低分正是我们要的；若实测否决过多，floor 可调（env）。
4. **权重分发**：模型 1.19GB 不进仓、不进 CI；CI 全走 fake provider。
   部署机按 runbook 经代理断点续传预热（已缓存）。

## 8. 不做

- 交互查询路径精排（spike 判死，见 §10 I2）；GPU 姿势等有 GPU 环境再议。
- ort/ONNX 路线（原生依赖 + cross musl 风险）。
- q8 量化（candle prefill 反慢 12×，实测记录在案）。
