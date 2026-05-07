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

1. **真正的 ANN 索引**。当前 `memory_embeddings.embedding BLOB` 是**全表扫描算 cosine**——规模上去会跪。**而且不只是慢**：`semantic_search_memories`（`src/storage/duckdb.rs:536`）SQL 里有硬编码 `limit 2000` + `order by updated_at desc`，意味着**老记忆会静默掉出语义召回窗口**——这不是单纯性能问题，是已经在悄悄发生的正确性边界。ANN 落地后这条截断一并消除。三个候选方案：
   - `usearch` crate（推荐）：单文件，支持持久化，和 DuckDB 共存；
   - `hnsw_rs` crate（纯 Rust，简单）；
   - DuckDB `vss` extension（与 bundled DuckDB 兼容性需测，PR 风险高）。
   - 选型建议：**`usearch` sidecar 索引文件 + DuckDB 行做权威源**，HNSW 损坏时可从 DuckDB 重建（对照 MemPalace 的 `mempalace repair --mode rebuild`）。
2. **HNSW 健康度自检**。MemPalace `backends/chroma.py::hnsw_capacity_status` 在每次启动比对 `sqlite_count` vs `hnsw_count`，超阈值就提示 repair。我们落 sidecar 索引后照搬这套自检 + 修复 CLI。
3. **混合检索的归一化打分**。当前 `score_candidates_hybrid`（`pipeline/retrieve.rs:149`）是加性整数，semantic_sim×64 vs scope×18 量级失衡。建议改成两路 RRF（reciprocal rank fusion）或先归一到 [0,1] 再加权——对齐 MemPalace 的 BM25 + 向量混合写法。

   **2026-04-29 复核**：仍未做。code 是 `let mut score = 0i64;` 起步加性串接 7+ 个维度（lexical∩semantic +26、evidence +2、text_match、scope -4..+18、memory_type×intent、confidence ×10、validated +3、freshness、decay ×12、graph_boost ±12、provisional -4）。唯一一处线性 rescale 是 semantic：`((cos+1)/2)*64`（[0, 64]）。**没有 RRF**，整个文件查不到 `reciprocal_rank` / `rank_fusion` 痕迹。失衡问题描述准确。

   **2026-04-29 落地**：✅ 改为两路 RRF（k=60, 缩放×1000），保留 lifecycle 加性分作为微调；新增 `score_candidates_hybrid_rrf` 与 `score_candidates_hybrid_legacy` 共存，`MEM_RANKER=legacy` 一档兜底；仅修 `pipeline/retrieve.rs`，无 schema 变更，无新依赖；4 个 RRF 单测 + 1 个 kill-switch smoke + 全套集成测试通过，clippy/fmt 净。see ROADMAP #5。

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
2. ~~**`embedding_jobs` 的 live-job dedupe 在应用层**（schema 002 注释承认 DuckDB bundled 不支持 partial unique index）——并发提交同一 memory 多个 job 会有竞态。~~ ✅ **复核后无 bug**：`try_enqueue_embedding_job` 已是 transaction + count-then-insert，且 `DuckDbRepository.conn` 是 `Arc<Mutex<Connection>>`（`src/storage/duckdb.rs:83`），整个进程内所有 DB 访问串行化，并发 caller 不可能撞上竞态。已更新 schema 注释（`db/schema/002_embeddings.sql`）和函数 doc（`try_enqueue_embedding_job`）澄清这一点，把 phantom bug 从待修列表撤掉。

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
- ✅ `compress_text` 按真实 token 截断（tiktoken-rs::o200k_base，2026-04-29 落地，ROADMAP #6）；之前按 `chars × 3` 估算，CJK 实际 ~3× 超预算的 bug 已修

### 建议

1. ✅ 在 ingest 路径加一个 `assert!(content.len() > 0)`，**禁止 `summary` 与 `content` 完全相同时写入**——这是 agent 偷懒抄过去的信号。（2026-04-29 落地：`IngestMemoryRequest.summary: Option<String>`，caller 提供时校验 `summary != content`，4 个单测 + 2 个集成测试，HTTP DTO 与 InvalidInput→400 映射也一并补上。ROADMAP #9。）
2. ✅ 把 `compress_text` 的"按词截断"换成 `tiktoken-rs` 之类的 token 计数器，对中英文都靠谱。（2026-04-29 落地：tiktoken-rs 0.11 + o200k_base，6 个单测覆盖 CJK / ASCII / 混合 / 边界。）
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

