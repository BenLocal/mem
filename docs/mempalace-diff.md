# mem ⇄ MemPalace 对照 & 借鉴清单

> 目的：记录本工程（`mem`，Rust + axum + DuckDB）与 [MemPalace](https://github.com/MemPalace/mempalace)（Python + ChromaDB + 独立 SQLite KG）的实现差异，沉淀**可向 MemPalace 借鉴的设计**与**对方在我们眼里值得保留的优势**。后续基于这份文档落地改造。
>
> 阅读对象：本人 + 协作的 agent。每个改造项尽量给到**触点文件 / 大致工作量 / 风险**，避免再读源码。

---

## 0. 一句话定性

| | mem | MemPalace |
|---|---|---|
| 形态 | 长驻 HTTP 服务（axum :3000），多 agent 共享一个 store | MCP server（stdio）+ CLI，单用户在自己机器上 |
| 哲学 | **结构化记忆生命周期**：status / supersedes / feedback / decay | **Verbatim 不改写**：原话存原话取，只压缩索引层 |
| 存储 | DuckDB 单文件（OLAP）+ indradb 图 | ChromaDB（SQLite + HNSW 段）+ 独立 SQLite 知识图 |
| 检索 | 加性整数打分（语义+词项+scope+intent+confidence+freshness+decay+graph） | 混合检索（BM25 + 向量）+ AAAK 索引层让 LLM 命中 |
| 输出 | **token-budgeted 四段式**：directives / relevant_facts / reusable_patterns / suggested_workflow | 返回 drawer 列表，让上层 LLM 自己整合 |

> **核心分歧**：`mem` 把记忆当**有生命周期的结构化数据**治理；MemPalace 把记忆当**不可变事实档案**保管。两条路线没有对错，但合并取舍要清楚。

---

## 1. 架构对照

### mem 模块拓扑（事实）

```
src/
├── domain/          memory / episode / query / workflow / embeddings 类型
├── storage/
│   ├── duckdb.rs    DuckDbRepository（memories / embeddings / jobs / feedback / episodes）
│   ├── graph.rs     GraphStore trait + IndraDbGraphAdapter + LocalGraphAdapter
│   └── schema.rs
├── embedding/       provider trait + embed_anything / openai / fake
├── pipeline/
│   ├── ingest.rs    initial_status / compute_content_hash / extract_graph_edges
│   ├── retrieve.rs  rank_with_graph_hybrid / merge_and_rank_hybrid / score_candidates_hybrid
│   ├── compress.rs  token-budgeted 四段式输出
│   └── workflow.rs  episode → workflow 抽取
├── service/
│   ├── memory_service.rs   编排
│   └── embedding_worker.rs 后台 job 消费者
└── http/            memory / review / graph / embeddings / health
```

### MemPalace 对应模块（参考）

```
mempalace/
├── mcp_server.py        所有 MCP tool 入口（≈ http/）
├── cli.py
├── miner.py             项目文件挖矿
├── convo_miner.py       会话转录挖矿
├── searcher.py          BM25 + 向量混合
├── knowledge_graph.py   三元组 + valid_from/valid_to（时序图）
├── palace.py            wing/room/drawer 操作
├── palace_graph.py      跨 wing tunnel
├── backends/            可插拔后端（chroma 默认）
├── dialect.py           AAAK 压缩格式
├── entity_detector.py   人/项目实体抽取
├── entity_registry.py   实体存储 + 消歧
├── layers.py            L0–L3 唤醒栈
└── hooks/               Claude Code Stop / PreCompact 钩子
```

---

## 2. 数据模型差异

### mem 的 schema（`db/schema/`）

- **`memories`**（主表）：`memory_id / tenant / memory_type / status / scope / visibility / version / summary / content / evidence_json / code_refs_json / project / repo / module / task_type / tags_json / confidence / decay_score / content_hash / idempotency_key / supersedes_memory_id / source_agent / created_at / updated_at / last_validated_at`
- **`memory_embeddings`**：`memory_id` PK，`embedding BLOB`，`embedding_model / embedding_dim / content_hash`，**线性扫描后算 cosine**
- **`embedding_jobs`**：持久化嵌入队列，`status ∈ pending|processing|completed|failed|stale`，`attempt_count` + `available_at` 用于退避
- **`episodes`**：`goal / steps_json / outcome / workflow_candidate_json` —— episode 是 workflow 的原料
- **`feedback_events`**：`feedback_kind` 影响 `confidence`/`decay_score`

### MemPalace 的存储模型

- **Wing**（人/项目）→ **Room**（日期/会话）→ **Drawer**（原文片段）
- ChromaDB collection `mempalace_drawers` 存 drawer 文档 + 向量
- 独立 SQLite KG：三元组 `(subject, predicate, object)` + `valid_from` / `valid_to`，**时序图**
- AAAK 是一个紧凑符号化的索引层，让 LLM 一次扫到目标 drawer

### 启示

| MemPalace 概念 | 是否值得引入 mem | 切入点 |
|---|---|---|
| Wing/Room/Drawer 三层结构 | ❌ 与现有 typed memory 冲突，不必引入 | — |
| 时序图（valid_from / valid_to） | ✅ **强烈建议** | `storage/graph.rs` 的 `GraphEdge` 加 `valid_from / valid_to`，`extract_graph_edges` 写当前时间，`supersedes` 路径上把旧边 `valid_to` 关闭 |
| Entity registry（人/项目消歧） | ⚠️ 可选 | `domain/` 加 `entity.rs`，`memories.project / repo / module` 改成外键，避免拼写漂移 |
| AAAK 索引层 | ❌ 与 token-budgeted 输出重复定位 | — |

---

## 3. 检索路径对照

### mem 现状（`pipeline/retrieve.rs::merge_and_rank_hybrid`）

```
score =
    semantic_sim × 64                       // [0, 64]
  + 26 if 同时在 lexical ∩ semantic
  + text_match_score                         // 词项命中 summary/content/code_refs/tags 多维加权
  + scope_score                              // scope_filters 命中 +18，未命中 -4
  + memory_type × intent                     // intent="debug" 时 Experience > Implementation > ...
  + confidence × 10
  + 3 if 已验证
  + freshness_score                          // 相对最新一条桶化
  - decay_score × 12
  - 4 if status ∈ {Provisional, PendingConfirmation}
  + 12 if 在 graph 邻居集合内
```

→ `pipeline/compress.rs` 按 `token_budget` 切成四段（30% / 35% / 20% / 15%），`compress_text` 按**词数**截断。

### MemPalace 现状

- `searcher.py` 走 BM25 + 向量两路 → 合并去重
- 没有 status / decay / confidence 调权
- 不做输出压缩，返回原文 drawer

### 启示（双向借鉴）

**mem → MemPalace 可借鉴的（先存档，本工程不动）**：
- token-budgeted 四段式输出
- decay/confidence/feedback 信号

**MemPalace → mem 可借鉴的**：

1. **真正的 ANN 索引**。当前 `memory_embeddings.embedding BLOB` 是**全表扫描算 cosine**——规模上去会跪。三个候选方案：
   - `usearch` crate（推荐）：单文件，支持持久化，和 DuckDB 共存；
   - `hnsw_rs` crate（纯 Rust，简单）；
   - DuckDB `vss` extension（与 bundled DuckDB 兼容性需测，PR 风险高）。
   - 选型建议：**`usearch` sidecar 索引文件 + DuckDB 行做权威源**，HNSW 损坏时可从 DuckDB 重建（对照 MemPalace 的 `mempalace repair --mode rebuild`）。
2. **HNSW 健康度自检**。MemPalace `backends/chroma.py::hnsw_capacity_status` 在每次启动比对 `sqlite_count` vs `hnsw_count`，超阈值就提示 repair。我们落 sidecar 索引后照搬这套自检 + 修复 CLI。
3. **混合检索的归一化打分**。当前 `score_candidates_hybrid` 是加性整数，semantic_sim×64 vs scope×18 量级失衡。建议改成两路 RRF（reciprocal rank fusion）或先归一到 [0,1] 再加权——对齐 MemPalace 的 BM25 + 向量混合写法。

---

## 4. 嵌入管线差异

### mem 的优势（保留）

- **持久化 job 队列**（`embedding_jobs` 表）+ 后台 worker（`service/embedding_worker.rs`）+ attempt 重试 + content_hash 失效检测——**比 MemPalace 内联 embed 更工业化**。
- 多 provider trait 化，`embed_anything` 本地 + OpenAI BYOK + fake 测试三选一。

### MemPalace 的做法

- 写时同步 embed（ChromaDB EmbeddingFunction），失败即写失败。
- 修复路径靠离线 `mempalace repair`。

### 启示

mem 这块**比 MemPalace 强**，不需要借鉴。但需要修两个 bug：

1. **`compute_content_hash` 用了不稳定哈希**（`pipeline/ingest.rs::compute_content_hash` 用 `std::collections::hash_map::DefaultHasher`，SipHash 进程内随机种子）——跨进程哈希不一致，作为 DB 索引/幂等键不安全。**改用 `sha2`**（依赖里已有）：
   ```rust
   use sha2::{Digest, Sha256};
   let mut hasher = Sha256::new();
   hasher.update(canonical_json_bytes);
   format!("{:x}", hasher.finalize())  // 截前 16 字符也行
   ```
   - 触点：`src/pipeline/ingest.rs:20-40`
   - 影响：现有 DB 中 `content_hash` 列要么全 reset，要么写一次性迁移。
   - 工作量：1 小时改 + 0.5 小时迁移脚本。
2. **`embedding_jobs` 的 live-job dedupe 在应用层**（schema 002 注释承认 DuckDB bundled 不支持 partial unique index）——并发提交同一 memory 多个 job 会有竞态。
   - 短期：在 `enqueue_embedding_job` 路径上加事务 + `INSERT ... WHERE NOT EXISTS (SELECT 1 FROM embedding_jobs WHERE tenant=? AND memory_id=? AND target_content_hash=? AND provider=? AND status IN ('pending','processing'))`。
   - 触点：`src/storage/duckdb.rs`（`enqueue_embedding_job` 实现）。
   - 工作量：2 小时。

---

## 5. 图层对照

### mem（`storage/graph.rs` + `pipeline/ingest.rs::extract_graph_edges`）

预定义 6 类边，写入时从字段自动派生：

| 关系 | 触发 |
|---|---|
| `applies_to` | memory → project |
| `observed_in` | memory → repo |
| `relevant_to` | memory → module（repo+module）|
| `uses_workflow` | memory → workflow（task_type 或 type=Workflow）|
| `supersedes` | memory → previous memory |
| `contradicts` | tags 中 `contradicts:<id>` 前缀 |

双实现：`IndraDbGraphAdapter`（基于 `MemoryDatastore`）+ `LocalGraphAdapter`（纯内存）。

### MemPalace（`knowledge_graph.py`）

- 三元组 `(subject, predicate, object)`
- **时序属性 `valid_from` / `valid_to`** —— 边可以"失效"而不是删除，支持时间点查询（"2025-06-30 那天 X 还在 Y 项目吗"）

### 启示

**强烈建议引入时序图**：

- `domain/memory.rs::GraphEdge` 加 `valid_from: String, valid_to: Option<String>` 两个字段
- `extract_graph_edges` 写边时填 `valid_from = now()`
- `supersedes` 路径触发时，把旧 memory 关联的边 `valid_to = now()` 关闭（保留历史）
- 检索的 `related_memory_ids` 默认只看 `valid_to IS NULL` 的活动边
- 加新接口 `neighbors_at(node_id, ts)` 支持时点查询

> 工作量：4–6 小时（含 indradb 的属性字段适配）；如果 indradb 属性 API 别扭，可考虑把图也换成 DuckDB 表（`graph_edges (from, to, relation, valid_from, valid_to)`），这样和主表统一备份/迁移更简单。

---

## 6. 状态机 / 生命周期 / 反馈

### mem 的强项（MemPalace 完全没有，建议保留并强化）

- `MemoryStatus`：`Provisional / Active / PendingConfirmation`
- `WriteMode`：`Auto / Confirm`
- `supersedes_memory_id` 形成版本链，`/memories/{id}` 返回 `version_chain`
- `feedback_events` → 影响 `confidence` / `decay_score`
- `last_validated_at` + `validation_score`

### 可改进点

1. **`decay_score` 当前是静态字段**——没有"时间到了自动衰减"的后台流程。可加一个低频 worker（类似 `embedding_worker`）按 `updated_at` 距今天数推 decay。
2. **`feedback_kind` 枚举不强**——schema 是 `text`。建议在应用层定义 enum，并把每种反馈对 confidence/decay 的增量做成可配置常量表。
3. **`PendingConfirmation` 队列没有 TTL**——`/reviews/pending` 可能堆积，建议加 `pending_since` + 自动降级到 `Provisional` 的策略。

---

## 7. Verbatim 原则的差异

MemPalace 第一原则是 "Verbatim always"——**永不改写用户内容**。当前 mem：

- ✅ `memories.content` 列存原文，`compress.rs` 只在**输出层**截断
- ⚠️ `memories.summary` 是调用方提供的——如果 agent 写入时已经"提炼"了，那就脱离 verbatim 了
- ⚠️ `compress_text` 按**词数**截断不是 token，容易在 CJK 文本上偏离 token_budget 很多

### 建议

1. 在 ingest 路径加一个 `assert!(content.len() > 0)`，**禁止 `summary` 与 `content` 完全相同时写入**——这是 agent 偷懒抄过去的信号。
2. 把 `compress_text` 的"按词截断"换成 `tiktoken-rs` 之类的 token 计数器，对中英文都靠谱。
3. 文档化：`summary` 是**索引/提示用**，`content` 是**事实源**——任何输出都必须基于 `content` 而不是 `summary`。

---

## 8. 改造路线图（建议优先级）

| # | 项 | 价值 | 工作量 | 风险 | 触点 |
|---|---|---|---|---|---|
| 1 | `compute_content_hash` 改 sha2 | 🔴 修正确性 bug | S（1.5h） | 需迁移 | `pipeline/ingest.rs` |
| 2 | `embedding_jobs` dedupe 走事务 + 条件插入 | 🔴 修并发 bug | S（2h） | 低 | `storage/duckdb.rs` |
| 3 | 引入 `usearch` sidecar ANN | 🟠 性能基础设施 | M（1–2 天） | 中（需要 repair 路径） | `storage/`、新增 `vector_index.rs` |
| 4 | HNSW 健康度自检 + repair CLI | 🟠 配套 #3 | S（4h） | 低 | 新增 `bin/mem-repair` |
| 5 | 图边时序化（valid_from/to） | 🟠 表达力 | M（4–6h） | 中 | `domain/memory.rs`、`storage/graph.rs`、`pipeline/ingest.rs` |
| 6 | 检索分数归一化 / RRF | 🟡 排序质量 | S（3h） | 低 | `pipeline/retrieve.rs` |
| 7 | `compress_text` 改 token 计数 | 🟡 输出准确性 | S（2h） | 低 | `pipeline/compress.rs` |
| 8 | `decay_score` 后台衰减 worker | 🟡 生命周期闭环 | S（3h） | 低 | `service/`、新增 `decay_worker.rs` |
| 9 | Entity registry（可选） | 🟢 数据卫生 | M（半天） | 中（迁移） | `domain/entity.rs`、schema |
| 10 | Verbatim 守护（content vs summary 校验） | 🟢 哲学一致性 | S（1h） | 低 | `pipeline/ingest.rs` |

> 🔴 = 修 bug；🟠 = 架构升级；🟡 = 体验优化；🟢 = nice-to-have。
> S ≤ 4h，M = 0.5–2 天，L > 2 天。

---

## 9. 不建议引入的 MemPalace 概念

- **Wing / Room / Drawer 隐喻**：和 mem 已有的 typed memory + scope 模型概念冲突，引入只会增加心智负担。
- **AAAK 压缩索引层**：mem 已经用 token-budgeted 输出解决了相同问题（让 LLM 看到的内容受控），AAAK 是对 BM25/向量的补充，不是替代。
- **Hooks 驱动的后台归档**：mem 是 HTTP 服务模型，不绑定具体编辑器；类比物是 caller agent 主动 POST，不需要 hook。
- **L0–L3 唤醒栈**：MemPalace 的 layered context 设计是为对话开场注入服务的；mem 的客户端（Codex/Cursor 等）有自己的开场注入逻辑，不应在服务端重做。

---

## 10. 信息源

- mem 的事实来自当前仓库代码（`src/`、`db/schema/`、`Cargo.toml`、`README.md`）。
- MemPalace 的事实来自其本地 checkout（`/root/workspace/master/mempalace/`），相关文件：`CLAUDE.md`、`mempalace/backends/chroma.py`、`mempalace/searcher.py`（README 元信息）、`mempalace/knowledge_graph.py`（元信息）。
- 凡是 README 没明说但代码里有的（如 `merge_and_rank_hybrid` 的具体权重、`embedding_jobs` 状态机），按本地代码为准；本文档与代码不一致时**以代码为权威**。

> 维护建议：每完成路线图中的一项，回来更新对应行的状态（✅ done / 🚧 in progress），并在 git commit message 里引用本文档章节号（例如 `feat(ingest): switch content_hash to sha2 (closes mempalace-diff §8 #1)`）。
