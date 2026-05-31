## MemPalace × mem 对照（v4 — 2026-05-31 上游 v3.3.6 增量补审）

> 本篇承接 [`mempalace-diff-v3.md`](./mempalace-diff-v3.md)。v3（2026-05-21）的核对基线是 mempalace HEAD `de7801e`（= 上游 **v3.3.3**），结论是「MCP 表面无新缺口；module/CLI 层 5 处缺口 #29–#33 + KG 层 K1–K8」。
>
> 本篇做两件事：
> 1. **确认 mem 侧 v3 遗留项的收口状态**——自 v3 以来 mem 又落地了若干项，先把账对平。
> 2. **把上游推进到当前最新稳定 tag `v3.3.6`（commit `db1fbe8`，2026-05-24）**，重扫 MCP / CLI / module 三个表面，跟 `de7801e` 做差集，找出 v3 之后上游**新增**的能力，逐项评估值不值得在 mem 落地。
>
> 维护原则同 v1–v3：本篇与代码不一致时**以代码为权威**；落地一项后回到对应表格更新状态（✅ done / 🚧 in progress）。

---

## 0. 方法论

| 步骤 | 来源 / 命令 |
|---|---|
| 拉上游 | `git remote add upstream https://github.com/MemPalace/mempalace.git` + `git fetch upstream --tags`（经代理）。上游已到 `v3.3.6`，另有 `release/v4-prep` 分支 |
| 扫 v3.3.6 表面（不动 fork develop） | `git worktree add --detach /tmp/mp-336 v3.3.6` |
| MCP 工具差集 | `grep -oE 'mempalace_[a-z_0-9]+' mcp_server.py \| sort -u`，对 `de7801e` 与 `v3.3.6` 两版做 `comm` |
| CLI 子命令差集 | `grep '^def cmd_' cli.py`，两版 `comm` |
| 新增 module | `git diff --diff-filter=A --name-only de7801e v3.3.6 -- 'mempalace/*.py'` |
| 变更体量 | `git diff --stat de7801e v3.3.6 -- mempalace/` + CHANGELOG 3.3.4–3.3.6 |
| mem 侧现状 | `git -C mem log`、`ls src/cli/*.rs`、`src/mcp/server.rs`、`src/embedding/*.rs` |

> 上游核对 commit：`db1fbe8`（Merge `release/3.3.6`，2026-05-24）。`v3.3.5` tag 在 2026-05-09，`v3.3.4` 介于其间。本篇 mem 侧核对 commit：`e0e0a5e`（master，2026-05-22）。

---

## 1. 一句话结论

> **上游 3.3.3→3.3.6 的表面变化极小，但新增了一条 mem 没有的能力主线：图层"活的连接"动力学**（Hebbian 增强 + Ebbinghaus 衰减 + 共现派生的 hallway/tunnel）。这是本轮唯一值得认真落地的方向——它恰好**吸收并超越了 v3 一直搁置的 K1（边 confidence 静态列）**：一条会随使用增强、随时间衰减的边权重，是静态 confidence 的超集。
>
> 其余上游新增——office 文档挖掘、gitignore 剪枝、虚拟行号、COCA 过滤、多语言 embedding 默认——要么超出 mem 的 transcript-first 范围、要么撞 §15.4 的"不做 prose 抽取/不烧 LLM"红线、要么 mem 早已用更强方案覆盖（多语言：mem 默认 embedder 疑似 Qwen3 系，本就多语言）。
>
> **mem 侧账目**：v3 的 #29–#33 + K2/K4/K5 已**全部闭环**；唯一历史欠账 K1/K3（边的 confidence / provenance 列）建议**并入本篇 K9 一起做**，因为动力学方案本就要给边加列。

---

## 2. mem 侧 v3 遗留项收口确认（自 2026-05-21 以来）