> 已抽出到独立文件，便于单独维护与外发：[`ROADMAP.MD`](./ROADMAP.MD)。
>
> 新增 / 调整路线图项请先回本文件改对应章节（§5 / §7 / §8 / §11 / §12 / §13 等），再同步更新 `ROADMAP.MD`——本文件仍是设计原则与论证的权威源，`ROADMAP.MD` 只是执行面板。

---

## 9. 不建议引入的 MemPalace 概念

- **Wing / Drawer 隐喻**：mem 已有等价物（`project`/`repo`/`module` 字段做 Wing，`memories` 行做 Drawer）；改名只会增加心智负担。**Room 单独例外，见 §11。**
- **AAAK 在服务端烧 LLM 的实现**：MemPalace 自己烧 LLM 因为它没有外部调用方；mem 是 HTTP 服务，**调用方已经有自己的 LLM**（Codex/Cursor/CI agent），服务端再烧一次是双重计算 + 双重账单。**精神可借鉴（"两段式 + LLM 精排"），但实现上 mem 应通过更丰富的候选包让 caller 自己精排，见 §12 #12。**
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

> **2026-04-29 落地**：✅ schema (`db/schema/004_sessions.sql`) + auto-bucket (`pipeline/session.rs::resolve_session`) + `memories.session_id` 落地。`MEM_SESSION_IDLE_MINUTES` 环境变量化（默认 30）。HTTP 端点（`GET /sessions` 等）、`episodes.session_id` 列、`DELETE /sessions/{id}` 软删均延后到后续 PR。详见 `docs/superpowers/specs/2026-04-29-sessions-design.md`。
>
> **本次落地的两点偏离原设计**：
> 1. `last_seen_at` 单独成列（原 §11 用 `ended_at.unwrap_or(started_at)`，对长 session 不 work）。
> 2. `memories.session_id` 没有 inline FK 约束 —— DuckDB 的 parser 不支持 `ALTER TABLE ADD COLUMN ... REFERENCES`。等价的写入序保证由 `resolve_session()` 提供（先 open_session，再 create_memory）。

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

---

## 12. 检索流水线三段式重构（取 mem 之结构化、借 MemPalace 之分层）

> **2026-04-29 落地**：✅ 按 line 513 的 "可降级为只做 Stage 2 拆分（半天）" 路径执行。`pipeline/retrieve.rs::apply_lifecycle_score` 抽出共享 helper（9 个普适信号），三个 scorer 都调它复用 lifecycle 加性层；evidence bonus 在两个 hybrid scorer 内联（`score_candidates` 非 hybrid 路径历史上没这个 bonus，保持原样）。3 个新单测 + 全套现有测试零 assertion 改动通过 —— 行为完全等价。**Stage 3 仍未做**：响应形状变更（暴露 `score_breakdown`）、`compress.rs` 解构、A/B `MEM_RANKER=three_stage` 切换均延后。详见 `docs/superpowers/specs/2026-04-29-lifecycle-score-extraction-design.md`。
>
> **触发完整 §12 重构的条件**：当 `tests/search_api.rs` 出现 P95 延迟报警 OR caller 反馈 token 截断失真 OR caller-side LLM rerank 真的接进来。

### 当前状态：两边都不全

| | mem 现状 | MemPalace 现状 |
|---|---|---|
| 召回（recall） | ❌ 全表线性扫 + cosine | ✅ HNSW + BM25 倒排 |
| 多信号融合 | ✅ 一锅加性求和 | ⚠️ RRF（无 lifecycle） |
| 生命周期感知 | ✅ decay/status/confidence/intent | ❌ 无 |
| LLM 兜底精排 | ❌ 无 | ✅ AAAK |
| 可扩展性 | ❌ O(N) | ✅ O(log N) |
| Verbatim 一致 | ⚠️ summary 权重 > content | ✅ 完全 verbatim |
| 延迟/成本 | ✅ 零 LLM 调用 | ❌ 每查询 1 次 LLM |

### 核心洞察

