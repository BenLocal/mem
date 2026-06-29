# ④ OSS 记忆引擎对照 — mem0 / agentmemory (v1 — 2026-06-05)

> 这是 mem 的**第四条对照线**，与 ① 平行但参照系不同：
> - **①** MemPalace 对齐 —— [`mempalace-diff.md`](./mempalace-diff.md) (v1–v4) + 执行面板 [`ROADMAP.MD`](./ROADMAP.MD)（#1–#36 / K1–K12）
> - **②** Backend 存储抽象 —— [`backend-coupling.md`](./backend-coupling.md)
> - **③** 长内容召回 —— [`long-content-recall.md`](./long-content-recall.md)
> - **④** OSS 记忆引擎对照（本篇）—— 参照 [mem0](https://github.com/mem0ai/mem0) 与 [agentmemory](https://github.com/rohitg00/agentmemory)，沿 **write → recall → feedback** 主轴找 mem 可借鉴的更优处理；产出 **O1–O5 + O7**（O6 来自 2026-06-26 更广赛道扫描，见 §O6）
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

## 5. 可借鉴的更优处理 → O1–O7

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

**已落地（`808cb59` 加列 + `709c648` 行为，以代码为权威）**：实际形态与上面 (b)(c) 有两处偏离，都是 lance 的 UPDATE 限制逼出来的（route-B 后 decay 写经 lancedb Rust `table.update()`，非 DuckDB 扩展）：
- (b) bump 触发点**没放进 `compress.rs`**（那是个纯函数，无 store/异步）。改成：`search` 在 `compress()` 返回后收集**真正写进输出**的 capsule_id（directives+facts+patterns，见 `SearchCapabilityCapsuleResponse::emitted_capsule_ids`），经 unbounded channel 异步丢给新的 **always-on `last_used_worker`**（仿 `potentiation_worker`）；worker 攒批去重后 `Store::bump_last_used_at` 盖 `last_used_at`，每 tenant 每 tick 一次批量 flush。"used"=进输出的语义不变，且彻底不在读路径上写。
- (c) decay 锚**不能直接写 `COALESCE(last_used_at, updated_at)`**：lance UPDATE 的 SET 表达式不支持 `COALESCE`（"Not implemented"），且 WHERE 不支持多值 `IN`（"pushed table filters"）。改成：decay sweep 拆**两条 WHERE 互斥的 UPDATE**（`last_used_at IS NOT NULL` 锚 last_used_at、`IS NULL` 锚 updated_at，每行只命中一次）；bump 逐 id 单等值 UPDATE。额外精化：sweep 的**每 tick 重置列从 `updated_at` 改成 `last_used_at`**，于是 `updated_at` 回归纯写时间 freshness（旧账：原本被 sweep 每小时刷平），`decay_score` 仍是累加器、feedback delta 不被抹。
- (d) **retrieve freshness bonus ✅ 已补（原标 optional）**：`pipeline/retrieve.rs` 的 freshness 评分项原锚 `updated_at`（纯写时间）；新增纯函数 `freshness_anchor_ts(memory)` = `last_used_at` 有效则用、否则回退 `updated_at`（空串/0 也回退），在 `apply_lifecycle_score`（per-memory）与 `score_with_hybrid`（pool `newest`）两处接入。与 (c) 的 decay 锚**对称**——用过的胶囊在**读路径**也更新鲜，闭合 O1「检索即强化」读写两端。`freshness_score` 量级窄（`[-14,6]`、tiebreaker 级），改动有界；O6b 金标回归门验证 recall 无回退（baseline 全绿）。单测 `freshness_anchor_prefers_last_used_then_updated_o1` + `used_capsule_ranks_fresher_than_untouched_o1` 覆盖。
- 旧表迁移：`migrate_capability_capsules_add_columns`（`add_columns(AllNulls)`）回填旧行 NULL → sweep 回落 `updated_at`，由 `legacy_uint64_version_compat` 端到端验证。新增 env `MEM_LAST_USED_FLUSH_SECS`（默认 5）。

### O2 🔍 — write 时近邻去重 / 矛盾检测（P1）✅ done（`b7b9528` refactor + worker，2026-06-05）

**现状（code）**：`pipeline/ingest.rs` 去重只有 `content_hash` 完全一致；同义改写直接落新行，矛盾并存，要等 agent 手动 `supersede`。

**借鉴**：mem0 抽取时把 `existing_memories` 喂给 LLM 避免重复；agentmemory 的矛盾检测。

**改法**：ingest 在 `content_hash` 检查之后，对新 capsule 做一次 semantic top-k 近邻查询；cosine 超阈值则**不自动覆盖**（守 verbatim），而是落 `PendingConfirmation` 并标注"疑似 supersede `<id>`"，进 `list_pending_review` 由人/agent 定夺。

**触点**：`pipeline/ingest.rs`（hash 检查后加近邻照会）、复用既有 semantic search + review 队列。
**风险**：低-中。阈值要保守（宁可漏判不可误并）；近邻查询走异步，别拖 ingest 延迟。

**已落地（以代码为权威）**：检查放在**异步 embedding worker** 里（用户拍板）——`post_embed` 完成 job 后，对刚嵌入的 `Active` capsule 用 `hybrid_candidates(空 text + 新向量)` 取 top-k、排除自身、逐候选取存量向量算精确 cosine；最高者 ≥ 阈值则 `set_capsule_status(.., PendingConfirmation)` + 写一条 `suspected_supersede` 图边。两处偏离原 (改法)：① **标注从 tag 改成图边**——lance 的 `.update()` 改不了 List 列（tags），且检查在 capsule 已落库后跑、只能事后写；图边是 mem 里 memory→memory 指针的惯用且安全表达（与 `contradicts`/`supersedes` 同族）。② 触发点**没放进 `compress.rs`/ingest 同步路**（那要每次 ingest 多一次同步嵌入）；放异步 worker 后零 ingest 延迟、且复用 worker 刚算出的向量。前置：把 `accept_pending`/`reject_pending` 合并成 `set_capsule_status` 单一原语（`b7b9528`），O2 的翻转直接复用、无需新 trait 方法。opt-in `MEM_INGEST_NEARDUP_ENABLED`，阈值 `MEM_INGEST_NEARDUP_THRESHOLD`（默认 0.92）。

### O3 🔍 — capsule recall 多样化（MMR / per-source cap）（P1）✅ done（`pipeline/retrieve.rs`，2026-06-05）

**现状（code）**：transcript 侧有 session **co-occurrence**（`transcript_recall.rs::score_candidates` 的 `session_counts`，那是**加权**，不是去重）；**capsule 侧没有任何多样化**。同一 supersede 链派生、或同 session 批量 ingest 的近似 capsule 可能霸占头部。

**借鉴**：agentmemory 的 `max 3 per session`。

**改法**：在 `pipeline/retrieve.rs::finalize` 排序后、截断前，加一个轻量 per-source（session / supersede-chain root / repo:module）配额裁剪或 MMR 式去冗。注意与 transcript 的 co-occurrence 是**两个相反方向**的东西，别混用同一函数。

**触点**：`pipeline/retrieve.rs::finalize`。
**风险**：低。配额是软上限，命中不足时回填。

**已落地**：选了**轻量 per-source 配额裁剪**（非 MMR——MMR 要 pairwise 相似度、更重，收益边际）。`retrieve.rs::finalize` 在 floor 过滤后调 `diversify_by_source(ranked, per_source_cap())`：source key = `session_id`（没有则用 capsule 自身 id，即不分组），每 source 在 head 至多 `cap`（默认 3）条，超额按原排名挪到**尾部不丢弃**（软上限，compress 仍可按 budget 纳入）。`supersedes` 链那一路问题其实 `hybrid_candidates` 的 SQL 早已做版本链去重（`NOT EXISTS superseded-by-active`），所以这里只需治"同 session 批量 ingest 霸占头部"。默认开 `MEM_RECALL_PER_SOURCE_CAP`（=0 关），与 transcript 侧 session co-occurrence 是相反方向、独立函数。

### O4 🔍 — graph boost 按 degree 衰减（P2）✅ done（`retrieve.rs::compute_graph_boosts`，2026-06-05）

**现状（code）**：`score_with_hybrid` 吃 `graph_boost_by_id`，但 boost **不看 entity 的度数**。挂在热门 entity（如 `project:` / `repo:` 节点）下的一大批 capsule 会被一刀切 boost，把真正相关的压下去。

**借鉴**：mem0 的 spread 衰减 `boost / (1 + 0.001*(num_linked-1)^2)`。

**改法**：graph boost 按锚 entity 的邻居数（度）做反比衰减；高扇出节点贡献的 boost 自动摊薄。

**触点**：graph boost 计算处（`graph_anchor_nodes` 下游、`score_with_hybrid`）。
**风险**：极小（基本是一个除式）。

**已落地**：`compute_graph_boosts` 加 `spread_decay(degree) = 1/(1+0.001·(degree-1)²)`，`boost = round(GRAPH_BOOST·spread·strength)`，max-over-anchors。`degree` = 锚 entity 的 **capsule fanout**（只数 capsule 端点，entity↔entity 的共现/tunnel 边不计）。比"一行式"略多一处结构改动：原 dynamics-OFF 路用扁平批量 `related_capability_capsule_ids`（丢了 per-anchor 归属），改成和 dynamics-ON 路一样**逐 anchor 走 `neighbors_within`**，才能拿到每个 anchor 的 degree——mem 是 local-first、图规模适中，N 个 anchor（top-5 capsule 的实体，有界）的额外 round-trip 可忽略。degree≤1 不衰减；现有 dynamics 测试 degree=2、spread≈0.999 四舍五入到原值，无回归。无 env 开关（公式固定）。

### O5 📦/⚙️ — ingest secret 脱敏（P2）✅ 落地

**现状（code）**：mem 把 transcript 整段 verbatim 存（`conversation_messages`）。相比两家，**verbatim 反而抬高了密钥泄露面**。

**借鉴**：agentmemory 存前 privacy filter（剥 API key / secret / `<private>`）。

**改法（两层，守 verbatim）**：原文在 access-control 下原样保留；只在**索引 / 输出层** redact 高置信密钥模式。不在存储层就地改写 content（那会破 verbatim 原则）。

**已落地（以代码为权威）**：新模块 `pipeline/redact.rs`——`redact_secrets`(默认开,opt-out `MEM_REDACT_SECRETS_DISABLED=1`;无命中走 `Cow::Borrowed` 零分配快路)。**两个 seam**(用户拍板的覆盖面=嵌入前 + 答案 + 横幅,不含 transcript):
- **答案 + 横幅**:redact 接在 `compress::compress_text`——所有压缩输出 prose(directive/fact/pattern body + source_summary + workflow goal/steps/signals)的**唯一 choke point**,一处即覆盖 MCP/HTTP search response;recall 横幅渲染的就是该 response,**transitively 覆盖横幅**,无需动 `hook.rs`。
- **嵌入前**:redact 接在 `worker/embedding_worker.rs::embed_input_chunks`,密钥不进向量索引。
- **白名单**(高置信,token 模式带 `\b` 防 `ask-`→`sk-` 误命中):OpenAI/通用 `sk-`、AWS `AKIA`、私钥块 `-----BEGIN…PRIVATE KEY-----`、`<private>…</private>`、GitHub `gh[posru]_`、JWT、`Bearer`。替换成 `[redacted:<kind>]`。
- **不动**:`capability_capsule_get`(显式 verbatim-fetch、access-controlled)+ 落盘 `content`/transcript content 保持原样。

**触点**：`pipeline/redact.rs`(新)、`pipeline/compress.rs::compress_text`、`worker/embedding_worker.rs::embed_input_chunks`。**不动** `memories.content` / transcript `content` 落盘。
**风险**：中→低。误 redact 风险用「高置信模式 + `\b` 边界 + 仅输出层(存储 verbatim 不破)」收口;默认开是因为纯输出层 mask 不碰存储。

### O6 🔍/⚙️ — 召回质量 eval 框架：金标集 + parity bench + CI 回归门（P1）✅ O6a/O6b 落地（`2e7a68f`）· O6c harness 就绪、真集数待快机 ★最高杠杆的质量基础设施

> **来源与其它 O 项不同**：O1–O5 与 O7 参照 mem0/agentmemory；唯独 O6 来自 2026-06-26 的更广赛道扫描（Zep/Graphiti 报 LongMemEval 63.8%、agentmemory 报 recall@5 95.2%、Cognee 自带 bench）——全赛道都用**可复现的准确率数**自证，而 mem 对外拿不出一个「召回有多准」的数。

**现状（code，2026-06-26 实读 —— O6 是收口扩展，不是从零造）**：mem 其实已有 eval 地基：
- `src/pipeline/eval_metrics.rs` —— 完整 IR 指标纯函数（`recall_at_k` / `ndcg_at_k` / `mrr` / `precision_at_k` / mempalace 式 `recall_any_at_k` / `recall_all_at_k`），全部 handworked 单测覆盖。
- `tests/recall_bench.rs` + `tests/bench/{runner,synthetic,fixture,geometry}.rs` —— 8-rung 消融 bench（Lexical / Semantic / Hybrid / Graph / Dynamics / Chunking On-Off / Oracle），出 `pretty_table` + `write_json` 到 `target/recall_bench/`；spec 在 `docs/superpowers/specs/2026-06-01-recall-bench-rebuild-design.md`。

**三个真缺口**：
1. **语料是 `bench::synthetic::generate` 合成的** —— 只能测 rung 之间的相对排序，测不出真实 query 在真实语料（尤其中文重语料）上的绝对召回。`eval_metrics.rs` 文件注释里点名要的 `tests/mempalace_bench.rs`（LongMemEval/mempalace parity）**留了坑但没建**。
2. **bench 是 `#[ignore]` 一次性**、写本地 JSON，**不进 CI、无回归门** —— RRF 权重 / `MEM_MIN_SCORE` / O1 衰减锚 / O3 配额这些旋钮一改，recall 悄悄回退没有任何 gate 拦。
3. **没有对外可比的公开数** —— Zep/agentmemory/Cognee 都有 LongMemEval / recall@k 自证，mem 回答不了「O1–O4 到底让召回更准还是更差」。

**借鉴**：Zep 的 LongMemEval parity（同一公开任务跑自家 pipeline 出可比数）；agentmemory 的 recall@5 自报口径；mem 自家 bench-driven 文化（QW-1 拆 RRF 先 bench 再落地）。

**改法（三小步，全增量、不碰主架构）**：
- **O6a 真实/中文金标集 ✅（`2e7a68f`）**：建 `tests/golden_recall/`（区别于已有的 `tests/golden/` SQL 快照）—— `corpus.json`（18 条 mem 自身技术 capsule，6 主题、锚词互斥，**脱敏 + 通用占位名**守 [[no-real-client-names-in-code]]）+ `qrels.json`（8 query → relevant id 集）+ `baseline.json`。确定性 `GeometryProvider` 向量 + 真 jieba BM25，喂 `eval_metrics.rs` 出 recall@k/ndcg/mrr。
- **O6b CI 回归门 ✅（`2e7a68f`）**：金标集 bench 做成**非 `--ignored`** 的 hermetic test（确定性向量、~8s），断言每指标 ≥ 版本化 `tests/golden_recall/baseline.json`，进 `ci.yml` 命名步骤「Recall regression gate」。对抗验证过(不可达 baseline → 红)。改排序的 PR 要么不掉、要么显式更新 baseline。
- **O6c LongMemEval parity bench ✅ harness 就绪 / ⚠️ 真集数待快机（`tests/mempalace_bench.rs`）**：补上注释里留坑的 `tests/mempalace_bench.rs`，`#[ignore]` 一次性、不进 CI。指标口径 = **session 级 memory recall@k（命中 `answer_session_ids`,LongMemEval 官方指标之一）**,复用 `eval_metrics::recall_any_at_k`;**显式区别于端到端 QA accuracy**(Zep 63.8% 那种,需 QA 模型 + LLM judge,本环境无网关,不做)。数据集 = 官方 `longmemeval_s_cleaned.json`(HF,500 题 ×~48 haystack session,277MB,**gitignore 不进仓**,`tests/mempalace_bench/data/` drop-in);缺文件时回退到 committed `subset.json`(6 条 format-faithful **synthetic 子集**,数仅示意)。**真集数未在本机产出**:Qwen3-0.6B 本地 CPU(contended)嵌 LongMemEval 长 haystack 太慢——**N=6 探测跑 1h40m 都没完**,全 500 题(~2.4 万 session 嵌入)不具备出数条件,需 GPU 或非 contended CPU 机器复跑。

**验收**：(O6a) `cargo test --test golden_recall -- --nocapture` 打印一张 recall@{1,5,10} / ndcg@10 / mrr 表 ✅；(O6b) 故意把 `RRF_K` 改坏 → CI 红 ✅；(O6c) `cargo test --test mempalace_bench -- --ignored --nocapture` 在真集上产出一行可写进 README 的 session-recall 数 —— harness ✅，**真集数待快机**（见下）。

**触点**：`tests/golden_recall/`（语料 + qrels + baseline）+ `tests/golden_recall.rs`（复用 `eval_metrics.rs`）+ `.github/workflows/ci.yml`（O6b 命名步骤）；`tests/mempalace_bench.rs` + `tests/mempalace_bench/{subset.json,.gitignore}`（O6c harness，复用 `eval_metrics` + 真 `EmbedAnythingEmbeddingProvider`）。**不动 `src/`**（指标库已齐）。
**风险**：低。纯测试侧增量，无 src 改动、无新依赖。把关点：金标集**脱敏 + 通用占位名**（公开仓）+ qrels 标注质量；O6c 真集 277MB **必须 gitignore**（已挡 `tests/mempalace_bench/.gitignore: data/`）。

**已落地（以代码为权威）**：
- O6a/O6b：`feat(eval): gold-set recall regression gate`（`2e7a68f`，CI 全绿）。
- O6c harness：`tests/mempalace_bench.rs`（`#[ignore]`）+ committed `subset.json`（6 条 format-faithful synthetic）+ `tests/mempalace_bench/.gitignore`（挡 277MB 真集）。bench 逻辑：每题 fresh `Store` → 每 haystack session 一条 capsule（真 Qwen3 batch 嵌入 + upsert）→ question 走真 hybrid `rank_with_hybrid_and_graph` → ranked capsule 映回 session id → `recall_any_at_k`/`recall_at_k`/`mrr` over `answer_session_ids`；type-stratified `LONGMEMEVAL_SAMPLE`（默认 50，0=全 500）。
- **真集数（公开数）= 未产出**：本机 Qwen3-Embedding-0.6B（CPU、contended）嵌 LongMemEval ~48-session 长 haystack 太慢，**N=6 探测 1h40m 未完**；全 500 题 ~2.4 万 session 嵌入在本机不可行。需在 **GPU 或非 contended CPU** 机器上 drop-in 真集后复跑 `cargo test --test mempalace_bench --ignored` 才能得到可对外引用的 session-recall@k。在那之前 README **不写**任何 LongMemEval 数（避免拿 synthetic 子集示意数误导对比）。

#### O6d 在线观测计数 ✅ 落地（O6 离线 eval 的在线补充）

O6a/b/c 衡量召回质量都是**离线**（CI / 一次性 bench）。线上跑着的服务**在做什么**——O5 脱敏到底有没有触发、O7(a) 近重复闸是过敏还是过松、feedback 回流率多少——之前只能 grep 日志。补一个**进程内原子计数器**注册表（`src/metrics.rs`，`once_cell::Lazy` 单例 + `AtomicU64`，零新依赖、不穿 `AppState`），经 `GET /metrics` 返回 JSON 快照（与 `/…/stats` 同风格、只读免鉴权同 `/health`）。计 5 类:`ingest_total`/`search_total`（速率分母）、`redaction_hits`（O5 命中:`redact_secrets` 返回 `Cow::Owned` 即 +1，不污染纯函数 `redact_all`）、`neardup_flags`（O2/O7(a) 翻 `PendingConfirmation` 成功后 +1）、`feedback_*`（按 6 种 `FeedbackKind` 分桶，typed 路由——新增 variant 是编译错而非静默漏）。**进程本地、重启清零、不持久化**（同 `MEM_MAX_INGEST_PER_SESSION` 语义），`Relaxed` 序、读路径无锁。埋点全在 choke point（`redact_secrets` / `embedding_worker::flag_if_near_duplicate` / `service::{ingest,ingest_batch,search,submit_feedback}`），坏请求 422 在进 handler 前被拒、不计数（端到端 smoke 验证过）。**触点**：新 `src/metrics.rs` + `src/http/metrics.rs` + 5 处 `metrics().inc_*()`。**风险**:极低（只读端点 + 计数器,不影响任何业务逻辑）。

### O7 🔍 — 对标 Mem0 的自动抽取 + 冲突消解：零-额外-LLM 版（P1）(a)(b)(c) ✅ ★(a)(b) 零 LLM，(c) opt-in 默认关 + fail-safe

> **硬约束**：本部署**没有多余的生成式 LLM**（无可达网关，见 O6c 勘探）。所以这条对标 Mem0「写时 LLM 抽取 + ADD/UPDATE/DELETE 对账」的能力，**默认形态必须零生成式 LLM**——靠已有的 embedding + 启发式 + review 队列拿到 80% 价值；真要 mem0 级细腻抽取，做成**默认关**的 opt-in lane，没配 LLM 时静默退回现状，**永不强依赖**。这与 §6「不做 inline LLM 抽取」不矛盾：(a)(b) 不是 LLM 抽取（是嵌入去重 + 规则抽取，守 verbatim），(c) 才是那条被 §6 排除的路——所以把它关在 opt-in 后面。

**现状（code）**：
- **写时去重**：`pipeline/ingest.rs` 只有 **exact `content_hash`**；同义改写落新行。O2（`worker/embedding_worker.rs::flag_if_near_duplicate`，opt-in 默认关）已做**pairwise** 近重复（new vs 最近邻，cosine ≥ `neardup_threshold` → `PendingConfirmation` + `suspected_supersede` 边）。
- **簇级语义聚类**：`pipeline/evolution/map.rs::build_clusters`（pairwise cosine union-find）+ `worker/evolution_worker.rs` 的 merge/generalize 已有，但那是**背景批量扫全池**、且 merge 直接 **Archive loser**（非 review 提案）。
- **抽取**：只有显式 `<mem-save>` 标签（`cli/mine.rs` 离线）或 agent 主动供 fact 才进库；**未打标签的对话里的高信号内容全丢**。
- **冲突消解**：靠 agent 手动 `supersede` / bitemporal `invalidate_edge`；无自动「检出语义对立」（O2 抓的是近重复，不是矛盾）。

**借鉴**：Mem0 写时把 `existing_memories` 喂 LLM 做 ADD/UPDATE 对账 + entity-link；agentmemory 的矛盾检测。**只取其形、不取其 LLM**。

**改法（三 lane）**：
- **(a) 【最高价值、零 LLM】ingest 语义近重复去重 → supersede 提案（在 O2 之上推广到簇级）✅ 落地**：O2 已落地 pairwise 版；O7(a) 把 `worker/embedding_worker.rs::flag_if_near_duplicate` 从「单个 cosine 最近邻」升级成「收集全部近重复（cosine ≥ 阈值）→ 选簇 canonical（最长内容、tie 取较早）→ 对 canonical 提 `PendingConfirmation` + `suspected_supersede` 提案」，**不自动 Archive**（守 verbatim/review，区别于 evolution merge）。单近重复时退化成 O2（无回归）。
  - **落地偏离（以代码为权威）**：没有字面复用 `evolution/map.rs::build_clusters` 的 union-find——那是**全池聚类**；单条新 capsule 的「簇」就是它的近重复集，所以只需复用 `evolution_worker::execute_merge` 的 **keep-longest canonical 规则**（`max_by(content.len).then(earlier created_at)`），提到纯函数 `pick_cluster_canonical`（单测覆盖）。候选 K 从 O2 的 5 提到 12 以容纳簇成员。extractor tag `o7_neardup_cluster`。**复用现有 Qwen3 embedding，零新模型、零 LLM**，沿用 O2 的 `MEM_INGEST_NEARDUP_ENABLED`/`_THRESHOLD`（默认仍关；验证稳后可评估默认开）。
- **(b) 【零 LLM】启发式抽取 lane ✅ 落地**：新模块 `cli/heuristic_extract.rs::heuristic_candidates`——对**未打 `<mem-save>`** 的 assistant 文本块抓高信号句（决策/因果/error→fix/含 `code_ref`/命中已知实体），每条作为 `ExtractedMemory{pending:true}` 由 miner 以 `write_mode:"propose"` → `PendingConfirmation` 落 review 队列（**绝不 Active**）。**纯正则/启发式 + `entity_normalize`,零 LLM、零新模型**。
  - **守住历史教训**：pre-2026-05-08 的散文 cue 抽取因「元提及」误造 **Active** 记忆被删除（见 `cli/mine.rs` 头注）；本次安全的关键 = **opt-in 默认关**（`MEM_MINE_HEURISTIC_EXTRACT=1`）+ 高精度 cue + 硬垃圾过滤（≥12 字/≤400/≥4 实词）+ 每条都 **review-gated**（PendingConfirmation，一次 reject 即弃）。
  - **落地细节**：分句正则 `[。！？\n]+|[.!?]\s+`（ASCII `.` 仅在后接空白才算句界，保住 `decay.rs` 等路径/版本号不被切碎）；每块至多 `MAX_PER_BLOCK=2` 候选 + 去重；idempotency key 加 `:h{sha8(content)}` 后缀避免与同行 `<mem-save>` 碰撞、且重跑幂等。entity cue 接口已留（接受 alias 切片），miner v1 传空（实体注入留作后续）。assistant 文本块限定（与既有抽取同范围,保守）。
- **(c) 【opt-in，默认关】生成式 LLM lane ✅ 落地**：新模块 `cli/llm_extract.rs`——`mem mine` 时对未打 `<mem-save>` 的 assistant 文本块调内部网关 `chat/completions` 抽原子事实 → `PendingConfirmation` 候选。**三层 fail-safe 保证永不强依赖**:① `MEM_MINE_LLM_EXTRACT` 默认关;② `LlmExtractConfig::from_env` 缺 `LLM_API_BASE`/`LLM_MODEL` 返回 `None`→lane 不活;③ `llm_candidates` 吞掉一切错误(网络/非 2xx/解析)返回空→静默退回 O7(a/b)/tag。**(c) 开启时 supersede (b)**(同块不双挖)。
  - **落地细节**：reqwest client `.no_proxy()`(= httpx `trust_env=False`,否则内网网关 IP 走公网代理 502,见 `internal-llm-gateway` skill);空 `LLM_API_KEY`→不发 Authorization(内网边缘鉴权);`caller:"mem-mine-o7c"` 统计;每块 ≤5 候选 + 同款长度过滤;每 mine 最多 40 块封顶(防打爆网关);`parse_candidates` 纯函数(剥 code fence、切首个 `[...]`、JSON 解析、过滤),单测覆盖。pending 候选走与 (b) 同一 `write_mode:"propose"` + `:h{sha8}` 幂等路径。

**验收**：
- (a) ingest 一条语义近重复（cosine ≥ 阈值但非 exact-hash）→ 新 capsule 落 `PendingConfirmation` + 指向簇 canonical 的 `suspected_supersede` 边；原文 verbatim 不动；**全程不调 LLM**。
- (b) 喂一段未打标签、含明确决策句的对话 → review 队列出现一条 `PendingConfirmation` 候选；**不调 LLM**。
- (c) 未配 LLM 网关时行为 == (a)+(b)（无报错、无 LLM 调用）；配了 + 显式开关才走细腻抽取。

**触点**：
- (a)：`pipeline/ingest.rs`（dedup 决策点）、`worker/embedding_worker.rs`（扩 `flag_if_near_duplicate` pairwise→簇级）、`pipeline/evolution/map.rs`（复用 `build_clusters`/union-find 原语）、graph `suspected_supersede` 边 + `set_capsule_status`。**检查走异步 worker、不拖 ingest 同步路**（同 O2 纪律）。
- (b)：`cli/mine.rs`（cue 规则复用）、`pipeline/entity_normalize.rs`、新的 ingest-旁路启发式模块 + review 队列（`PendingConfirmation`）。
- (c)：新的 opt-in extractor（env 开关后）+ 内部网关 client（新），默认关。

**风险**：(a) 低-中——阈值要保守（复用 O2 的 `MEM_INGEST_NEARDUP_THRESHOLD`，宁漏不误），簇查询必须异步、不拖 ingest。(b) 中——启发式精度，误抓会刷屏 review；靠 cue 高精度 + 一律落 `PendingConfirmation`（review-gated，无害）收口。(c) 低（默认关）——只在显式配置时活，缺 LLM 必须 fail-safe 退回 (a)+(b)。**(a)(b) 零 LLM 可直接做；(c) 默认关、对标但不强依赖。**

---

## 6. 不做 / 本质形态差异

- **inline LLM fact 抽取（write 时 / 默认路径）** —— mem0/agentmemory 在写时用 LLM 把对话压成结构化 fact；mem 的**默认路径故意不做**，抽取放在 `mem mine` 离线管线、落盘 verbatim（保真 > 即时结构化）。**O7(c) 提供了一条 opt-in 逃生口**（`mem mine` 离线、非 write 路径、`MEM_MINE_LLM_EXTRACT` 默认关、缺 LLM 静默退回、产出仍是 review-gated 候选），所以默认哲学不变——LLM 抽取始终关在显式开关后，不是默认行为。
- **只增不删（mem0 additive）** —— mem 用 supersede 链 + 状态机表达版本，比 mem0 纯堆积更可控，**不回退**。
- **四层 consolidation（agentmemory）** —— mem 已有近似分层：transcript archive(生) ≈ Working、capsule ≈ Semantic、`workflow.rs` episode→workflow ≈ Procedural。Episodic（会话级摘要）是唯一缺口，但价值待测，**暂不立项**。

---

## 7. 落地顺序

| 优先 | 项 | 层 | 工作量 | 价值 |
|---|---|---|---|---|
| **P0 ✅** | O1 使用强化 + 衰减重置（`808cb59`+`709c648`） | 🔍 | M（加列 + last_used worker + decay 锚） | 直击 mem 最大结构性弱点：让 ranking 在 agent 不回调时也能自主生长 |
| P1 ✅ | O2 write 近邻去重/矛盾（`b7b9528`+worker） | 🔍 | M | 预防膨胀与矛盾并存（异步 worker，落 PendingConfirmation + suspected_supersede 边，守 verbatim） |
| P1 ✅ | O3 capsule 多样化（`retrieve.rs`） | 🔍 | S | 消除头部近似条目霸占（per-source 软配额，默认 3，session 为 key） |
| P2 ✅ | O4 graph degree 衰减（`retrieve.rs`） | 🔍 | S | 抑制热门节点过度 boost（spread_decay，按锚 fanout 反比） |
| **P2 ✅** | O5 secret 脱敏 | 📦/⚙️ | M | 降低 verbatim 泄露面。**✅ 落地**：`pipeline/redact.rs`(默认开,opt-out `MEM_REDACT_SECRETS_DISABLED`)接在 `compress_text`(覆盖答案+横幅)+ `embed_input_chunks`(嵌入前);白名单 sk-/AKIA/私钥块/`<private>`/GitHub/JWT/Bearer,带 `\b` 防误命中;存储 verbatim 不动、`capability_capsule_get` 不 redact |
| **P1 ✅/⚠️** | O6 召回质量 eval 框架（金标集 + CI 门 + parity） | 🔍/⚙️ | M | 🔴 全赛道入场券。O6a/O6b ✅（`2e7a68f`，CI 全绿回归门）；O6c harness ✅、**真集公开数待快机**（本机 Qwen3-0.6B CPU N=6 跑 1h40m 未完，需 GPU/非 contended CPU drop-in 真集复跑） |
| **P1 (a)(b)(c)✅** | O7 Mem0 式自动抽取 + 冲突消解（零-额外-LLM 版） | 🔍 | (a) S-M ／ (b) M ／ (c) M | 🟠 对标 Mem0 写时抽取/对账，但默认零生成式 LLM。**(a) ✅**：簇级语义近重复→canonical supersede 提案（`flag_if_near_duplicate`+`pick_cluster_canonical`，`o7_neardup_cluster`）；**(b) ✅**：启发式高信号抽取→PendingConfirmation（`heuristic_extract.rs`，opt-in `MEM_MINE_HEURISTIC_EXTRACT`）；**(c) ✅**：生成式 LLM lane（`llm_extract.rs`，opt-in `MEM_MINE_LLM_EXTRACT` 默认关 + 三层 fail-safe，缺 LLM 静默退回 (a)+(b)）。三条都 review-gated。 |

> commit close 引用：O1 已落地 = `feat(schema): add last_used_at column` (`808cb59`) + `feat(lifecycle): retrieval reinforcement resets the decay clock via last_used_at` (`709c648`) + `docs(agents)` (`181fe67`)。