| v3 项 | 题目 | v3 当时状态 | 现状（mem master `e0e0a5e`） | 证据 |
|---|---|---|---|---|
| #29 | fact_check API + MCP | ✅ | ✅ | v3 已记 |
| #30 | 近似去重 worker | ✅ | ✅ | v3 已记 |
| #31 | `mem init` CLI | ✅ | ✅ | `a235646 feat(cli): add mem init …`；`src/cli/init.rs` 在树 |
| #32 | per-session mine cursor | 🚧 待做 | ✅ **新闭环** | `bdfbca4 feat(mine): per-transcript cursor fast-skip …(v3 #32)` |
| #33 | 外发 embedding 启动警告 | ✅ | ✅ | v3 已记 |
| K2 | topic-tunnel worker | ✅ | ✅ | `2a964ee` |
| K4 | `kg_query_predicate` MCP | ❌ 未做 | ✅ **新闭环** | `15e51d6 feat(graph): K4 kg_query_predicate MCP + K5 …` |
| K5 | fuzzy neighbor suggestions | ❌ 未做 | ✅ **新闭环** | 同上 `15e51d6` |
| **K1** | 边 `confidence` 列 | ❌ 搁置 | ❌ **仍欠** → 见 **K9** | 需 schema 迁移 session |
| **K3** | 边 `extractor`/provenance 列 | ❌ 搁置 | ❌ **仍欠** → 随 K1/K9 一起 | 同上 |

> mem CLI 现 **6** 个子命令：`serve` / `mcp` / `mine` / `wake-up` / `feedback` / `init`（v3 时是 5 个，新增 `init`）。

---

## 3. 上游 3.3.3 → 3.3.6 表面差集

### 3.1 MCP 工具

`mempalace_*` 唯一名计数：**3.3.3 = 31 → 3.3.6 = 32**。

| 变化 | 工具 | mem 对应 / 评估 |
|---|---|---|
| ➕ ADDED | `mempalace_sync` | gitignore 剪枝的 MCP 入口（见 §4 #36），mem 暂不做 |
| ➕ ADDED | `mempalace_warmup_probe__`（尾 `__` = 内部/隐藏） | embedder 冷启动预热探针；mem 嵌入是 async worker，无此 cold-start 痛点 |
| ➖ REMOVED | `mempalace_drawers` | 合并进 `mempalace_list_drawers`，纯整理 |

> 结论同 v2：**MCP 表面层依旧对齐，无新功能缺口**。

### 3.2 CLI 子命令

v3.3.6：`compress hook init instructions mcp migrate mine repair repair_status search split status sweep **sync** wakeup`。

| 变化 | 子命令 | 说明 |
|---|---|---|
| ➕ ADDED | `mempalace sync` | gitignore-aware drawer prune（#1252），见 §4 #36 |

### 3.3 新增 module（`mempalace/*.py`，diff-filter=A）

| module | 行数 | 做什么 | 归类 |
|---|---|---|---|
| **`dynamics.py`** | 256 | **纯函数**：Hebbian 增强 + Ebbinghaus 衰减 + Cepeda 间隔效应，作用于 hall/tunnel dict | → **K9**（建议做） |
| **`hallways.py`** | 330 | wing 内**实体共现边**（co-occurrence ≥ `min_count`，默认 2），JSON 持久化 | → **K10**（建议做） |
| **`format_miner.py`** | 979 | office 文档挖掘（PDF/docx/pptx/xlsx/RTF/EPUB → MarkItDown / striprtf） | → **#34**（暂不做） |
| **`sync.py`** | 321 | gitignore 感知 drawer 剪枝（源文件被 ignore/删除/移动则清） | → **#36**（暂不做） |
| `_stdio.py` | 71 | Windows UTF-8 流重配（cp1252 mojibake 修复） | 🚫 不适用（mem = Rust/Linux） |

---

## 4. 新能力逐项评估（编号续 v3：#34+ / K9+）

### 4.1 图层动力学（本轮重点）

#### K9 ✅建议做 — 边的"活权重"动力学（吸收 K1）

**上游做法**（`dynamics.py`，纯数学，无 I/O）：给每条 hall/tunnel 记录加四个字段，调 `potentiate()` / `apply_decay()` 演化：