mem 的**生命周期信号**（`decay/status/confidence/intent`）和 MemPalace 的**分层流水线 + LLM 精排****正交、不冲突**。两边强项的并集就是最优解。

### 目标架构

```
┌─────────────────────────────────────────────────────────────┐
│ Stage 1（召回 / Recall）  sublinear，零 LLM                   │
│   BM25 + 向量（HNSW）→ top-K 候选（K=50–100）                 │
│   ↑ 借 MemPalace：HNSW + RRF 融合                             │
│   ↑ 前置：§8 #3（usearch sidecar）+ §8 #6（RRF）              │
├─────────────────────────────────────────────────────────────┤
│ Stage 2（结构化过滤 / Filter）  零 LLM                        │
│   按 mem 现有信号重排 top-K → top-N（N=5–10）                 │
│   - scope_filters / status / pending 惩罚                     │
│   - decay_score、confidence、validation                       │
│   - intent × memory_type                                      │
│   - graph_boost（图邻居）                                     │
│   ↑ 保 mem 路线：把 lifecycle 从"线性求和的一项"              │
│     改为"召回后的 rerank 过滤层"                              │
├─────────────────────────────────────────────────────────────┤
│ Stage 3（精排 / Rerank）  调用方做，服务端不烧 LLM            │
│   返回更丰富的候选包（top-N 含元数据）                        │
│   caller agent 用自己的 LLM + 当前对话上下文做最终挑选        │
│   ↑ AAAK 的精神：让 LLM 兜底；                                │
│     但**实现位置在 caller**，避免双重计算 + 双重账单          │
└─────────────────────────────────────────────────────────────┘
```

### 与现有代码的对应关系

| 阶段 | 现 in mem | 改造后 |
|---|---|---|
| Stage 1 召回 | `merge_and_rank_hybrid` 一次过 + cosine 全扫 | 拆出 `recall_candidates(query, k=100)`，调 #3 的 ANN 索引 + BM25 |
| Stage 2 过滤 | 加性求和混在一起 | 拆出 `apply_lifecycle_filters(candidates, query)`，把 decay/status/intent/graph_boost 单独算 |
| Stage 3 精排 | `compress.rs` 已有半截（token_budget 切分） | 输出层不再做粗暴截断，改为返回**结构化候选包**，让 caller 决定 |

### 输出格式调整（compress.rs）

现在 `compress.rs` 强行把候选切成 `directives / relevant_facts / reusable_patterns / suggested_workflow` 四桶 + 词截断。三段式重构后，**Stage 3 的输入应该是更丰富的元数据，让 caller LLM 自己分类**：

```jsonc
{
  "candidates": [
    {
      "memory_id": "mem_...",
      "memory_type": "implementation",
      "content": "...",                    // verbatim, 不截断
      "summary": "...",                    // index hint, 不当事实源
      "score_breakdown": {
        "recall": {"bm25": 12.3, "vector_sim": 0.82},
        "lifecycle": {"confidence": 0.9, "decay": 0.0, "validated": true},
        "context": {"scope_match": true, "intent_boost": 8, "graph_neighbor": false}
      },
      "code_refs": [...], "tags": [...], "graph_links": [...],
      "session_id": "...", "version_chain": [...]
    },
    ...
  ],
  "stage_1_recall_size": 87,
  "stage_2_filter_size": 8,
  "token_budget_hint": 400              // caller 可选择压缩到此预算
}
```

`token_budget` 从"硬截断"降级为"提示"，**caller 可选择性压缩**（compress.rs 仍可作为可选 helper 暴露给老调用方）。

### 不做什么

- ❌ **不**在服务端调 LLM（成本 + 延迟 + API 依赖）
- ❌ **不**抛弃 mem 已有的所有信号（intent / decay / scope / graph_boost 全保留，只是搬到 Stage 2）
- ❌ **不**把 `compress.rs` 的旧契约一刀切（保留 `/memories/search` 旧响应形状作为 deprecated 兼容路径，至少一个 minor 版本）
- ❌ **不**强制 caller 用 LLM 精排（caller 可只用 Stage 1+2 结果直接答；Stage 3 是 opt-in）

### 风险

