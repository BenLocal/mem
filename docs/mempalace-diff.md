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
| **Wing**（实体分组） | ❌ 已有等价物 | `project` / `repo` / `module` 字段 + `applies_to` 图边 |
| **Drawer**（原子单元） | ❌ 已有等价物 | `memories` 行就是 |
| **Room**（时间桶/会话容器） | ✅ **建议加**（mem 真没有） | 新增 `sessions` 表，`memories` / `episodes` 加 `session_id` FK；与 `episodes`（目标键）正交，不重复 — 详见 §11 |
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

### 设计原则（先确立，再排路线）

mem 的本性是 **"结构化记忆生命周期"**（status / supersedes / feedback / decay），这条路线**保留并强化**。同时借用 MemPalace 的 **"Verbatim 永不改写"** 纪律——但**只约束存储与原文对外的呈现层**。两条原则按层叠加，**不冲突**：

```
存储层（memories.content）             ← 📦 Verbatim 不改写、不丢话（借 MemPalace 纪律）
输出/索引层（compress / 排序 / 元数据） ← 🔍 Structured 加权 / lifecycle / 索引（保 mem 路线）
基础设施（持久化、性能、修 bug）       ← ⚙️ 不归入上面任一项
```

判定一项改造能不能做，就按这套套：

- **会改写或丢失原话**（无论存储还是输出）→ **禁止**，即便能省 token 或加快检索
- **只改排序/打分/元数据**（不动 `content` 也不裸奔截断） → **允许**，feedback / decay / status / supersedes 都属此类
- **修存储层使其更 verbatim** → **优先**（如 #10 守护、#7 token 截断）

并明确文档化：**`memories.content` 是事实源（fact source），`memories.summary` 只用于索引/提示**——任何对外承诺、引用、链接、回答必须基于 `content`，不能基于 `summary`。这条规约会随 §8 #10 一并落到 ingest 校验里。

### 路线图

| # | 层 | 项 | 价值 | 工作量 | 风险 | 触点 |
|---|---|---|---|---|---|---|
| 1 | ⚙️ | ✅ `compute_content_hash` 改 sha2（含启动迁移）| 🔴 修正确性 bug | S（1.5h） | 需迁移 | `pipeline/ingest.rs`、`storage/{schema,duckdb}.rs` |
| 2 | ⚙️ | `embedding_jobs` dedupe 走事务 + 条件插入 | 🔴 修并发 bug | S（2h） | 低 | `storage/duckdb.rs` |
| 3 | 🔍 | 引入 `usearch` sidecar ANN | 🟠 性能基础设施 | M（1–2 天） | 中（需要 repair 路径） | `storage/`、新增 `vector_index.rs` |
| 4 | ⚙️ | HNSW 健康度自检 + repair CLI | 🟠 配套 #3 | S（4h） | 低 | 新增 `bin/mem-repair` |
| 5 | 🔍 | 图边时序化（valid_from/to） | 🟠 表达力 | M（4–6h） | 中 | `domain/memory.rs`、`storage/graph.rs`、`pipeline/ingest.rs` |
| 6 | 🔍 | 检索分数归一化 / RRF | 🟡 排序质量 | S（3h） | 低 | `pipeline/retrieve.rs` |
| 7 | 📦 | `compress_text` 改 token 计数（CJK 不再按词裸奔截断）| 🟡 输出 verbatim 纪律 | S（2h） | 低 | `pipeline/compress.rs` |
| 8 | 🔍 | `decay_score` 后台衰减 worker | 🟡 生命周期闭环 | S（3h） | 低 | `service/`、新增 `decay_worker.rs` |
| 9 | 🔍 | Entity registry（可选） | 🟢 数据卫生 | M（半天） | 中（迁移） | `domain/entity.rs`、schema |
| 10 | 📦 | **Verbatim 守护**（ingest 校验 `summary != content`，禁止把提炼版塞 `content`；同时把"`content` 是事实源、`summary` 只做索引"写进 `AGENTS.md` / `README.md`） | 🟢 哲学一致性 | S（1h） | 低 | `pipeline/ingest.rs`、`AGENTS.md`、`README.md` |
| 11 | 🔍 | **Sessions**（时间桶容器，对齐 MemPalace 的 Room） | 🟠 表达力 | M（半天） | 中（schema 迁移）| 新增 `sessions` 表 + `memories.session_id` + auto-bucket on ingest，详见 §11 |

> **层（Layer）**：📦 = Verbatim 存储/输出纪律；🔍 = Structured 检索/排序/元数据；⚙️ = 基础设施 / 修 bug。
> **价值**：🔴 = 修 bug；🟠 = 架构升级；🟡 = 体验优化；🟢 = nice-to-have。
> **工作量**：S ≤ 4h，M = 0.5–2 天，L > 2 天。

> **建议批次**：
> - **批 A（修 bug，3.5h）** ✅#1 已完成；#2 — 实际已被单 Mutex 保护，改"修注释"。
> - **批 B（Verbatim 纪律落地，3h）** #10 + #7 + 文档化 — 把上面"设计原则"刻进代码。
> - **批 C（数据模型扩展，1 天）** #5 + #11 同期做，省一次 schema 迁移阵痛。
> - **批 D（性能/排序，1.5–2 天）** #3 + #4 + #6。
> - **批 E（生命周期 / 卫生，半天–1 天）** #8 + #9。

---

## 9. 不建议引入的 MemPalace 概念