```
strength: float        # Hebbian 连接权重，floor=0.05，cap=5.0，DEFAULT=1.0
stability: float       # 抗衰减度；间隔重复时增长（Cepeda），DEFAULT=1.0
last_activated: str    # ISO 时间，potentiate 时更新
access_count: int      # 累计共激活次数
# 常量：POTENTIATION_INCREMENT=0.05  STABILITY_INCREMENT=0.1  SPACED_INTERVAL_HOURS=1.0
```
- **Hebbian**（Hebb 1949「fire together, wire together」）：边被再次共同命中 → `strength += 0.05`（封顶 5.0）
- **Ebbinghaus**（1885 遗忘曲线）：随 `now - last_activated` 指数衰减，`stability` 越高衰得越慢
- **Cepeda**（2006 间隔效应）：**间隔**重复（≥1h）才涨 `stability`，集中刷不涨——防 burst 灌水

**mem 现状**：边（`graph_edges`）只有 `valid_from`/`valid_to`，**无权重、无衰减**。衰减目前只作用在 *memory* 上（`decay_score` + `decay_worker`），**图层没有**。

**为什么吸收 K1/K3**：K1 想加的是**静态** `confidence` 列；一条"会涨会衰"的 `strength` 是它的超集。与其先做静态列再推翻，不如一次把 `strength`/`stability`/`last_activated`/`access_count`（+ K3 的 `extractor`/`source` provenance）一起加进 `graph_edges` schema。

**落地形状**（沿用 mem「先 service 后 worker」+ §15.4 无 LLM）：
1. `graph_edges` schema 加列（Lance `add_columns(AllNulls)` 迁移 + GraphEdge 构造位 + record_batch helper + DuckDB 投影——这正是 K1 估的 ~1 天体量）
2. 新 `src/domain/edge_dynamics.rs` 放纯函数 `potentiate` / `apply_decay`（对标 `dynamics.py`，可直接移植常量）
3. retrieve 的 `graph_boost` 用 `strength`（衰减后）加权，替代当前的二值命中
4. potentiate 触发点：retrieve 命中一条边时记一次 access（与 memory feedback 同构）；衰减在读时惰性算或随 `decay_worker` 扫
- **默认 OFF**（`MEM_EDGE_DYNAMICS_ENABLED=1`），与 dedup/topic-tunnel 同样保守
- **工作量**：L（schema 迁移是大头）。**优先级 P1，但必须单独排一次 spec→implement session**——和 v3 对 K1 的判断一致。

#### K10 ✅建议做 — scope 内实体共现边（hallway 等价）

**上游做法**（`hallways.py::compute_hallways_for_wing`）：扫一个 wing 的所有 drawer，**同一 drawer 内每对不同实体**算一次共现；共现计数 ≥ `min_count`（默认 2）则物化一条 hallway 记录（带 pair + count + 出现的 rooms）。

**mem 现状**：边来自 caller 传入的 `topics` 经 `EntityRegistry` 解析（`extract_graph_edge_drafts`），是**声明式**的；没有"从同一 capsule 里多个实体自动连边"的**共现派生**。K2 做的是 *topic-overlap 跨 project* 的 tunnel，**不是**实体共现。

**落地形状**：新 `src/worker/cooccurrence_worker.rs`，按 `(project, repo)` 分组扫活跃 capsule 的 `topics`/entity，组内 pairwise 共现计数 ≥ 阈值 → `add_edge_direct` 写 `cooccur:entity:<a>↔<b>` 边（幂等）。复用 K2 的 sweep 骨架。默认 OFF。
- **工作量**：M（对标 K2 ~3h）。**优先级 P2**——K2 的天然兄弟，可在 K9 schema 落地后顺手做（共现边正好挂 `strength`）。

#### K11 🟡谨慎 — 共现 → 跨 scope tunnel 提升

**上游做法**（3.3.6 CHANGELOG #1565）：同一实体在多个 wing 的 hallway 里出现 → 自动提升成跨 wing tunnel。

