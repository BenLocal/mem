# ④ OSS 记忆引擎对照 — mem0 / agentmemory (v1 — 2026-06-05)

> 这是 mem 的**第四条对照线**，与 ① 平行但参照系不同：
> - **①** MemPalace 对齐 —— [`mempalace-diff.md`](./mempalace-diff.md) (v1–v4) + 执行面板 [`ROADMAP.MD`](./ROADMAP.MD)（#1–#36 / K1–K12）
> - **②** Backend 存储抽象 —— [`backend-coupling.md`](./backend-coupling.md)
> - **③** 长内容召回 —— [`long-content-recall.md`](./long-content-recall.md)
> - **④** OSS 记忆引擎对照（本篇）—— 参照 [mem0](https://github.com/mem0ai/mem0) 与 [agentmemory](https://github.com/rohitg00/agentmemory)，沿 **write → recall → feedback** 主轴找 mem 可借鉴的更优处理；产出 **O1–O5**
>
> **维护原则**同 ①/②/③：本篇与代码不一致时**以代码为权威**；落地一个 O 项后回 §5 表格更新状态（✅ done / 🚧 in progress）+ commit hash（格式 `… (closes oss-memory-diff O#)`）。新增 O 行要同步回 [`ROADMAP.MD`](./ROADMAP.MD) 的「OSS 对照路线（O 系列）」表。

---

## 0. 调研基线

| 项 | 值 |
|---|---|
| 日期 | 2026-06-05 |
| mem 基线 commit | `5ea1ab2` |
| mem0 参照 | `mem0ai/mem0` @ main（README + `mem0/memory/main.py` 直读） |
| agentmemory 参照 | `rohitg00/agentmemory` @ main（README + 仓库结构；TypeScript + iii-engine Rust 二进制） |
| 方法 | 外部仓 fetch 源码 / README，mem 侧用 codegraph 实读 `pipeline/retrieve.rs`、`pipeline/ranking.rs`、`worker/decay_worker.rs` |
| 注意 | agentmemory 的 `95.2% recall@5 on LongMemEval-S` 等是厂商自报数据，需打折看；mem0「2026-04 additive 算法」结论来自当前 main 源码而非旧论文 |

---

## 1. 一句话结论

> mem 在**显式 feedback 粒度**（5 种 taxonomy）和**时间建模**（bitemporal graph + 既有 `decay_worker` 线性时间衰减）上**比两家都先进**；融合层也已经是 RRF（`RRF_K=60`，见 `ranking.rs`），与两家同档。
>
> 唯一的**结构性弱点**是：mem 的生命周期信号**完全依赖 agent 显式回调**，而 `retrieve` 是纯读、**不回写任何使用痕迹**——没有 `last_used_at` / `access_count`，`decay_worker` 又以 `updated_at`（最后一次**写**）为锚，于是"天天被召回的记忆"和"再没人看的记忆"**以完全相同的速度衰减**。mem 自己的历史反复踩这个非对称（auto-recall 里的 `30d19dc`/`4d538a6`/`d3e8d80`：「沉默会冻结 ranking」）。
>
> mem0 靠"干脆不衰减、只增不删"绕开；agentmemory 靠 **access-count 强化 + Ebbinghaus 遗忘曲线**的隐式闭环解掉。**最高性价比的借鉴 = O1：给 retrieve 加一层隐式使用强化，并让它重置衰减时钟。**

---

## 2. 两个项目的处理流程

### 2.1 mem0（当前 main，2026-04 "additive" 算法）

**本次最大发现**：世间印象里 mem0 那套「LLM 判定 ADD/UPDATE/DELETE/NOOP 的两阶段对账」，**在现行 main 已被废弃**，换成只增不改。

```
WRITE  memory.add(messages, user_id)
  Phase1: semantic search 取已有相似记忆 (existing_memories)
  Phase2: 只调一次 LLM（ADDITIVE_EXTRACTION_PROMPT）
          输入 = existing_memories + new_messages + last_k_messages + custom_instructions
          输出 = fact 扁平列表
  → hash 去重 → 批量 insert
  ★ "Memories accumulate; nothing is overwritten" —— 无 UPDATE/DELETE 对账环节
  ★ entity 抽取 + embed + 跨记忆 link
  （_update_memory / _delete_memory 函数仍在，但 inference 路径不再驱动它们）

RECALL  memory.search(query, filters, top_k)
  semantic 超取 (internal_limit = max(limit*4, 60))
  ‖ BM25 keyword（vector store 支持 keyword_search 时）
  ‖ entity boost：query 命中 entity 所链接的记忆最多 +0.5
     spread 衰减  boost = sim * W / (1 + 0.001*(num_linked-1)^2)   ← 防热门 entity 过度 boost
  → score_and_rank() 融合 semantic + 归一化 BM25 + entity → 阈值过滤 → top-k
  + temporal reasoning（把正确"日期"的实例排上来）

FEEDBACK  ── 无 ──
  无 access-count / 无 decay / 无显式 feedback。纯堆积。
```

存储：vector store（Qdrant 等）+ entity/graph store + history DB（sqlite，记 ADD/UPDATE/DELETE 日志）。

### 2.2 agentmemory（rohitg00，TypeScript + iii-engine Rust 二进制）

骨架是模拟人类睡眠记忆固化的**四层 consolidation**。

```
WRITE  PostToolUse hook 自动捕获
  mem::observe（生）→ SHA-256 去重（5 分钟窗口）→ privacy filter（剥 API key/secret/<private>）
  → 存原始 observation → mem::compress（LLM 压成结构化 fact）→ embedding
  → 进 BM25 + vector 索引
  四层: Working(生) → Episodic(会话摘要) → Semantic(fact/pattern) → Procedural(workflow)

RECALL  memory_smart_search
  BM25 ‖ Vector ‖ Graph → RRF 融合 (k=60)
  + session diversification（单会话最多 3 条）

FEEDBACK  ── 隐式 / 自动 ──
  Ebbinghaus 遗忘曲线做时间衰减；常被访问的记忆被强化（access-count 驱动 retention）
  TTL 失效 / 矛盾检测与消解 / 按 importance 自动 eviction
  SessionEnd hook 摘要整段会话 → consolidation
```

存储：SQLite + in-memory vector（默认），可选 Postgres+pgvector。

---

## 3. write → recall → feedback 三段对照

| 阶段 | mem0 (current) | agentmemory | **mem（本项目，code 实读）** |
|---|---|---|---|
| **write: fact 抽取** | LLM 一次 additive（带已有记忆） | LLM compress（observe→compress） | **不抽取**（verbatim；agent 供 fact／`mem mine` 离线抽取）→ 见 §6 不做 |
| **write: 去重** | hash + 抽取时考虑已有 | SHA-256(5min) + **矛盾检测** | `content_hash`（**仅完全一致**，`pipeline/ingest.rs`）→ **O2** |
| **write: 脱敏** | — | **privacy filter** | 无（verbatim）→ **O5** |
| **write: 生命周期** | 只增不改 | TTL / eviction | **状态机 + supersede 链 + auto-promote**（mem 优） |
| **recall: 融合** | weighted（sem+BM25+entity） | RRF k=60（sem+BM25+graph） | **RRF k=60（lex+sem）+ 加法栈**（`score_with_hybrid`，scope/intent/confidence/freshness−decay/graph）—— 同档 |
| **recall: 多样化** | — | **单会话最多 3 条** | transcript 侧有 session **co-occurrence**（`transcript_recall.rs`）／**capsule 侧无任何多样化** → **O3** |
| **recall: 时间性** | temporal reasoning | recency | **freshness（`ranking.rs`，紧致 [-14,6] tiebreaker）+ bitemporal graph（valid_from/to, as_of）**—— mem 优 |
| **recall: graph 衰减** | **按 degree spread 衰减** | graph traversal | 有 `graph_boost_by_id`，**未按 degree 衰减** → **O4** |
| **feedback: 显式** | **无** | 无 | **5 种 taxonomy**（useful/applies_here/outdated/does_not_apply/incorrect）→ confidence/decay/status（mem 独有，最强） |
| **feedback: 时间衰减** | 无 | Ebbinghaus（指数 + 访问重置） | **有**：`decay_worker` 线性 `0.01/day`，仅 Active 行，**锚 `updated_at`，1.0 饱和**——但**无访问重置** → **O1** |
| **feedback: 使用强化** | 无（entity-link 间接 boost） | **access-count 强化** | **无**：`retrieve` 纯读，无 `last_used_at`/`access_count`，召回不回写 → **O1** |

---

## 4. mem 已经领先的地方（别动坏了）

1. **显式 feedback 粒度** —— 5 种 taxonomy 两家都没有；mem0 连 feedback 机制都没有。这是 mem 的护城河，O1 是**在它下面垫隐式层**，不是替换它。
2. **bitemporal graph** —— `valid_from/valid_to` + `as_of` 时点查询比两家都强。
3. **既有时间衰减** —— `decay_worker` 已上线（ROADMAP #7）。两家里只有 agentmemory 有衰减，mem0 没有。
4. **RRF 已落地** —— `ranking.rs::rrf_contribution`（rank-1≈16/通道，双通道≈33）与两家融合层同档；不需要"引入 RRF"。
5. **verbatim 原则 + supersede 链 + 状态生命周期** —— 与 mem0「扔掉对账、纯堆积」相反，mem 保留版本管理，保真度占优。

---

## 5. 可借鉴的更优处理 → O1–O5

> 层标记同 mempalace：📦 存储/输出纪律｜🔍 索引/排序/生命周期｜⚙️ 基础设施。

### O1 🔍 — 检索使用强化 + 衰减时钟重置 ★最高性价比（P0）✅ done（`808cb59` + `709c648`，2026-06-05）

**现状（code）**：`retrieve` 纯读；无 `last_used_at` / `access_count` 列；`decay_worker::apply_time_decay` 以 `updated_at`（最后一次**写**）为锚做 `0.01/day` 线性衰减。后果：被反复召回并真正用到的 capsule，与无人问津的，衰减速度完全相同；生命周期只能靠 agent 显式 `memory_feedback` 推动，而那条链路在 mem 历史上反复"静默失效"。

**借鉴**：agentmemory 的 access-count 强化 + Ebbinghaus"访问重置遗忘曲线"。

**改法（三小步，向后兼容）**：
- (a) 加列 `last_used_at`（nullable）+ 可选 `use_count`。capsule schema 加列即可，旧行 NULL。
- (b) **触发点选"真正被用"而非"被检索到"**：在 `pipeline/compress.rs` 真正把某条 capsule 写进压缩输出（directives/relevant_facts/...）的那一刻，异步 bump `last_used_at = now`（`use_count += 1`）。只 retrieve 到但没进输出**不**计——逼近 agentmemory 的"used"语义，避免把噪声也强化。
- (c) `decay_worker` 的衰减锚从 `updated_at` 改成 `COALESCE(last_used_at, updated_at)`——使用即"重置遗忘曲线"。可选：retrieve 加一个**很小**的 `recently_used` 加性 bonus（量级对齐 freshness 的 [-14,6]，仅作 tiebreaker，不喧宾夺主）。

**为什么合成一项**：(a)(b)(c) 共用同一根 `last_used_at` 列；拆开做会两次改 schema。

**触点**：capsule schema（加列）、`pipeline/compress.rs`（输出即 bump 的 hook）、`worker/decay_worker.rs`（衰减锚）、`pipeline/retrieve.rs`（可选 bonus）。
**风险**：低-中。bump 必须**异步、读路径外**，不能阻塞召回；写放大用"每会话每 capsule 至多 bump 一次"收口（与显式 feedback 的"每会话至多一条"同纪律）。

**已落地（`808cb59` 加列 + `709c648` 行为，以代码为权威）**：实际形态与上面 (b)(c) 有两处偏离，都是 lance DuckDB 扩展的 UPDATE 限制逼出来的：
- (b) bump 触发点**没放进 `compress.rs`**（那是个纯函数，无 store/异步）。改成：`search` 在 `compress()` 返回后收集**真正写进输出**的 capsule_id（directives+facts+patterns，见 `SearchCapabilityCapsuleResponse::emitted_capsule_ids`），经 unbounded channel 异步丢给新的 **always-on `last_used_worker`**（仿 `potentiation_worker`）；worker 攒批去重后 `Store::bump_last_used_at` 盖 `last_used_at`，每 tenant 每 tick 一次批量 flush。"used"=进输出的语义不变，且彻底不在读路径上写。
- (c) decay 锚**不能直接写 `COALESCE(last_used_at, updated_at)`**：lance UPDATE 的 SET 表达式不支持 `COALESCE`（"Not implemented"），且 WHERE 不支持多值 `IN`（"pushed table filters"）。改成：decay sweep 拆**两条 WHERE 互斥的 UPDATE**（`last_used_at IS NOT NULL` 锚 last_used_at、`IS NULL` 锚 updated_at，每行只命中一次）；bump 逐 id 单等值 UPDATE。额外精化：sweep 的**每 tick 重置列从 `updated_at` 改成 `last_used_at`**，于是 `updated_at` 回归纯写时间 freshness（旧账：原本被 sweep 每小时刷平），`decay_score` 仍是累加器、feedback delta 不被抹。可选的 retrieve freshness bonus **未做**（标 optional，留待后续）。
- 旧表迁移：`migrate_capability_capsules_add_columns`（`add_columns(AllNulls)`）回填旧行 NULL → sweep 回落 `updated_at`，由 `legacy_uint64_version_compat` 端到端验证。新增 env `MEM_LAST_USED_FLUSH_SECS`（默认 5）。

### O2 🔍 — write 时近邻去重 / 矛盾检测（P1）✅ done（`b7b9528` refactor + worker，2026-06-05）

**现状（code）**：`pipeline/ingest.rs` 去重只有 `content_hash` 完全一致；同义改写直接落新行，矛盾并存，要等 agent 手动 `supersede`。

**借鉴**：mem0 抽取时把 `existing_memories` 喂给 LLM 避免重复；agentmemory 的矛盾检测。

**改法**：ingest 在 `content_hash` 检查之后，对新 capsule 做一次 semantic top-k 近邻查询；cosine 超阈值则**不自动覆盖**（守 verbatim），而是落 `PendingConfirmation` 并标注"疑似 supersede `<id>`"，进 `list_pending_review` 由人/agent 定夺。

**触点**：`pipeline/ingest.rs`（hash 检查后加近邻照会）、复用既有 semantic search + review 队列。
**风险**：低-中。阈值要保守（宁可漏判不可误并）；近邻查询走异步，别拖 ingest 延迟。

**已落地（以代码为权威）**：检查放在**异步 embedding worker** 里（用户拍板）——`post_embed` 完成 job 后，对刚嵌入的 `Active` capsule 用 `hybrid_candidates(空 text + 新向量)` 取 top-k、排除自身、逐候选取存量向量算精确 cosine；最高者 ≥ 阈值则 `set_capsule_status(.., PendingConfirmation)` + 写一条 `suspected_supersede` 图边。两处偏离原 (改法)：① **标注从 tag 改成图边**——lance 的 `.update()` 改不了 List 列（tags），且检查在 capsule 已落库后跑、只能事后写；图边是 mem 里 memory→memory 指针的惯用且安全表达（与 `contradicts`/`supersedes` 同族）。② 触发点**没放进 `compress.rs`/ingest 同步路**（那要每次 ingest 多一次同步嵌入）；放异步 worker 后零 ingest 延迟、且复用 worker 刚算出的向量。前置：把 `accept_pending`/`reject_pending` 合并成 `set_capsule_status` 单一原语（`b7b9528`），O2 的翻转直接复用、无需新 trait 方法。opt-in `MEM_INGEST_NEARDUP_ENABLED`，阈值 `MEM_INGEST_NEARDUP_THRESHOLD`（默认 0.92）。

### O3 🔍 — capsule recall 多样化（MMR / per-source cap）（P1）

**现状（code）**：transcript 侧有 session **co-occurrence**（`transcript_recall.rs::score_candidates` 的 `session_counts`，那是**加权**，不是去重）；**capsule 侧没有任何多样化**。同一 supersede 链派生、或同 session 批量 ingest 的近似 capsule 可能霸占头部。

**借鉴**：agentmemory 的 `max 3 per session`。

**改法**：在 `pipeline/retrieve.rs::finalize` 排序后、截断前，加一个轻量 per-source（session / supersede-chain root / repo:module）配额裁剪或 MMR 式去冗。注意与 transcript 的 co-occurrence 是**两个相反方向**的东西，别混用同一函数。

**触点**：`pipeline/retrieve.rs::finalize`。
**风险**：低。配额是软上限，命中不足时回填。

### O4 🔍 — graph boost 按 degree 衰减（P2）

**现状（code）**：`score_with_hybrid` 吃 `graph_boost_by_id`，但 boost **不看 entity 的度数**。挂在热门 entity（如 `project:` / `repo:` 节点）下的一大批 capsule 会被一刀切 boost，把真正相关的压下去。

**借鉴**：mem0 的 spread 衰减 `boost / (1 + 0.001*(num_linked-1)^2)`。

**改法**：graph boost 按锚 entity 的邻居数（度）做反比衰减；高扇出节点贡献的 boost 自动摊薄。

**触点**：graph boost 计算处（`graph_anchor_nodes` 下游、`score_with_hybrid`）。
**风险**：极小（基本是一个除式）。

### O5 📦/⚙️ — ingest secret 脱敏（P2）

**现状（code）**：mem 把 transcript 整段 verbatim 存（`conversation_messages`）。相比两家，**verbatim 反而抬高了密钥泄露面**。

**借鉴**：agentmemory 存前 privacy filter（剥 API key / secret / `<private>`）。

**改法（两层，守 verbatim）**：原文在 access-control 下原样保留；只在**索引 / 输出层** redact 高置信密钥模式（`sk-...`、AWS AKIA、私钥块、`<private>…</private>`）。不在存储层就地改写 content（那会破 verbatim 原则）。

**触点**：index/输出侧（`pipeline/compress.rs` 输出过滤 + 嵌入前文本），不动 `memories.content` / transcript `content` 落盘。
**风险**：中。要先定"哪些算密钥"白名单，避免误 redact 正常内容；与 §6「verbatim 不破」边界强相关，落地前需单独定调。

---

## 6. 不做 / 本质形态差异

- **inline LLM fact 抽取（write 时）** —— mem0/agentmemory 都在写时用 LLM 把对话压成结构化 fact；mem **故意不做**，把抽取放到 `mem mine` 离线管线，落盘 verbatim。这是哲学差异（保真 > 即时结构化），不是 gap，**不引入**。
- **只增不删（mem0 additive）** —— mem 用 supersede 链 + 状态机表达版本，比 mem0 纯堆积更可控，**不回退**。
- **四层 consolidation（agentmemory）** —— mem 已有近似分层：transcript archive(生) ≈ Working、capsule ≈ Semantic、`workflow.rs` episode→workflow ≈ Procedural。Episodic（会话级摘要）是唯一缺口，但价值待测，**暂不立项**。

---

## 7. 落地顺序

| 优先 | 项 | 层 | 工作量 | 价值 |
|---|---|---|---|---|
| **P0 ✅** | O1 使用强化 + 衰减重置（`808cb59`+`709c648`） | 🔍 | M（加列 + last_used worker + decay 锚） | 直击 mem 最大结构性弱点：让 ranking 在 agent 不回调时也能自主生长 |
| P1 ✅ | O2 write 近邻去重/矛盾（`b7b9528`+worker） | 🔍 | M | 预防膨胀与矛盾并存（异步 worker，落 PendingConfirmation + suspected_supersede 边，守 verbatim） |
| P1 | O3 capsule 多样化 | 🔍 | S | 消除头部近似条目霸占 |
| P2 | O4 graph degree 衰减 | 🔍 | S（一行式） | 抑制热门节点过度 boost |
| P2 | O5 secret 脱敏 | 📦/⚙️ | M（两层设计，先定边界） | 降低 verbatim 带来的泄露面 |

> commit close 引用：O1 已落地 = `feat(schema): add last_used_at column` (`808cb59`) + `feat(lifecycle): retrieval reinforcement resets the decay clock via last_used_at` (`709c648`) + `docs(agents)` (`181fe67`)。