1. **行为漂移**：Stage 2 用 rerank 替代加性求和，**排序可能与现在不同**。需要：
   - 保留旧 `merge_and_rank_hybrid` 实现作为 `legacy_rank` 路径，加 `MEM_RANKER=legacy` 环境变量做 A/B 切换
   - 跑 `tests/search_api.rs` 的所有现有用例，确认 top-1 不退化（允许 top-5 内顺序变动）
2. **召回参数调优**：K=50–100 的选择直接影响 Stage 2 输入质量。先用 100 作为保守起点，加指标观察 Stage 1→Stage 2 的过滤率
3. **响应体变大**：Stage 3 返回更多元数据，`/memories/search` 响应体可能从几 KB 涨到几十 KB。加 `?compact=1` 兼容老 caller
4. **依赖前置**：#3（HNSW）和 #6（RRF）必须先做，否则 Stage 1 还是线性扫，三段式没意义

### 工作量

M（1 天，**前提是 #3 + #6 已完成**）：

- 4h：拆 `merge_and_rank_hybrid` → `recall_candidates` + `apply_lifecycle_filters`
- 2h：响应格式扩展 + `compact=1` 兼容
- 2h：A/B 切换（`MEM_RANKER=legacy|three_stage`） + 测试

### 决策点（later choose）

加这个 #12 是**记录设计意图**，不是承诺立刻做。建议：

- **必做时机**：当数据量增长到 `tests/search_api.rs` 出现 P95 延迟报警，或 caller 反馈"压缩输出失真"时
- **可跳条件**：mem 永远小规模（<1000 memories/tenant）+ 调用方满意现在的输出 → 不需要重构
- **联动**：批 D 走完 #3/#4/#6 后再评估；如果 #3 落地后召回质量已经够好，#12 可降级为只做 Stage 2 拆分（半天）

> commit 时引用：`refactor(retrieve): split into recall + filter + rerank stages (closes mempalace-diff §8 #12)`

---

## 13. Claude Code / Codex 无感集成包（让"用户毫无察觉"成为产品特性）

### 现状对比

MemPalace 在 Claude Code 里实现了真正的"无感"：用户写消息 → AI 自然回答 → 后台无声 mine → 下次会话开始 AI 已经"记得"。这个体验**不是来自 mempalace Python 包本身**，而是来自三类 Claude Code/Codex hook 脚本 + 一个离线 miner + 一个 wake-up CLI 的组合。

mem 当前依赖 SKILL.md 提示 caller agent 主动调 `memory_search` / `memory_ingest`——能跑但**不无感**：依赖 prompt 遵守，且 agent 的 CoT 会泄漏 "I'll save this to mem"。

### 三个组件

#### 1. Hook 脚本（`hooks/mem_save_hook.sh` 等）

**Stop hook** — 每 N 轮触发一次背景 mine：
```bash
# 简化版逻辑（仿 mempal_save_hook.sh）
INPUT=$(cat)                          # Claude Code 传入 {session_id, transcript_path, ...}
EXCHANGE_COUNT=$(...)                 # 从 transcript JSONL 数 user 消息
if [ $((EXCHANGE_COUNT - LAST_SAVE)) -ge 15 ]; then
    mem mine "$TRANSCRIPT_PATH" --mode convo &   # 后台 fork，立即返回
    echo "$EXCHANGE_COUNT" > "$LAST_SAVE_FILE"
fi
echo '{}'                              # 空 JSON → AI 自然 stop，用户无感
```

**PreCompact hook** — 上下文压缩前最后一次 mine：
```bash
mem mine "$TRANSCRIPT_PATH" --mode convo --include-pending &
echo '{}'
```

**SessionStart hook** — 会话开始注入 wake-up：
```bash
WAKEUP=$(mem wake-up --tenant local --token-budget 800)
cat <<EOF
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"$WAKEUP"}}
EOF
```

三个脚本都跨平台（bash + Python 解析 stdin）+ 跨 agent runtime（Claude Code `.claude/settings.local.json` / Codex `.codex/hooks.json` 安装路径都覆盖）。

#### 2. `mem mine` 离线 transcript miner

**新子命令**（`src/cli/mine.rs`）：

```bash
mem mine <transcript-path-or-dir> [--mode convo|projects] \
         [--tenant local] [--since <iso8601>] [--include-pending]
```