**评估**：是 K10 的上层。**等 K10 落地、观察共现边质量后再做**，否则容易制造噪声跨边。**优先级 P3**，依赖 K10。

#### K12 ✅建议做（小）— 写时拒绝倒置 valid 区间

**上游做法**（3.3.5 #1214）：`add_triple()` 在写时拒绝 `valid_to < valid_from`——否则 `query(as_of)` 的 `valid_from<=as_of AND valid_to>=as_of` 永不命中，行**存了但永久不可见**，是 P0 数据完整性脚枪。

**mem 现状待查**：`add_edge_direct` / `invalidate_edge` 是否校验 `valid_to >= valid_from`？若无，补一个写时校验 + 单测。开区间（单边）与点事实（`valid_from==valid_to`）放行。
- **工作量**：S（~30min）。**优先级 P1（防御性，便宜）**，可独立于 schema session 先做。

### 4.2 摄入与其它（基本不动 mem）

| # | 上游能力 | 评估 | 结论 |
|---|---|---|---|
| **#34** | office 文档挖掘（`format_miner.py`，PDF/docx/…→MarkItDown） | mem 是 transcript-first；Rust 侧无 MarkItDown 等价，要么 FFI 要么 shell-out Python，转换栈很重；且与"挖 Claude 对话"的核心场景正交 | 🚫 **暂不做**——等出现明确的"挖本地文档库"需求再评估 |
| **#35** | 多语言 embedding 默认（`embeddinggemma-300m` ONNX，替换英文单语 all-MiniLM） | mem 默认 `embed_anything` 从 HF 载模型，源码注释出现 `Qwen3-1024`——**疑似默认 Qwen3-Embedding 系（1024 维，多语言强、中文 OK）**。若属实，mem 在多语言上**本就不输、甚至强于 3.3.6 前的 mempalace** | 🟡 **先确认 `EMBEDDING_MODEL` 默认值**。若默认确为多语言模型→无缺口；若是英文单语→才升 P1 |
| **#36** | `mempalace sync` = gitignore 感知 drawer 剪枝 | mem 挖的是 transcript 不是 repo 文件，gitignore 语义弱关联；可迁移的概念是"源 transcript 已删→剪对应 capsule/archive"，但当前无痛点 | 🚫 **暂不做**——低价值 |
| — | 虚拟行号 + 外科手术式 closet 指针（按行/日期引用） | mem 已存全量 verbatim transcript archive；行级引用是 niche | 🚫 暂不做 |
| — | COCA 高频英文词过滤（防 "system"/"user" 被当实体） | 只对 **prose 自动抽实体**有意义；mem 实体来自 caller `topics`，**不做 prose 抽取**（§15.4 / v1 #20） | 🚫 不做（红线外） |

---

## 5. 推荐路线表（本篇新增）

| # | 题目 | 改动面 | 工作量 | 优先级 | 依赖 |
|---|---|---|---|---|---|
| **K12** | 边写时拒绝倒置 `valid_to<valid_from` | `add_edge_direct`/`invalidate_edge` + 校验 + 单测 | S（~30min） | **P1**（便宜防御） | 无 |
| **K9** | 边「活权重」动力学（strength/stability/decay）+ 吸收 K1/K3 列 | `graph_edges` schema 迁移 + `domain/edge_dynamics.rs` + retrieve graph_boost + worker | L（~1 天） | **P1**（需专门 spec session） | schema 迁移 |
| **K10** | scope 内实体共现边（hallway 等价） | `worker/cooccurrence_worker.rs`（仿 K2） | M（~3h） | **P2** | K9 列就位最佳 |
| **K11** | 共现→跨 scope tunnel 提升 | 在 K10 sweep 上加提升逻辑 | S | **P3** | K10 |
| **#35** | 确认/升级多语言 embedding 默认 | 先 confirm `EMBEDDING_MODEL`；必要时换默认模型 | S（确认）/ M（换模型） | 待定 | — |