- **Wing / Drawer 隐喻**：mem 已有等价物（`project`/`repo`/`module` 字段做 Wing，`memories` 行做 Drawer）；改名只会增加心智负担。**Room 单独例外，见 §11。**
- **AAAK 压缩索引层**：mem 已经用 token-budgeted 输出解决了相同问题（让 LLM 看到的内容受控），AAAK 是对 BM25/向量的补充，不是替代。
- **Hooks 驱动的后台归档**：mem 是 HTTP 服务模型，不绑定具体编辑器；类比物是 caller agent 主动 POST，不需要 hook。
- **L0–L3 唤醒栈**：MemPalace 的 layered context 设计是为对话开场注入服务的；mem 的客户端（Codex/Cursor 等）有自己的开场注入逻辑，不应在服务端重做。

---

## 10. 信息源

- mem 的事实来自当前仓库代码（`src/`、`db/schema/`、`Cargo.toml`、`README.md`）。
- MemPalace 的事实来自其本地 checkout（`/root/workspace/master/mempalace/`），相关文件：`CLAUDE.md`、`mempalace/backends/chroma.py`、`mempalace/searcher.py`（README 元信息）、`mempalace/knowledge_graph.py`（元信息）。
- 凡是 README 没明说但代码里有的（如 `merge_and_rank_hybrid` 的具体权重、`embedding_jobs` 状态机），按本地代码为准；本文档与代码不一致时**以代码为权威**。

> 维护建议：每完成路线图中的一项，回来更新对应行的状态（✅ done / 🚧 in progress），并在 git commit message 里引用本文档章节号（例如 `feat(ingest): switch content_hash to sha2 (closes mempalace-diff §8 #1)`）。

---

## 11. Sessions（对齐 MemPalace 的 Room，但和 episodes 正交）

### 为什么 mem 现在缺这个

mem 的时间维度只是 `created_at` / `updated_at` 时间戳，**没有"会话/工作时段"这个一等容器**。下面这些查询当前都答不出来：

- "我今天工作了什么" — 勉强（`WHERE date(created_at)=today`），但跨午夜或多 agent 并行就乱
- "上次调 invoice retry 那波我都记了什么" — 没有 session ID 可锚，只能时间范围 + repo 双过滤瞎猜
- "刚才那个搞砸的 session 全删掉" — 完全答不出来

### 和 `episodes` 的关系

不是替代，是**正交**：

| | `episodes` | `sessions` |
|---|---|---|
| 主键 | **目标**（goal） | **时间** |
| 边界 | 任务完成时收尾 | 静默超过 N 分钟自动新开 |
| 跨度 | 可跨多个时间段 | 一个连续工作时段 |

8 小时调试 = 1 episode + 2 session（中间睡了一觉）；
一上午 = 1 session + 5 个小 episode。

每条 `memories` 同时拥有 `session_id`（自动）和**可选的** `episode_id`。

### Schema

```sql
create table sessions (
  session_id text primary key,
  tenant text not null,
  caller_agent text not null,        -- "codex-cli", "cursor", "ci:job-42"
  started_at text not null,
  ended_at text,
  goal text,                         -- 自由文本，可选
  memory_count integer not null default 0
);

create index if not exists idx_sessions_agent_active
  on sessions(tenant, caller_agent, ended_at);

alter table memories  add column session_id text references sessions(session_id);
alter table episodes  add column session_id text references sessions(session_id);

create index if not exists idx_memories_session on memories(session_id);
create index if not exists idx_episodes_session on episodes(session_id);
```

### 写入路径（auto-bucket）

`memory_service::ingest` 在写 memory 之前先解析 session：

```rust
async fn resolve_session(
    repo: &DuckDbRepository,
    tenant: &str,
    caller_agent: &str,
    now: &str,
    idle_minutes: u64,             // 默认 30
) -> Result<String, StorageError> {
    if let Some(s) = repo.latest_active_session(tenant, caller_agent).await? {
        let last_activity = s.ended_at.as_deref().unwrap_or(&s.started_at);
        if minutes_since(last_activity, now) < idle_minutes {
            return Ok(s.session_id);   // 续用
        }
        repo.close_session(&s.session_id, now).await?;
    }
    repo.open_session(tenant, caller_agent, now).await
}
```

（一个 caller_agent 上一次活动 < 30 分钟 → 续用；否则关旧、开新。）

### 检索 / 公共面

- `memory_search` 加可选 `session_id` 过滤
- `GET /sessions?tenant=...&agent=...&since=...` 列表
- `GET /sessions/{id}` 返回会话 + 其下 memory_id 列表 + episode_id 列表
- `DELETE /sessions/{id}` 撤销整段记录（用 `supersedes_memory_id` 软删，不真删）

### 不做什么

- ❌ **不**自动给 episode 派生 session（episode 是 caller 主动调用的）
- ❌ **不**做"跨 caller_agent 合并 session"（每个 agent 独立桶）
- ❌ **不**让 `goal` 在写入时强制必填（不是 episode）
- ❌ **不**复用 MemPalace 的 Wing/Drawer 命名（保留 mem 现有术语）

### 风险

- **schema 迁移**：DuckDB 1.x 的 `alter table` 在大表上是全表重写。建议在低活时段执行，或通过 export/reload 路径。
- **`session_id` 历史空值**：迁移后老 memory 的 `session_id` 为 NULL，检索时按"NULL 视为独立伪 session"处理；不要回填假 session。
- **idle_minutes 怎么定**：默认 30 分钟够 90% 场景；CI/批处理可能需要更短（例如 `MEM_SESSION_IDLE_MINUTES=5`）。环境变量化。

### 工作量

M（半天）= schema 迁移 + `resolve_session` + ingest 路径接线 + 一个 `/sessions` 路由 + 单元测试。

> commit 时引用：`feat(sessions): auto-bucket memories into time-based sessions (closes mempalace-diff §8 #11)`