**convo 模式**：
- 读 Claude Code / Codex 的 JSONL transcript
- 按消息切分：user 消息 → 不入库（用户的话归用户）；assistant 消息中**显式标记保存的事实**（"I'll remember:" 模式或 `<mem-save>` 标签）→ POST 到 `/memories`
- 多步任务完成 → POST 到 `/episodes`
- 每条记录带上 `session_id`（来自 transcript 的 session_id 字段，对齐 #11）

**projects 模式**：
- 扫描代码/笔记目录，提取标题/decision 类的标记（README、CHANGELOG、`docs/decisions/*.md`）
- POST 到 `/memories` 作为 `memory_type=implementation`

**核心设计选择**：
- ❌ **不**自动保存 user 消息原文（违反 verbatim 边界——user 的话在 transcript 里就是原始事实）
- ✅ **只**保存 assistant 显式承诺保存的内容（`<mem-save>...</mem-save>` 标签或 SKILL.md 教 agent 用的模式）
- ✅ **idempotency_key** 用 `transcript_path:line_no` —— 同一段转录跑多次 mine 不重复入库

#### 3. `mem wake-up` 子命令

**新子命令**（`src/cli/wake_up.rs`）：

```bash
mem wake-up [--tenant local] [--token-budget 800] [--session-id <last>]
```

**输出**（注入到 SessionStart hook 的 `additionalContext`）：
- **L0**（~100 tokens）：从 `~/.mem/identity.txt` 读"用户身份"
- **L1**（~700 tokens）：调内部 `memory_search` 拿 top-K facts/preferences/patterns，按 #12 三段式格式压缩
- 总长度受 `token_budget` 硬约束

**关键差异 vs 已有 `memory_bootstrap` MCP tool**：
- `memory_bootstrap` 是 MCP 工具，**caller agent 必须主动调**——SKILL.md 提示但不强制
- `mem wake-up` 是 CLI，**SessionStart hook 自动调**——绕过 caller，**真无感**

### 与已有组件的关系

| 已有 | 升级为 |
|---|---|
| `memory_bootstrap` MCP tool | 保留（caller 主动调）；`mem wake-up` CLI 是 hook-driven 的封装 |
| `memory_ingest` MCP tool | 保留（caller 主动调）；`mem mine` 是 transcript-driven 的封装，背后还是同一 HTTP API |
| SKILL.md "autopilot workflow" | 保留作为 prompt-level 指引；hooks 是 enforcement-level 兜底 |

**没有任何旧能力被替换**——hooks 是叠加层，prompt-level 调用仍然有效（只是不再是唯一保险）。

### 不做什么

- ❌ **不**截获 user 原话保存（user 的话留在 transcript，不进 memory）
- ❌ **不**做"实时 streaming mine"（hook 触发的 batch mine 已经够用，streaming 增加复杂度）
- ❌ **不**绑定单一 agent runtime（hook 脚本自己适配 Claude Code + Codex；其他 runtime 加适配文档即可）
- ❌ **不**强制启用（hook 注册仍然是 opt-in，README 给安装说明，用户不装也能跑）

### 风险

1. **跨平台 shell 兼容**：bash/zsh、macOS/Linux/Windows-WSL 都得测。**MemPalace 的脚本里有大量 macOS GUI launch 兜底逻辑（`MEMPAL_PYTHON` 环境变量绕开 PATH 问题），mem 抄一份**
2. **transcript 格式漂移**：Claude Code 和 Codex 的 JSONL schema 不同，且各自版本会演进。`mem mine --mode convo` 必须有 schema 探测 + 容错，遇到不认识的字段跳过不报错
3. **`mem wake-up` 的 token_budget 漂移**：注入到 SessionStart 后 caller LLM 的 context 减少了 ~800 tokens；要监控 caller 抱怨"context 不够用"的情况，提供 `MEM_WAKEUP_DISABLE=1` 一键关闭
4. **与 #11 (Sessions) 的依赖**：wake-up 的 L1 essential story 应该按 "上一个 session" 加权——否则 100 个会话的 fact 一锅捞出，user 体验是"AI 记得乱七八糟的东西"。**不做完 #11 不要做 #13**

### 工作量

M（1.5 天，前提是 #11 完成）：