### 决策点
- **可立刻做**：**K12**（独立、便宜、防数据脚枪）。
- **✅ 已落地（2026-05-31）**：**K1 + K3 列**已独立实现——`graph_edges` 现有 `confidence Float32` + `extractor Utf8`（均 nullable），含 mem 首个 on-disk `add_columns(AllNulls)` 迁移、HTTP caller 声明、4 个 TDD 测试。详见 [`mempalace-diff-v3.md`](./mempalace-diff-v3.md) §7.2 K1/K3 行。
- **下一次 schema session**：**K9** 现在**不再需要从零加列**——只在已落地的 `confidence` 列上叠加 `strength` / `stability` / `last_activated` / `access_count` 四个动力学字段 + Hebbian/Ebbinghaus 演化 + retrieve `graph_boost` 加权（K1 推迟的那部分）；落地后顺带 **K10**。
- **先确认再说**：**#35**——查 mem 默认 embedder 是不是已经多语言（大概率是 Qwen3，则此项作废）。
- **不做**：#34 office 挖掘 / #36 gitignore 剪枝 / 虚拟行号 / COCA 过滤（理由见 §4）。

---

## 6. 为什么 3.3.5 / 3.3.6 的一大批 bugfix 不适用 mem

上游这两版的 bug 修复**绝大多数是后端/平台专属**，与 mem 的 Lance + DuckDB + Linux 栈无关——和 v3 §3 对 `repair.py` 的判断同理：

| 上游 bugfix 类别 | 例（PR） | 为何 mem 无此 failure mode |
|---|---|---|
| ChromaDB HNSW flush/segment 损坏 | tool_search 重试(#1396)、from-sqlite 重建(#1308)、segment quarantine(#1452) | mem 用 Lance native ANN，无 usearch/HNSW sidecar（v3 已论证） |
| SQLite `MAX_VARIABLE_NUMBER` / 文件锁 | compress 分页(#1073)、close 释放锁(#1067) | mem KG 在 DuckDB/Lance，无 chroma-sqlite 变量上限/锁语义 |
| Windows 专属 | `_stdio.py` cp1252(#1282)、bash 3.2 `mapfile`(#1441)、cp1252 锁文件(#1438) | mem 单二进制，无 Python/bash hook 层 |
| 连字符 wing 名截断 | create_tunnel(#1529)、save-hook(#1424) | mem 不按字符串切 scope，用 entity_id |
| **KG 倒置区间静默不可见** | add_triple 校验(#1214) | ⚠️ **唯一可借鉴** → 已列 **K12** |

> 即：mem 在 v3.3.5/3.3.6 这两版上"落后"的几乎全是它结构上不会遇到的 bug；唯一有价值的防御性教训已收编为 K12。

---

## 7. 时间戳与维护

- 本篇生成时间：**2026-05-31**
- 上一次比较：v3，2026-05-21（基线 `de7801e` = v3.3.3）
- 本篇上游基线：**`db1fbe8` = v3.3.6**（2026-05-24）；mem 基线：`e0e0a5e`（master，2026-05-22）
- 上游远端：已加 `upstream = https://github.com/MemPalace/mempalace.git`（本地 fork `origin = BenLocal/mempalace`，develop 停在 `de7801e`=v3.3.3，工作区未动）
- **v4-prep 预警**：上游有 `release/v4-prep` 分支——mempalace **v4 在酝酿**，可能带架构级变化。下次（上游发 v4 时）应重跑 §0 方法论，必要时起 `mempalace-diff-v5.md`。
- 维护建议：
  1. 完成 K9 / K10 / K11 / K12 / #35 任一项后，回 §5 表标 ✅ + commit hash（格式 `… (closes mempalace-diff-v4 K9)`）
  2. ✅ K1/K3 已于 2026-05-31 **独立落地**（非经 K9）——v3 §7.2 已标 ✅。K9 现只需在已有 `confidence` 列上叠加 strength/stability/decay 动力学 + retrieve 加权
  3. 上游发 v4 正式版时，重扫并起 v5
  4. 临时 worktree `/tmp/mp-336` 用完即 `git worktree remove`；`upstream` 远端保留供下次 fetch