- 4h：`mem mine --mode convo` 实现（解析 Claude Code JSONL + 提取 `<mem-save>` 标签）
- 2h：`mem mine --mode projects` 实现（扫描 docs/ 目录提 decision 类标记）
- 2h：`mem wake-up` 实现（L0 读文件 + L1 调内部 search + 压缩到 budget）
- 3h：三个 hook 脚本（Stop/PreCompact/SessionStart）+ 跨平台测试
- 1h：README 集成章节 + 注册到 `.claude/settings.local.json` / `.codex/hooks.json` 的步骤指引

### 决策点（later choose）

加这个 #13 不是承诺立刻做。建议：

- **必做时机**：mem 第一个真实用户问"为什么我每次开会话都得手动调用 memory_search"
- **可跳条件**：mem 永远只服务程序化 caller（CI/批处理），没有交互式 agent 用户 → 不需要"无感"
- **联动**：必须先做完 #11；如果 #11 推迟，#13 也跟着推迟

> commit 时引用：`feat(integration): add Claude Code / Codex hook bundle and offline miner (closes mempalace-diff §8 #13)`

---

## 14. Conversation Archive（verbatim transcript 全量归档，与 memories 管道完全隔离）

> **2026-04-30 落地**：✅ 在 §13 的 `mem mine` 之上加一条**全量原始对话归档**管道。新表 `conversation_messages`（每个 transcript block 一行，verbatim）+ 独立队列 `transcript_embedding_jobs` + 独立 HNSW sidecar `<MEM_DB_PATH>.transcripts.usearch`；与 `memories` 表 / 嵌入队列 / sidecar **完全不共享**任何状态或向量空间。`mem mine` 改为 dual-sink，单次扫描既写既有 memories 路径也写新 archive；`mem repair --check|--rebuild` 同时覆盖两个 sidecar。HTTP 路由 `POST /transcripts/messages` / `POST /transcripts/search` / `GET /transcripts?session_id=…&tenant=…`。**MCP 表面零变化**——transcript 搜索仅 HTTP，agent 走 `memory_search` → 命中后用 `session_id` 拉对应 transcript。详见 spec [`docs/superpowers/specs/2026-04-30-conversation-archive-design.md`](./superpowers/specs/2026-04-30-conversation-archive-design.md) 与 plan [`docs/superpowers/plans/2026-04-30-conversation-archive.md`](./superpowers/plans/2026-04-30-conversation-archive.md)。

### 与既有路线的关系

§7 verbatim 纪律的自然延伸：`memories.content` 守护事实级原文，`conversation_messages.content` 守护对话级原文。两条管道共用一个 `session_id` 锚点（§11 Sessions），但 ranking / lifecycle / compress / verbatim guard 全部不动。新增 env：`MEM_TRANSCRIPT_EMBED_DISABLED=1`（停 transcript embedding worker，避免 OpenAI 用户成本翻倍）、`MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY`（默认 256，低于 memories sidecar 的 1024，因为单 session 写入 burst 更大）。

> commit 时引用：`feat(transcripts): add conversation_messages archive pipeline alongside memories (closes mempalace-diff §14)`

---

## 15. 当前差异快照（2026-05-06）

> ROADMAP 上 #1–#15 已全部 ✅，能借鉴的项基本吸收完。本节是给"半年后再看"的人做一个快速定位：剩下的差异是哪些、属于哪个层级、为什么不再缩小。
>
> 维护建议：每完成新的对齐项 / 新增本质差异时，更新本节的"已吸收"或"mem 独有"表，避免再读 ROADMAP 才知道边界。

### 15.1 本质形态差异（不会消除）

| 维度 | mem | MemPalace | 论证位置 |
|---|---|---|---|
| 服务模型 | 长驻 HTTP（axum :3000）+ 多 agent 共享 | MCP server（stdio）+ CLI，单用户单机 | §0 |
| 存储引擎 | DuckDB 单文件 + usearch HNSW sidecar + 内置 `graph_edges` 表 | ChromaDB（SQLite + HNSW 段）+ 独立 SQLite KG | §0 / §2 |
| 语言/栈 | Rust + axum + tokio + DuckDB-rs | Python + ChromaDB + SQLite | §0 |
| 核心哲学 | 结构化记忆**生命周期**（status / supersedes / feedback / decay） | **Verbatim** 不改写、只压缩索引层 | §0 / §6 / §7 |
| 输出形态 | token-budgeted 四段式（directives / facts / patterns / workflow） | 返回 drawer 列表，让 caller LLM 自己整合 | §3 / §12 |

> 两条哲学路线 mem 已经做了**双轴叠加**：📦 存储层吸收 verbatim 纪律（§7 / §14），🔍 索引/排序/lifecycle 保留 mem 路线（§6 / §12）。判定一项改造能不能做仍按 §8 的"会改写或丢失原话 = 禁止；只改排序/打分/元数据 = 允许"分流。

### 15.2 已吸收的 MemPalace 设计

| 来源（MemPalace 概念） | mem 落地 | ROADMAP # | 章节 |
|---|---|---|---|
| 时序图 `valid_from` / `valid_to` | `graph_edges` 表内置 | #4 | §5 |
| HNSW ANN（消除全表扫 + `limit 2000` 隐式截断）| usearch sidecar | #2 | §3 / §4 |
| HNSW 健康自检 + repair CLI | `mem repair --check / --rebuild` | #3 | §3 |
| BM25 + 向量 RRF | `pipeline/retrieve.rs` 两路 RRF（k=60） | #5 | §3 |
| Entity registry（人/项目消歧） | `entities` + `entity_aliases` 表 | #8 | §2 |
| Verbatim 纪律（token 计数 + summary 守护）| tiktoken-rs / ingest 校验 `summary != content` | #6 / #9 | §7 |
| Sessions（≈ Room，与 episodes 正交） | `sessions` 表 + auto-bucket on ingest | #10 | §11 |
| Hook + miner + wake-up "无感"集成 | Stop/PreCompact/SessionStart hooks + `mem mine` + `mem wake-up` | #12 | §13 |
| Verbatim 对话归档 | `conversation_messages` 独立管道（与 memories 完全隔离） | — | §14 |
| LongMemEval 横向对标 | Rust port of `longmemeval_bench.py`，3-rung 输出对齐 mempalace `raw / rooms / full` | #15 | — |

### 15.3 mem 独有（MemPalace 没有）

- **`embedding_jobs` 持久化队列** + 后台 worker（含 batch 模式 `EMBEDDING_BATCH_SIZE`）+ attempt 重试 + content_hash 失效检测——比 MemPalace 内联 embed 更工业化（§4）
- **`MemoryStatus` 三态**（Provisional / Active / PendingConfirmation）+ `WriteMode::Confirm` 评审流（§6）
- **`feedback_events`** 调权 `confidence` / `decay_score`（§6）
- **`decay_worker`** 后台时间衰减（ROADMAP #7）
- **`fts_worker`** 后台 BM25 rebuild（`MEM_FTS_REBUILD_INTERVAL_MS`，把 FTS 重建移出 search 读路径）
- **加性 lifecycle 重排层**（intent × memory_type / scope / freshness / graph_boost）—— RRF 融合后又叠了一层 mem 特色信号（§3 / §12）
- **Recall quality bench**（10-rung ablation；ROADMAP #14）

### 15.4 已论证不引入的 MemPalace 概念

详见 §9，简列：

- **Wing / Drawer 隐喻**：mem 已有等价物（`project` / `repo` / `module` 字段做 Wing，`memories` 行做 Drawer）；改名只增加心智负担。**Room 例外，已落地为 §11 Sessions**。
- **AAAK 服务端烧 LLM**：mem 是 HTTP 服务，caller 已有自己的 LLM；服务端再烧 = 双重计算 + 双重账单。精神已通过"返回更丰富候选包让 caller 自精排"吸收（§12 Stage 3）。
- **L0–L3 唤醒栈**：caller 各自有开场注入逻辑；服务端通过 `mem wake-up` 注入 SessionStart，**让 caller 决定怎么用**而非在服务端预定义层级（§13）。

### 15.5 一句话总结

> mempalace-diff 路线图（#1–#15）已全部 ✅。剩下的差异是**有意保留的形态/哲学差异**——服务模型（HTTP vs MCP）、存储栈（Rust+DuckDB vs Python+Chroma）、哲学侧重（lifecycle 治理 vs verbatim 档案），不是落后或缺口。新对齐项请先回 §5 / §7 / §11 / §12 / §13 / §14 等加章节，再同步更新本节 15.2/15.3/15.4 表格 + ROADMAP.MD。
