# mem 整体架构

> **本文档定位**:mem 的顶层架构总览——"从这里开始"的地图。它把散落在 `docs/` 里的深潜文档串成一张全景图,讲清**模块边界**、**数据如何流动**、**为什么是现在这个样子**。需要某个子系统的细节(schema、逐接口、逐算法)时,顺着正文里的链接下钻到对应深潜文档。
>
> **读者**:给 mem 贡献代码 / 排查问题 / 做设计评审的人。假设你能读 Rust,但不假设你读过任何一个源文件。
>
> **对齐基线**:`master` @ `c32f4bf`(2026-07-15)。架构描述以代码当前状态为准;个别源文件里的注释可能落后于代码(正文会标出),此时以本文和代码实际行为为准。

---

## 目录

1. [关于本文档:与既有 docs 的关系](#1-关于本文档与既有-docs-的关系)
2. [一览:mem 是什么](#2-一览mem-是什么)
3. [进程形态与入口](#3-进程形态与入口)
4. [分层架构总览](#4-分层架构总览)
5. [领域模型与生命周期](#5-领域模型与生命周期)
6. [存储层](#6-存储层)
7. [双管道:capsule 与 transcript](#7-双管道capsule-与-transcript)
8. [Capsule 四阶段管道](#8-capsule-四阶段管道)
9. [Embedding 子系统](#9-embedding-子系统)
10. [后台 worker 全景](#10-后台-worker-全景)
11. [知识图谱与实体](#11-知识图谱与实体)
12. [接口面](#12-接口面httpmcpcli钩子)
13. [横切关注点](#13-横切关注点)
14. [关键设计约束与不变量](#14-关键设计约束与不变量)
15. [附录](#15-附录)

---

## 1. 关于本文档:与既有 docs 的关系

`docs/` 下已有一批**深潜文档**,每篇讲透一个子系统。本文不重复它们,而是给出全景并链接下钻。索引:

| 文档 | 讲什么 | 什么时候读 |
|------|--------|-----------|
| `docs/architecture.md`(本文) | 顶层全景、模块边界、数据流 | 入门、评审前先看这篇 |
| [`docs/database-schema.md`](database-schema.md) | 每张 Lance 表的字段、类型、约束 | 改 schema / record_batch 之前 |
| [`docs/api-data-flow.md`](api-data-flow.md) | HTTP 接口逐个 + 请求在管道里的流动 | 加/改接口、追一个请求 |
| [`docs/mcp-capabilities.md`](mcp-capabilities.md) | MCP 工具面、每个工具的参数与语义 | 改 MCP 工具、接 agent |
| [`docs/backend-coupling.md`](backend-coupling.md) | 存储后端抽象、sub-trait 拆分、耦合治理 | 动存储抽象、加后端 |
| [`docs/remove-duckdb-keep-lance.md`](remove-duckdb-keep-lance.md) | route-B 迁移记录:删 DuckDB 读引擎、留 Lance | 想懂"为什么读路径是 lance-native / FTS 是 Tantivy" |
| [`docs/postgres-backend.md`](postgres-backend.md) / [`docs/clickhouse-backend.md`](clickhouse-backend.md) | PG / CH 后端的接线、schema、状态 | 用非 Lance 后端 |
| [`docs/evolution-worker.md`](evolution-worker.md) | 记忆演化引擎(merge/refine/split/generalize) | 动演化线 |
| [`docs/offline-reranker-lane.md`](offline-reranker-lane.md) | I2 离线 cross-encoder 重排闸 | 动 merge 防御 / reranker |
| [`docs/long-content-recall.md`](long-content-recall.md) | 长内容分块与召回 | 动 chunk / 长胶囊召回 |
| [`docs/oss-memory-diff.md`](oss-memory-diff.md) | 与主流 OSS 记忆系统的对照线 + 路线 | 设计方向、对标 |
| [`docs/ROADMAP.MD`](ROADMAP.MD) | 路线图(O/K/H/G/I 系列编号项) | 找"某个 feature 的编号与动机" |
| [`AGENTS.md`](../AGENTS.md)(= `CLAUDE.md`) | 给 agent 的操作手册:命令、env 变量、约定 | 日常开发、查 env 默认值 |
| [`CHANGELOG.md`](../CHANGELOG.md) | 逐 feature 的历史"为什么" | 考古"这块当初为什么这么改" |

**约定**:本文谈到 env 变量默认值/阈值时以 `AGENTS.md` 为权威来源;谈到字段/接口时以 `database-schema.md` / `api-data-flow.md` 为权威来源。刻意不在此复制它们,避免文档漂移。

**两个阅读提示**(先说,免得后文被绊住):

- **路线图代号**:正文里形如 `O2` / `O7a` / `G4` / `H4` / `I2` / `K9` / `K10` / `P3` 的短代号,是 [`ROADMAP.MD`](ROADMAP.MD) 的路线图**系列编号**(O=召回优化、K=知识图谱、H/G=图/治理、I=索引/重排、P=后端移植分阶段)。它们只是溯源锚点,**不影响理解、可以跳过**;想知道某个 feature 的完整动机时再拿代号去 `ROADMAP.MD` / `CHANGELOG.md` 查。
- **route-B**:指 2026-06-24 那次存储迁移——删掉 DuckDB 读引擎、读写全转 lance-native、全文检索转 Tantivy。第一次出现在 §1,完整解释在 §6.3,速查见 §15.2 术语表。

---

## 2. 一览:mem 是什么

**mem 是一个 local-first、单二进制的 Rust 记忆服务,给多 agent 工作流用**。它不是一个 KV 存储,而是一套"记忆有生命周期"的系统:写进去的事实会被打分、衰减、演化、归档,检索质量随反馈信号复利式提升。

四条贯穿全局的核心理念:

- **Local-first / 单二进制**:一个 `mem` 可执行文件,默认把数据存在本机的 Lance 数据集目录里。没有必须的外部服务依赖(PG/CH 是可选后端)。嵌入推理默认也在本机跑(`embed_anything`)。
- **记忆有生命周期,而非 CRUD**:每条记忆(capsule)有状态机(`Provisional → Active → PendingConfirmation`,以及经 `incorrect` 反馈进入 `Archived`)、版本链(`supersedes_memory_id`)、置信度与衰减分。检索、反馈、后台 worker 共同推动这些状态演进。
- **Verbatim 存储,索引/排序另算**:`memories.content` 是**事实源**,存储层永不改写、永不截断。所有压缩、摘要、redaction 只发生在**输出/索引层**,不回写存储。这是贯穿全系统的第一不变量(见 §14)。
- **反馈闭环是契约**:调用方读了并用了某条记忆,就该回传信号;生命周期只在有信号时才推进。没有反馈,排序质量就冻结在写入时刻。

一张总体分层图:

```
                 ┌──────────────────────────────────────────────┐
   agent / CLI ──┤  接口面:HTTP(axum) · MCP(stdio JSON-RPC) · CLI 子命令 · Claude Code hooks │
                 └───────────────┬──────────────────────────────┘
                                 │
                 ┌───────────────▼──────────────┐
                 │  Service 门面(请求内同步执行)   │  CapabilityCapsule · Transcript · Entity · FactCheck
                 └───────────────┬──────────────┘
                                 │
                 ┌───────────────▼──────────────┐
                 │  Pipeline(行为的核心)          │  ingest → retrieve → compress → workflow
                 └───────────────┬──────────────┘
                                 │
                 ┌───────────────▼──────────────┐        ┌────────────────────────────┐
                 │  Storage:Backend(11 sub-trait) │◄──────┤ 后台 worker(各自 tokio 任务) │
                 │  Lance(默认) / Postgres / ClickHouse │      │ embedding · decay · vacuum · │
                 └───────────────┬──────────────┘        │ evolution · auto_promote · … │
                                 │                        └────────────────────────────┘
                 ┌───────────────▼──────────────┐
                 │  磁盘:Lance 数据集 + Tantivy(内存 BM25) │
                 └──────────────────────────────┘
```

要点:**worker 与 service 是旁路关系**——service 在请求 future 内同步跑完,worker 在自己的 tokio 任务里永远循环,两者都持 `Arc<dyn Backend>`,通过存储层(而非彼此)通信。

---

## 3. 进程形态与入口

### 3.1 单二进制 + 子命令

入口 `src/main.rs`。二进制名 `mem`(crate version 0.2.4,edition 2021),默认子命令是 `serve`。子命令一览:

| 子命令 | 作用 | 长驻? |
|--------|------|-------|
| `serve`(默认) | 跑 HTTP 记忆服务(axum,默认 `127.0.0.1:3000`) | 是 |
| `mcp` | 跑 MCP stdio 服务,JSON-RPC 转发到 `MEM_BASE_URL` | 是 |
| `init` | 脚手架一个新 `.mem/` 目录(env 默认 + taxonomy 起步文件),首跑 UX | 否 |
| `mine` | 从 Claude Code transcript 挖记忆 + 归档每个 block(双 sink) | 否 |
| `import <source>` | 批量归档某 agent 的会话到 transcript 档案(只归档,不抽记忆);如 `import claude-code` 走 `~/.claude/projects` | 否 |
| `hook <event>` | Claude Code 钩子入口:读 stdin 的 JSON payload,打印钩子输出信封;**永远 exit 0**(钩子不能阻塞用户) | 否 |
| `wake-up` | 查询并格式化记忆用于会话启动注入(token 预算内的高置信度记忆) | 否 |
| `feedback-from-transcript` | 扫 transcript,对被后续 assistant block 引用的记忆 POST `applies_here` 反馈(接在 Stop/PreCompact 钩子里) | 否 |
| `sync` | 把所有数据域从一个后端逐字复制到另一个(Lance/PG/CH 任意↔任意),目标端重建 embedding | 否 |

一次性子命令(`init`/`mine`/`import`/`hook`/`wake-up`/`feedback-from-transcript`/`sync`)的 handler 返回 `i32` 进程退出码,`main.rs` 分发后 `std::process::exit`。所有子命令共享 `Config::from_env`。

### 3.2 运行时与内存

`main.rs` 里两个刻意的选择,都是在 96 核大机器上被 RSS 问题逼出来的(详见源码注释):

- **jemalloc 作全局分配器**(`tikv_jemallocator`),并在启动时打开 `background_thread` 让空闲内存按定时归还 OS。glibc 的 per-thread arena 会把 RSS 往上棘轮(数天从 5→8GB)且不归还;嵌入推理(Candle/gemm)是主要的分配 churn。
- **手工建 tokio 运行时**(不用 `#[tokio::main]`)以便 `max_blocking_threads(32)`。默认 512 太大——本地嵌入推理是 blocking 调用,`spawn_blocking` 负载会把 blocking 池顶到天花板(曾见 500–800 个 `tokio-rt-worker` 线程、~11GB RSS)。32 对这个推理负载足够。

### 3.3 `serve` vs `mcp` 拓扑

关键:**MCP 不直接读数据集**。它是一个 JSON-RPC 转发器,把 agent 的工具调用翻译成 HTTP 请求打到 `MEM_BASE_URL`(默认 `http://127.0.0.1:3000`)指向的 `mem serve`。

```
   Claude / agent ──stdio JSON-RPC──►  mem mcp  ──HTTP──►  mem serve  ──►  Lance 数据集
                                     (转发器,无状态)      (唯一 writer)
```

含义:一个数据集目录只能有**一个 `mem serve`**;两个 serve 指同一 Lance 目录会抢写(见 §6.5 single-writer 锁)。MCP 侧无状态,可多开。MCP 用 stdout 做 JSON-RPC 帧,所以它的日志强制走 stderr(`init_tracing(stdio_protocol=true)`)。MCP 实现在 `src/mcp/`(`server` / `client` / `config` / `result`);工具面文档见 [`mcp-capabilities.md`](mcp-capabilities.md)。

---

## 4. 分层架构总览

`src/lib.rs` 暴露的顶层模块即分层:

```
接口面   http/         axum 路由(12 个子路由 merge)+ logging 中间件
         mcp/          MCP stdio 转发器
         cli/          子命令 handler(serve/mcp/mine/hook/wake_up/feedback/import/sync/init)

门面     service/      4 个 service:CapabilityCapsule · Transcript · Entity · FactCheck
                       —— 在请求 future 内同步执行,编排 pipeline + storage

核心     pipeline/     行为的心脏:ingest → retrieve → compress → workflow(+ chunk/ranking/redact/…)
         evolution/    记忆演化引擎(map/synthesis),被 evolution_worker 驱动
         rerank/       I2 离线 cross-encoder 重排(candle Qwen3 / fake)

数据     domain/       领域类型:capability_capsule / conversation_message / entity / episode / …
         storage/      Backend(11 sub-trait)+ 三后端实现 + Tantivy FTS + 类型
         embedding/    嵌入 provider trait(embed_anything / openai / fake)

旁路     worker/       12 个后台 worker,各自 tokio 任务
         metrics/      进程级原子计数器单例(GET /metrics)
         config/       Config::from_env
         error/        统一错误 → HTTP 状态码映射
```

### 4.1 装配点:`AppState::from_config`

`src/app.rs` 是唯一装配点。`AppState::from_config(config)` 做四件事,顺序固定:

1. **建嵌入 provider**(写时 auto-embed + 查询时 query embedding 都要用)。若 provider 会把内容发出本机(hosted),启动时**大声 warn**(隐私守卫,`MEM_PRIVACY_WARN_SUPPRESS=1` 静音)。
2. **按 `config.backend` 开存储句柄**(`match` 三个 arm,见 §6.1),产出统一的三元组 `(store: Arc<dyn Backend>, edge_access_tx, capsule_used_tx)`——`match` 之下的一切代码 backend-agnostic。
3. **spawn 后台 worker**(见 §10)。
4. **建 4 个 service**,把 store、provider、各种 channel sender 注入进去,组装成 `AppState`。

`AppState` 是 `Clone` 的(内部都是 `Arc`),axum 用它作 router state。

### 4.2 一次请求的流动(以 capsule search 为例)

```
POST /capability_capsules/search
  → http::capability_capsule handler
  → CapabilityCapsuleService::search
       ├─ query embedding(provider)
       ├─ Backend::search_candidates  ── Lance ANN + Tantivy BM25 候选
       ├─ pipeline::retrieve          ── 加性打分 + RRF 融合 + 多样性 cap + graph boost
       ├─ pipeline::compress          ── token 预算内四段输出 + redaction
       └─ 为每条命中 emit「capsule-used」事件 → last_used_worker(异步,不阻塞读)
  → JSON 响应
```

逐接口的数据流见 [`api-data-flow.md`](api-data-flow.md)。

---

## 5. 领域模型与生命周期

领域类型在 `src/domain/`。核心是 **capsule**(记忆胶囊),外加几条平行档案。

> **命名对照(先看,避免 grep 踩坑)**:领域类型叫 `capability_capsule`(旧名 `memory`,本文两词互换),存储表叫 `capability_capsules`,列是 `capability_capsules.content`——本文和部分源码里出现的 `memories.content` 指的是**同一列**(历史写法)。图边:领域侧类型在 `domain/edge_dynamics.rs` 等,存储表叫 `graph_edges`。以表名为准。

### 5.1 Capsule 类型与状态机

一条记忆是一个 capsule。它有**类型**(决定写入时的初始状态与审查策略)也有**状态**(生命周期位置)。

**6 种类型**(`domain/capability_capsule.rs`):`Preference` / `Workflow`(合称 **guidance**,召回时受保护)、`Implementation` / `Experience` / `Episode` / `Diary`。

**3 个存活状态 + 1 个终态**:`Active`(正常可召回) · `PendingConfirmation`(待审,不进活跃池) · `Provisional`(**遗留**状态,见下) · `Archived`(终态,等同删除)。

**写入时的初始状态**由 `pipeline/ingest.rs::initial_status(type, write_mode)` 按下表路由——注意它**只产出 `Active` 或 `PendingConfirmation`**:

| 类型 \ write_mode | `Auto` | `Propose` |
|-------------------|--------|-----------|
| `Preference` / `Workflow`(guidance) | PendingConfirmation | PendingConfirmation |
| `Implementation`/`Experience`/`Episode`/`Diary` | **Active** | PendingConfirmation |

生命周期迁移:

```
   ingest                    ┌──────────────────────┐   auto_promote(空闲够久)
   ──────► guidance / propose │  PendingConfirmation │ ──────────────────────┐
                             └──────────────────────┘                       │
   ingest                                                                    ▼
   ──────► 非 guidance + Auto ──────────────────────────────────────►  ┌──────────┐
                                                                        │  Active  │ ← 正常可召回
   review_accept ────────────────────────────────────────────────────► └────┬─────┘
                                                                             │ feedback: incorrect
                                                                             ▼
                                                                        ┌──────────┐
                                                                        │ Archived │ ← 永久,等同删除
                                                                        └──────────┘
```

- **审查门**:非 `Active` 的写入(guidance 类型 / `Propose` 模式 / near-dup 疑似重复 / 启发式或 LLM 抽取)都落 `PendingConfirmation`,走人工审查(`review_accept`)或自动晋升(`auto_promote_worker`),**绝不直接 Active**——这是防止低质记忆污染召回的闸。
- **`Provisional` 是遗留状态**:主 ingest 入口**不再铸造**它(旧矩阵曾把 `(非 guidance, Propose)` 路由到 `Provisional`,导致这些行低置信度地溜进活跃池却不挂审查钩子,对 `list_pending_review` 隐形)。它仍是管道理解的合法状态(`retrieve.rs` 对它的打分与 `PendingConfirmation` 相同),只是不再从这个入口产生。老数据里可能还有。
- **版本链**:`supersedes_memory_id` 把新版指向被取代的旧版,形成链。检索时做**版本链去重**(只保留链头),在 Rust 里算(`pipeline/retrieve.rs`)。
- **打分字段**:`confidence`(置信度)、`decay_score`(衰减分)、`last_used_at`(上次被召回用到的时间,锚定衰减时钟)。

改动任何碰记忆的代码,必须尊重状态转移——见 `domain/capability_capsule.rs` + `pipeline/ingest.rs::initial_status`。

### 5.2 反馈事件与生命周期推进

`feedback_events` 表记录每次反馈,并即时改写底层记录的 `confidence` / `decay_score` / `status`。五种 `FeedbackKind`:

| `feedback_kind` | confidence Δ | decay Δ | 副作用 | 何时发 |
|-----------------|------|------|--------|--------|
| `useful` | +0.10 | 0 | 标记 validated | 记忆**直接**解决了当前任务(最强正向) |
| `applies_here` | +0.05 | 0 | — | 相关上下文但非承重事实(温和正向) |
| `outdated` | 0 | +0.20 | — | 写入时对、现在过时(改名/回滚/过期凭据) |
| `does_not_apply_here` | 0 | +0.10 | — | 别处对、此 scope 不适用(降权,不归档) |
| `incorrect` | 0 | 0 | **status → Archived** | 事实错误(破坏性,永久) |

无离线批处理——反馈写入即时可见于下一次检索。这就是"反馈闭环是契约"的机制根:排序 = 语义+词法+scope+intent+confidence+freshness−decay+graph 的整数加权和,反馈动其中两项,下次召回同一记忆时名次就变了。详见 §8.2 与 §12.5。

### 5.3 平行档案

除 capsule 外,还有几条平行的数据域,各有独立类型:

- **Transcript 档案**(`conversation_message`):逐字会话归档,与 memories **零共享状态**的平行管道(见 §7)。
- **Episode**(`episode`):事件序列,workflow 抽取的原料(`pipeline/workflow.rs`:episode → workflow)。
- **Entity**(`entity`):实体注册表,把别名字符串规范化到稳定 `entity_id`(UUIDv7),见 §11。
- **Graph edge**(存储表 `graph_edges`):带有效期(`valid_from`/`valid_to`)的知识图谱边,见 §11。
- **Workflow**(`workflow`):从 episode 泛化出的可复用流程。

schema 细节见 [`database-schema.md`](database-schema.md)。

---

## 6. 存储层

存储层(`src/storage/`)是全系统最大的一块(~24k LOC)。核心抽象是 **`Backend` 伞 trait**,三个实现,一个默认(Lance,route-B 之后是 lance-native 读写)。

### 6.1 Backend 伞 trait 与 11 个 sub-trait

`src/storage/backend.rs`:`Backend` 把 11 个存储 sub-trait 聚合成一个 supertrait,加空白 impl(blanket impl)。service/worker 持一个 `Arc<dyn Backend>` 而非 11 个 `Arc<dyn XxxStore>`。

```rust
pub trait Backend:
    CapsuleStore + CapsuleSearchStore + EmbeddingJobStore + EmbeddingVectorStore
    + GraphStore + TranscriptStore + EntityRegistry + SessionStore
    + MaintenanceStore + MineCursorStore + EvolutionCandidateStore
    + Send + Sync + 'static {}

impl<T> Backend for T where T: /* 同上全部 */ {}
```

11 个 sub-trait 各管一块职责:

| sub-trait | 职责 |
|-----------|------|
| `CapsuleStore` | 记忆 CRUD、状态转移、版本链 |
| `CapsuleSearchStore` | 候选拉取(ANN + BM25 + scope/graph fallback) |
| `EmbeddingJobStore` | 嵌入任务队列(claim/complete/fail/stale) |
| `EmbeddingVectorStore` | 向量存取(写入/最近邻) |
| `GraphStore` | 图边读写、按有效期失效、多跳 BFS |
| `TranscriptStore` | transcript 档案 CRUD + 读 |
| `EntityRegistry` | 实体/别名规范化 |
| `SessionStore` | 会话元数据 |
| `MaintenanceStore` | vacuum、重建索引、健康 |
| `MineCursorStore` | `mem mine` 的 per-transcript 游标(反馈去重) |
| `EvolutionCandidateStore` | 演化候选(merge/refine/split 提案) |

为什么用 supertrait + blanket impl 而非"一堆 `Arc<dyn _>` 的 struct":交接时只需一次 `Arc::clone`;任何实现了全部 11 个 sub-trait 的具体类型自动成为 `Backend`,零样板。测试用的 `InMemoryCapsuleStore` 只实现子集,**刻意不满足** `Backend`——parity 测试继续用 `Arc<dyn CapsuleStore>`。详见 [`backend-coupling.md`](backend-coupling.md)。

### 6.2 三后端与运行期选择

`MEM_BACKEND`(`lance` 默认 | `postgres` | `clickhouse`)选后端。**三者都编译进每个 build**(default 依赖,自 2026-06-25 起无 cargo feature 门控),运行期在 `app.rs` 的 `match config.backend` 里分发:

| 后端 | 句柄 | 状态 | Store-glue worker(last_used/potentiation) |
|------|------|------|--------|
| **Lance**(默认) | `Store::open_with_provider(db_path, provider)` | 生产就绪 | **spawn**(需要具体 `Arc<Store>`) |
| **Postgres** | `PostgresCapsuleStore::connect(url)` + 幂等迁移 | 可用(transcript fan-out 已接) | 跳过(是优化非正确性,receiver 丢弃) |
| **ClickHouse** | `ClickHouseBackend::connect(url)` + `apply_migrations` | **UNVALIDATED**——11 个 sub-trait 中 `CapsuleStore` 外多为 `unimplemented!()` 桩(P3–P5 填),从未跑过真实 CH | 跳过 |

PG/CH 只需在 URL 里带凭据(`MEM_POSTGRES_URL` / `MEM_CLICKHOUSE_URL`,缺则启动明确报错)。详见 [`postgres-backend.md`](postgres-backend.md) / [`clickhouse-backend.md`](clickhouse-backend.md)。

> **注**:`app.rs` 里两个 Store-glue worker(K9 potentiation、O1 last_used)在 `match` 的 Lance arm 内 spawn,因为它们调 `Store` 级组合方法(`potentiate_edge` / `bump_last_used_at`),需要具体类型;PG/CH arm 给 capsule service 一个"活的" `capsule_used_tx` 但 receiver 被丢弃,事件静默丢掉。

**加一个新后端的步骤**(以 PG arm 为参考模板,**不要**拿 CH 当模板——它多数 sub-trait 还是 `unimplemented!()` 桩):

1. 实现全部 **11 个 sub-trait**(§6.1),你的具体类型即自动满足 `Backend`(blanket impl,无需手写 `impl Backend`)。可分阶段——先 `CapsuleStore`,其余 `unimplemented!()` 占位,像 CH 的 P2–P5。
2. 在 `config.rs` 的 `BackendKind` 加枚举 arm + `MEM_BACKEND` 解析 + 后端 URL/连接参数。
3. 在 `app.rs` 的 `match config.backend` 加一个 arm,产出统一三元组 `(store: Arc<dyn Backend>, edge_access_tx: Option<…>, capsule_used_tx)`;若不打算实现 Store-glue 优化,`edge_access_tx = None`、`capsule_used_tx` 给个活 sender 但丢弃 receiver(照抄 PG arm)。
4. 幂等迁移放 `migrations/` + 后端的 `apply_migrations`;`match` 之下的所有代码 backend-agnostic,无需改动。

详见 [`backend-coupling.md`](backend-coupling.md) §6.5 + [`postgres-backend.md`](postgres-backend.md)。

### 6.3 Lance-native 读写(route-B)

**这是理解读路径的关键**。2026-06-24 的 route-B 删掉了老的 DuckDB 读引擎,读写现在都走 **lancedb Rust API**:

- **没有** SQL 引擎、`ATTACH`、进程内 DuckDB 连接、读连接序列化、per-connection 版本缓存(`refresh`/`mark_dirty`/`ensure_fresh` 全删)。
- 读表达为 lancedb 查询:`query().only_if("<sql predicate>")` 做 filter 扫描,`query().nearest_to(vec).nprobes().refine_factor()` 做向量 ANN,`fetch_*_by_ids` 补全整行。
- **排序、聚合、版本链去重、graph-BFS、RRF 融合全在 Rust 里**跑在这些原语之上(`pipeline/retrieve.rs::sql_rrf` 在 BM25+ANN 候选 id 的名次上重算 RRF——老那条 fused SQL 的可移植等价物)。
- **读一致性**:读连接用 `read_consistency_interval(0)`(Strong),每次读透明看到最新已提交版本(便宜的 per-read manifest 检查),无需显式 refresh。

迁移全记录见 [`remove-duckdb-keep-lance.md`](remove-duckdb-keep-lance.md)。

### 6.4 三个"非 SQL"子系统:FTS / 向量 ANN / 图

route-B 之后,几个功能不再靠 Lance 的表函数,而是各自独立:

**① 全文/BM25 = Tantivy(内存)**。`src/storage/fts.rs` 是自包含子系统,**不是** Lance 索引:per-bucket(capsule + transcript)的 `Index::create_in_ram` 倒排索引,用 **jieba** 精确模式分词器(应对中文为主的语料)。查询文本被 jieba 切成 term,组成 `should`/OR 的 `BooleanQuery`(一段不空格的 CJK 直接丢给 Tantivy `QueryParser` 会被当成 0 命中的短语查询,所以必须切词)。索引在 `LanceStore::open` 启动时从源 Lance 表全量重建,之后 vacuum worker 驱动 `rebuild_query_indexes`/`ensure_query_indexes` 时再重建;真实规模全量重建 <1s(10× 时 ~6s),所以没有磁盘索引、没有 stale 窗口、没有软降级机制。

> **文档漂移提示**:`src/worker/mod.rs` 顶部注释仍说"BM25 index 用 lance extension 的 native FTS,没有 fts_worker"——这段**已过时**。route-B 后 BM25 是上面的 Tantivy 子系统。以本节和 `storage/mod.rs` / `storage/fts.rs` 为准。

**② 向量 ANN = Lance IVF_PQ(非自动)**。Lance **不会**自动建向量索引——未建索引的 `nearest_to` 会全表扫(曾让 transcript 搜索在大 `conversation_message_embeddings` 表上 5–11s)。`ensure_vector_indexes`(`lance_store/maintenance.rs`,vacuum worker 在启动 + 每次 sweep 驱动)在缺索引时建 per-embedding-table 的 IVF_PQ,delta 涨了就重建;<5k 行的表保持 flat(反正亚秒)。

**③ 图 = `graph_edges` Lance 表(带有效期)**。`src/storage/lance_store/graph.rs`:边带 `valid_from`/`valid_to`(valid-time 时序,见 §15.2)。`sync_memory_edges` 写活跃边,supersede 时 `close_edges_for_capability_capsule`;读默认 `valid_to IS NULL`(只活跃)。时间点查询走 `neighbors_within(node, max_hops, as_of)`(Rust 迭代 BFS,`MAX_HOPS_CAP = 3`)。详见 §11。

### 6.5 Single-writer 锁与写路径

**先厘清"single-writer"的两个 scope,免得看似矛盾**:

- **进程级(互斥)**:同一 DB 目录只能有**一个 `mem` 进程**在写。`Store::open` 用跨平台咨询文件锁(`fs4`)拒绝第二个 `mem` 进程打开同一目录,把这条约定变成运行期守卫。
- **进程内(并发)**:单个 `mem serve` 进程**内部有多个并发写任务**(ingest、decay、各 worker),**不是**单写线程。所以碰写路径的代码要按并发写来设计,提交冲突交给 Lance native retry(见下)。

下面这些机制都是在"进程内多写任务"这个前提下成立的:

写路径注意点:

- **Decay 写**(`apply_time_decay`:硬过期 + 2 趟衰减;`bump_last_used_at`)在 `src/storage/lance_store/decay.rs`,走 lancedb `table.update()`——与 ingest **同一个 writer**,所以老的 dual-writer 竞态没了。
- 两个 Rust-API writer 之间的提交冲突由 **Lance 自带 native retry** 处理(`UpdateBuilder::execute_with_retry`,~10 次/30s,重试间重新快照)。`LanceStore::with_lance_commit_retry` 只是极端竞争下的薄外层安全网(重开表重试,最多 3 次)+ 确定性测试缝。
- **Vacuum**(Lance manifest 剪枝)默认非激进——`MEM_VACUUM_AGGRESSIVE=1` 才 opt-in `delete_unverified=true`,因为激进剪枝曾绕过 Lance 的 in-flight 底线、删掉在途提交仍引用的 manifest,导致 serve core dump(`mem serve` 有多个并发 writer 任务,**不是**单 writer)。

### 6.6 存储层模块地图

```
storage/
  backend.rs              Backend 伞 trait + blanket impl
  {capsule,capsule_search,embedding_job,embedding_vector,graph,
   transcript,session,maintenance,mine_cursor,evolution_candidate}_store.rs
                          10 个 sub-trait 定义(+ InMemoryCapsuleStore 测试实现)
  entity_registry.rs      EntityRegistry —— 第 11 个 sub-trait(实体/别名)
  store.rs                Store —— 组合 LanceStore + open-lock,暴露全方法面
  lance_store/            Lance 实现(pub(crate),外部只经 Backend)
    mod.rs                LanceStore + Connection(read_consistency_interval(0))
    capability_capsules.rs / transcripts.rs / episodes.rs / sessions.rs
    embedding.rs / graph.rs / entities.rs / decay.rs
    maintenance.rs        ensure_vector_indexes / rebuild_query_indexes / vacuum
    evolution.rs / mine_cursors.rs
  fts.rs                  Tantivy 内存 BM25 子系统(jieba)
  postgres_store/         PG 后端(backend/capsule_store/mod)
  clickhouse_store/       CH 后端(11 个文件,P2–P5 分阶段)
  open_lock.rs            fs4 single-writer 锁
  types.rs                行 payload + StorageError/GraphError
  time.rs                 时间戳工具
```

---

## 7. 双管道:capsule 与 transcript

mem 里有**两条平行管道**,共享零状态,这是理解全局的一个关键切分:

```
   capsule 管道(记忆)                    transcript 管道(逐字会话档案)
   ─────────────────                     ──────────────────────────
   表  capability_capsules               表  conversation_messages
   队列 embedding_jobs                    队列 transcript_embedding_jobs
   向量 capability_capsule_embeddings     向量 conversation_message_embeddings
   worker embedding_worker               worker transcript_embedding_worker
   service CapabilityCapsuleService      service TranscriptService
   接口 /capability_capsules/*           接口 /transcripts/*(仅 HTTP)
   MCP  capability_capsule_* 工具         MCP  transcript_* 工具
```

- **capsule 管道**:抽取出的结构化记忆,走完整的 ingest→retrieve→compress→workflow 四阶段、生命周期、演化、图谱。
- **transcript 管道**:逐字会话归档,不做记忆抽取(那是 capsule 管道的事),只归档 + 独立的语义搜索。`mem mine` 是**双 sink**:一次 transcript 扫描既写抽取出的记忆(进 capsule 管道),又把每个 block(text/tool_use/tool_result/thinking)写进 transcript 档案。

**为什么分开**:记忆是被打分、演化、会归档的"提炼物";transcript 是永不改写的"原始证据"。两者生命周期、redaction 策略、索引方式都不同,硬凑一条管道会互相拖累。

transcript 读路径有一个重要的**软降级**特性,见 §10.4。

---

## 8. Capsule 四阶段管道

Pipeline(`src/pipeline/`)是**行为的心脏,不是 `service/`**。四个主阶段:

```
  ingest.rs         retrieve.rs                compress.rs              workflow.rs
  ───────           ──────────                 ──────────               ──────────
  状态判定           加性整数打分:               token 预算内四段输出:      episode → workflow
  content_hash      语义+词法+scope+intent      directives /             泛化
  (sha2)            +confidence+freshness       relevant_facts /
  redaction         −decay+graph                reusable_patterns /
  graph edge 抽取    + hybrid RRF 融合           suggested_workflow
  entity 解析        + 多样性 cap / pool 限       + redaction
  embedding 入队     + graph boost
```

辅助模块:`chunk`(长内容分块,见 [`long-content-recall.md`](long-content-recall.md))、`ranking`(打分细节)、`redact`(secret redaction)、`entity_normalize`(实体规范化)、`session`(会话)、`transcript_recall`(transcript 召回)、`eval_metrics`(离线评估指标)。

### 8.1 Ingest 阶段

`pipeline/ingest.rs`:

1. **状态判定**:根据 `write_mode`(active / propose)与治理开关,决定落 `Active` 还是 `PendingConfirmation`。
2. **content_hash**:sha2 算内容哈希,用于幂等去重(同内容重复 ingest 不消耗额度、不重复建行)。
3. **Redaction**:高置信度 secret 模式在**输出/索引层**被 mask(见 §13.2),存储仍 verbatim。
4. **Graph edge 抽取**:`extract_graph_edge_drafts` 从 topics 抽草稿边,`resolve_drafts_to_edges` 经 `EntityRegistry` 把 `to_node_id` 解析成 `entity:<uuid>`。
5. **Embedding 入队**:往 `embedding_jobs` 表写一行,嵌入异步做(失败不阻塞 ingest,见 §9)。
6. **可选近重复审查**(O2/O7a):嵌入后由 embedding worker 检查近重复簇,疑似则翻 `PendingConfirmation`(见 §10.3)。

**Verbatim 守卫**:调用方给了显式 `summary` 时,server 强制它 ≠ `content`(防止 agent 把提炼文本塞进 content 字段)。没给 summary 时,server 从 `content[:80]` 派生一个仅供索引用的 summary。

### 8.2 Retrieve 阶段

`pipeline/retrieve.rs` 是排序核心(各信号的打分/权重实现在此,辅以 `pipeline/ranking.rs`)。**加性整数打分**:每条候选的分 = 语义 + 词法 + scope + intent + confidence + freshness − decay + graph 的整数加权和。

关键机制:

- **Hybrid 检索**:BM25(Tantivy)候选 + ANN(Lance)候选,`sql_rrf` 在两个 id 名次列表上重算 **RRF 融合**。
- **多样性 cap**(O3,`MEM_RECALL_PER_SOURCE_CAP` 默认 3):同一 source(session_id,否则胶囊自身 id)在排序头部最多留 N 条,溢出推到尾部——**软** cap,不丢东西,token 预算仍能到尾部;防一个 session 的近重复批霸榜。
- **Pool 限**(`MEM_RECALL_POOL_LIMIT` 默认无界):设 N>0 时只把最近写的 N 行非 guidance 装进生命周期池;`Preference`/`Workflow` guidance 永远含入(floor-exempt);hybrid 命中不受 cap 影响照样并入——是随语料增长的 scale 旋钮。
- **Graph boost**:1-hop 图邻居给命中加分。

### 8.3 Compress 阶段

`pipeline/compress.rs`:token 预算内产出**四段结构化输出**——`directives` / `relevant_facts` / `reusable_patterns` / `suggested_workflow`。它操作 `content`(事实源),**永不**用 `summary` 当答案基础。这是所有压缩后搜索答案 + recall banner 文本的**唯一 choke point**,也是 redaction 的一个接缝(见 §13.2)。

recall banner 有两种风格(`MEM_RECALL_STYLE`,默认 `index`):`index` 只给一行 headline + `[mem_…]` id,agent 按需 `capability_capsule_get` 拉 verbatim(banner 体积小 ~45%);`snippet` 回退到完整片段。**注意**:banner 格式被 `cli/feedback.rs::scan_transcript` 反向解析,改渲染器要连解析器一起改(round-trip 测试锁着)。

### 8.4 Workflow 阶段

`pipeline/workflow.rs`:episode(事件序列)→ workflow(可复用流程)的泛化。与 §10.3 的 H4 `workflow_generalize` 演化提案相关。

---

## 9. Embedding 子系统

嵌入是**异步 + 持久**的,失败不阻塞 ingest:

```
   写入(capsule/transcript)
        │  ingest 往队列写一行
        ▼
   embedding_jobs / transcript_embedding_jobs 表
        │  worker claim(状态 pending → processing,带 5min lease)
        ▼
   embedding_worker / transcript_embedding_worker
        │  provider.embed_batch(...)  (spawn_blocking,本地推理)
        ▼
   capability_capsule_embeddings / conversation_message_embeddings 表
        │  Lance 内部管向量索引(IVF_PQ,ensure_vector_indexes 建)
        ▼
   状态 → completed | failed | stale
```

- **Provider trait**(`src/embedding/`):`embed_anything`(本地,默认)/ `openai`(hosted)/ `fake`(测试)。`instance` / `wire` / `provider` 是接线。
- **Lease / orphan 回收**:job claim 进 `processing` 带 5 分钟可见性超时(`EMBEDDING_JOB_LEASE_MS = 300_000`)。超时的 `processing` 行被当**孤儿**(worker 崩了/进程重启/中途出错),下次 claim 可回收——否则孤儿永不被重拾,胶囊永久丢嵌入。
- **批量**:`EMBEDDING_BATCH_SIZE` 默认 8(2026-05-21 从 1 翻上来摊薄每 tick 开销);`EMBEDDING_WORKER_POLL_INTERVAL_MS` 默认 10s(从 1s 翻上来,1Hz 曾致 510% 空闲 CPU + 800+ blocking 线程)。
- 嵌入是 redaction 的一个接缝(pre-embed 文本先 mask,secret 永不进向量索引,见 §13.2)。

---

## 10. 后台 worker 全景

12 个 worker(`src/worker/`),各自 tokio 任务,各自 cadence,都持 `Arc<Backend>`。与 service 的区别:service 在请求内同步跑,worker 永远后台循环。`app.rs` 在装配时按 config 开关 spawn。

### 10.1 总表

| worker | 作用 | 默认 | 开关 env |
|--------|------|------|---------|
| `embedding_worker` | 消费 `embedding_jobs` → 写向量;兼 O2 近重复审查 | **ON**(总在) | — |
| `transcript_embedding_worker` | transcript 版嵌入 worker | **ON** | `MEM_TRANSCRIPT_EMBED_DISABLED=1` 关 |
| `decay_worker` | 批量 UPDATE `decay_score`(活跃行,封顶 1.0) | **ON** | — |
| `last_used_worker` | O1 检索强化:coalesce「capsule-used」事件,盖 `last_used_at`(锚定衰减时钟),读路径外 | **ON**(Lance) | `MEM_LAST_USED_FLUSH_SECS` 调 cadence(默认 5) |
| `vacuum_worker` | Lance manifest 剪枝 + 驱动重建 FTS/向量索引 | **ON** | `MEM_VACUUM_DISABLED=1` 关 |
| `auto_promote_worker` | 长空闲 `PendingConfirmation` → `Active`(审计一行 `feedback_events`) | **ON** | `MEM_AUTO_PROMOTE_DISABLED=1` 关 |
| `evolution_worker` | 记忆演化(merge/refine/split/generalize) | **OFF** | `MEM_EVOLUTION_ENABLED=1` 开(见 §10.3) |
| `dedup_worker` | 近重复 sweep,归档簇内较短者(破坏性) | **OFF** | `MEM_DEDUP_ENABLED=1` 开 |
| `idle_archive_worker` | 空闲归档(治理闸) | **OFF** | `MEM_IDLE_ARCHIVE_ENABLED=1` 开 |
| `topic_tunnel_worker` | 跨项目 `user_tunnel:topic:<X>` 边自动派生 | **OFF** | `MEM_TOPIC_TUNNEL_ENABLED=1` 开 |
| `cooccurrence_worker` | K10 实体共现 → `cooccurs_with` 边 | **OFF** | `MEM_COOCCURRENCE_ENABLED=1` 开 |
| `potentiation_worker` | K9 边动力学(Hebbian potentiation),读路径外 | **OFF**(Lance) | `MEM_EDGE_DYNAMICS_ENABLED=1` 开 |

> 多数治理 worker 是 **single-tenant MVP scope**(从 `MEM_TENANT` 取,默认 `local`);多租户扩展路径见各 worker 源码注释。
>
> **"12" 是 Lance 后端的满配**:PG/CH 后端跳过两个 Store-glue worker(`last_used` / `potentiation`,它们调 Lance-only 的 `Store` 级方法,见 §6.2),所以非 Lance 后端实际运行的 worker 数 <12。

### 10.2 三种 worker 语义模式

- **队列消费型**(embedding / transcript_embedding):claim-process-complete,带 lease/backoff。
- **周期 sweep 型**(decay / vacuum / auto_promote / dedup / idle_archive / topic_tunnel / cooccurrence / evolution):定时扫一批,做一批。
- **channel 排水型**(last_used / potentiation):读路径 emit 事件进内存 channel,worker 排水 + coalesce + 批量写,**把写压力挪出读路径**。这类 worker 在 Lance arm 里 spawn 且持具体 `Arc<Store>`(调 Store 级组合方法)。

### 10.3 记忆演化与治理(重点)

这是 mem 区别于普通记忆存储的地方——记忆会**自我演化**。核心是 `evolution_worker` + `src/evolution/`(引擎:`map` / `synthesis`):

- **Merge**(合并):余弦聚类找近重复簇,keep-longest 存活,其余归档。但有**两道防御**防止把"同话题不同事实"错并:
  - **主防御(总在,零 LLM)**:`detect_merge` 里的**过程孪生改路**(`is_procedural_sibling_cluster`)——成对不相交的 commit/code_ref 事实锚 = 同一过程的 N 次执行(reranker 都分不出与重复的区别),整簇不并;成员 ≥ `generalize_min_n` 时发一个 H4 `workflow_generalize` 占位(WORKFLOW 型 `PendingConfirmation`)。
  - **次防御(I2 离线 reranker,默认 OFF)**:每个待归档 loser 用 Qwen3-Reranker-0.6B(`src/rerank/candle_qwen3.rs`,CPU ~700ms/对,按 batch 载入、绝不常驻)双向打分,几何均值低于 `MEM_RERANK_MERGE_FLOOR`(默认 0.5)则**取消**该候选。rerank 出错则 fail-closed HOLD(留 pending 下轮重试)。详见 [`offline-reranker-lane.md`](offline-reranker-lane.md)。
- **Refine / Split**:精炼、拆分记忆(见 [`evolution-worker.md`](evolution-worker.md))。
- **写时近重复**(O2/O7a,`MEM_INGEST_NEARDUP_ENABLED`,默认 OFF):embedding worker 嵌完新 `Active` 胶囊后,向量搜其近重复簇,挑簇 canonical(最长,tie 取更早),把新胶囊翻 `PendingConfirmation` + 记 `suspected_supersede` 边待审(verbatim-safe,不自动合并/归档)。
- **KG functional predicate 自动失效**(G4,`MEM_KG_FUNCTIONAL_PREDICATES`,默认空 = OFF):`graph_add_edge` 写一条 predicate 被列为"单值"的新边时,先自动 close 掉同 `(from, predicate, *)` 的其它活跃边(Graphiti 式"新事实取代冲突旧边",纯结构化三元组、无 LLM)。只列**真正单值**的 predicate。

治理开关多数在本地实例上被打开(代码默认往往是 OFF/保守),排查"短胶囊被拒/被归档"先查实例的 `config.env`。演化线全貌见 [`evolution-worker.md`](evolution-worker.md);与 OSS 对标见 [`oss-memory-diff.md`](oss-memory-diff.md)。

### 10.4 transcript 读路径的软降级(重要运维特性)

Lance 的一个 stale-index ragged-batch bug:索引扫描偶发 `all columns in a record batch must have the same length`(`scanner.rs`),根因是**stale/部分覆盖的索引**——索引覆盖到 build 时刻的行,transcript embedding worker 继续追加行(unindexed delta),扫描合并"已索引段 + 未索引尾"时产出不等长列。route-B 后 FTS 半边已解决(Tantivy 无 Lance FTS 索引);剩下暴露的是 `conversation_message_embeddings` 上的 IVF/ANN 扫描。

**当前行为**:`TranscriptService::search` 在**每个 lance-scan 边界**软降级——语义 ANN、recent-browse、anchor 注入、Phase-2 hydrate、Phase-5 context window——捕 lance 读错误 `warn!` 后降级(而非 500)。**规则:transcript 读路径任何新的 lance-scan 调用必须放进这个软降级模式,不能裸 `?`**。净效果:transcript 搜索永不 500。

**自愈**(2026-06-29):语义 ANN 边界遇 ragged-batch 时强制重建索引(`rebuild_query_indexes`)并重试一次,stale 窗口首次命中即自愈;进程级 `ReindexGuard`(CAS + RAII)封顶一个在途重建,防止失败查询雪崩。彻底消除仍需上游 Lance 修 stale-index 扫描。

---

## 11. 知识图谱与实体

mem 内建一个**带有效期的知识图谱**(valid-time 时序,见 §15.2),不是附属特性——它参与检索打分(graph boost)、事实断言(commit_fact / kg_add_edge)、跨项目关联(tunnels)。

### 11.1 图边(`graph_edges` 表)

- 边 = `(from_node_id, predicate, to_node_id)` + `valid_from` / `valid_to`(有效期,valid-time)。读默认 `valid_to IS NULL`(只活跃)。
- 写:ingest 抽取的边经实体解析后 `sync_memory_edges` 写入;调用方直接给的边走 `add_edge_direct`(保留调用方 `valid_from`)。
- 失效:supersede 触发 `close_edges_for_capability_capsule`;显式事实关闭走 `invalidate_edge(from, predicate, to, ended_at)`;G4 functional predicate 自动失效见 §10.3。
- 读:活跃邻居默认只读;时间点查询 `neighbors_within(node, max_hops, as_of)`(Rust 迭代 BFS,`MAX_HOPS_CAP = 3`);全图聚合 `graph_stats()`。

### 11.2 实体注册表

`entities` + `entity_aliases` 表把别名字符串规范化到稳定 `entity_id`(UUIDv7)。`MemoryRecord.topics: Vec<String>` 是调用方输入;ingest 经 `EntityRegistry` 把 `graph_edges.to_node_id` 解析成 `entity:<uuid>`。别名在 PK 处规范化(小写 + 空白折叠),`canonical_name` 保留调用方原样。**tenant-scoped,session-orthogonal**。

### 11.3 派生边(worker 产)

- **Tunnels**(`topic_tunnel_worker`):跨项目共享 topic → `user_tunnel:topic:<X>` 边,MCP 有 `kg_find_tunnels` / `kg_follow_tunnels` / `kg_list_user_tunnels`。
- **Cooccurrence**(`cooccurrence_worker`):项目内实体对共现 ≥ `min_count` → `cooccurs_with` 边(实体↔实体,经 kg_query / 多跳浮现,不进 1-hop retrieve boost)。

图/KG 的 MCP 工具面(`kg_add_edge` / `kg_invalidate_edge` / `kg_query_predicate` / `kg_timeline` / `graph_neighbors` / `graph_stats` / `commit_fact` / `fact_check` 等)见 [`mcp-capabilities.md`](mcp-capabilities.md)。

---

## 12. 接口面(HTTP、MCP、CLI、钩子)

### 12.1 HTTP(`src/http/`)

axum 0.8,`http::router()` merge 12 个子路由 + 一个 logging 中间件:

| 子路由 | 覆盖 |
|--------|------|
| `health` | `/health`(read-only,不鉴权) |
| `capability_capsule` | 记忆 CRUD / search / ingest / batch / feedback / supersede / review / bootstrap … |
| `transcripts` | `/transcripts/messages` · `/transcripts/search` · `/transcripts?session_id=…`(仅 HTTP) |
| `embeddings` | 嵌入 job / provider / rebuild(admin) |
| `review` | 审查队列 accept/edit_accept/reject |
| `graph` | 图边 / 邻居 / stats |
| `entities` | 实体 CRUD / alias |
| `fact_check` | `POST /fact_check`(pre-ingest 实体+KG sanity check,无 LLM) |
| `maintenance` | reindex / 维护 |
| `metrics` | `GET /metrics`(read-only,不鉴权) |
| `mine_cursors` | `mem mine` 游标 |
| `admin` | `/admin/reindex` 等 |

逐接口 + 数据流见 [`api-data-flow.md`](api-data-flow.md)。

### 12.2 MCP(`src/mcp/`)

stdio JSON-RPC 转发器,把工具调用转成 HTTP 打到 `MEM_BASE_URL`。默认 tenant 从 `MEM_TENANT`(默认 `local`)。`MEM_MCP_EXPOSE_EMBEDDINGS=1` 开 admin `embeddings_*` 工具。transcript 搜索**刻意只走 HTTP**,不上 MCP 面。工具清单见 [`mcp-capabilities.md`](mcp-capabilities.md)。

### 12.3 CLI 子命令(`src/cli/`)

`serve` / `mcp` 之外的一次性工具:`mine`(双 sink 挖记忆 + 归档)、`import`(纯归档)、`wake-up`(会话启动注入)、`feedback-from-transcript`(补反馈)、`hook`(Claude Code 钩子入口)、`sync`(跨后端迁移)、`init`(脚手架)。启发式/LLM 抽取 lane 在 `cli/heuristic_extract.rs`(O7b,`MEM_MINE_HEURISTIC_EXTRACT`)/ `cli/llm_extract.rs`(O7c,`MEM_MINE_LLM_EXTRACT` + `LLM_*`),都默认 OFF、都走审查门。

### 12.4 Claude Code 钩子集成

`mem hook <event>` 读钩子 stdin 的 JSON、打印钩子输出信封、**永远 exit 0**(不能阻塞用户)。典型接法:

- **UserPromptSubmit** → 自动 recall banner 注入(风格由 `MEM_RECALL_STYLE` 定)。
- **Stop / PreCompact** → `mem feedback-from-transcript` 补 `applies_here` 反馈,即使 agent 忘了调 `capability_capsule_feedback`,生命周期也能闭环。

> **已知脆点**:反馈补偿依赖解析 transcript 里的 recall banner / 搜索工具名。banner 格式或工具名(如 `memory→capability_capsule` 改名、插件命名空间前缀)一变,`cli/feedback.rs::scan_transcript` 会静默匹配不到 → 反馈静默失效。改任一侧先查这个解析器(round-trip 测试锁着两侧)。

### 12.5 反馈闭环

见 §5.2 的 kind 表。要点:每条记忆每 session **至多一个**信号(取最强的);只对**真读了并用了**的记忆发;`incorrect` 是破坏性的(归档),留给"我核实过它错了"。反馈即时写入、下次检索可见,无离线批。MCP 单入口 `capability_capsule_feedback`(`tenant` 由 wrapper 从 `MEM_TENANT` 自动填)。

---

## 13. 横切关注点

### 13.1 可观测性(`src/metrics.rs` + `GET /metrics`)

进程级 `once_cell::Lazy` 的 `AtomicU64` 计数器单例(零新依赖,**不**穿过 `AppState`,choke point 经 `crate::metrics::metrics()` 直接够到)。`GET /metrics`(read-only、不鉴权,像 `/health`)返回扁平 JSON 快照。计数器名**管道 scope 显式**(mem 跑两条平行管道 + episodes):`capsule_ingest_total` / `capsule_search_total` / `transcript_ingest_total` / `transcript_search_total` / `episode_ingest_total` + `redaction_hits`(单一跨面计数)+ `neardup_flags` + `kg_auto_invalidated` + `feedback_*`(逐 kind)。**进程本地,重启归零,不持久**。增量只在行为 choke point,`Relaxed` 序,读路径无锁。这是 *online* 补充,*offline* 是 O6 eval 框架(golden_recall / mempalace_bench)。

### 13.2 安全:secret redaction(O5,默认 ON)

`pipeline/redact.rs::redact_secrets` 在**输出/索引层**mask 高置信度 secret 模式(`sk-`、AWS `AKIA`、私钥、GitHub token、JWT、`Bearer`、Stripe、Slack、Google `AIza…` 等,token 模式带 `\b` 防 in-word 误触)。接在**四个接缝**:

1. `pipeline/compress.rs::compress_text`(capsule 压缩输出的唯一 choke point);
2. `worker/embedding_worker.rs::embed_input_chunks`(pre-embed,key 永不进向量索引);
3. `worker/transcript_embedding_worker.rs`(transcript pre-embed);
4. `service/transcript_service.rs::redact_window_blocks`(transcript 搜索输出,prompt-bound 路径)。

**存储恒 verbatim**——`memories.content` / transcript `content` 磁盘上永不改写;显式 verbatim-fetch 路径(`capability_capsule_get` / `transcripts_range` / `get_by_session`)刻意**不** redact。无匹配文本返回 `Cow::Borrowed`(热路径零分配)。`MEM_REDACT_SECRETS_DISABLED=1` 整体关。

### 13.3 隐私守卫

配置的嵌入 provider 若会把内容发出本机(hosted),启动 `from_config` 大声 warn;`MEM_PRIVACY_WARN_SUPPRESS=1` 静音。

### 13.4 配置(`src/config.rs`)

`Config::from_env` 是所有子命令共享的配置源。env 变量分组(`db_path` / `bind_addr` / backend 选择 / embedding / vacuum / auto_promote / dedup / evolution / idle_archive / topic_tunnel / cooccurrence / edge_dynamics / ingest / rerank …)。**每个 env 的默认值与动机以 [`AGENTS.md`](../AGENTS.md) 为权威**,那里逐条列了默认值 + 为什么是这个默认 + 反向 opt-out 开关。

### 13.5 部署与打包

- **Docker**:`Dockerfile`;**跨编译**:`cross build --release`(读 `Cross.toml`)。
- **npm 安装器**:`@shibenenen/mem`(`packaging/`)。
- **部署运维**:`deploy/`(supervisord 托管)。重装/升级时 cargo-bin 可能被 mcp 转发进程占用(Text-file-busy),用 rm+cp 绕。新装默认数据路径是 `mem.lance`(旧 `mem.duckdb` 目录名仅为兼容保留,里面装的是 Lance 数据集)。

---

## 14. 关键设计约束与不变量

改 mem 之前,先认清这几条硬约束——违反它们的改动几乎一定是 bug:

1. **Verbatim 规则**:`memories.content` 是事实源,存储层永不改写/截断。`summary` 只是索引/提示,**绝不**作答案基础或引用来源。压缩/摘要/redaction 只在输出/索引层。调用方给 `summary` 时强制 ≠ `content`。
2. **两轴分层**(见 [`oss-memory-diff.md`](oss-memory-diff.md) / `mempalace-diff` §8):📦 存储保持 verbatim;🔍 索引/排序/生命周期是结构化信号的家;⚙️ 基础设施/bug-fix 自成一轨。动排序/ingest/输出前,先说清自己在哪一层。
3. **Single-writer**:一个数据集目录只能一个 `mem serve`(`fs4` 锁运行期守卫);MCP 无状态可多开。注意 `mem serve` 进程**内部**有多个并发 writer 任务,所以 vacuum 的激进剪枝默认 OFF。
4. **Local-first**:默认无外部依赖(PG/CH 可选),嵌入默认本机。hosted provider 会 warn。
5. **反馈闭环是契约**:生命周期只在有反馈时推进;只读消费者会让所有记忆的分冻结在写入时刻。
6. **审查门**:非 `Active` 写入(propose/near-dup/抽取)绝不直接进 `Active`,一律 `PendingConfirmation` 待审。
7. **状态机纪律**:碰记忆的代码必须尊重 `Provisional/Active/PendingConfirmation/Archived` 转移(`domain/capability_capsule.rs` + `pipeline/ingest.rs::initial_status`)。
8. **软降级纪律**:transcript 读路径的新 lance-scan 必须进 §10.4 的软降级模式,不能裸 `?`。

工程门(提交前必过,CI 强制):`cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`(含 `tests/`)。

---

## 15. 附录

### 15.1 模块地图(带注释)

```
src/
  main.rs            入口:clap 子命令分发、jemalloc、tokio 运行时(blocking 池 32)
  lib.rs             顶层模块导出
  app.rs             唯一装配点 AppState::from_config(backend match + worker spawn + service)
  config.rs          Config::from_env
  error.rs           统一错误 → HTTP 状态码(如 RateLimited → 429)
  metrics.rs         进程级原子计数器单例

  http/              axum 路由(12 子路由 + logging 中间件)
  mcp/               MCP stdio 转发器(server/client/config/result)
  cli/               子命令 handler(serve/mcp/mine/import/hook/wake_up/feedback/sync/init
                     + heuristic_extract/llm_extract 抽取 lane)

  service/           4 门面:capability_capsule / transcript / entity / fact_check(+ embedding_helpers)
  pipeline/          ingest / retrieve / compress / workflow(+ chunk/ranking/redact/
                     entity_normalize/session/transcript_recall/eval_metrics)
  evolution/         演化引擎:map / synthesis(被 evolution_worker 驱动)
  rerank/            I2 离线重排:candle_qwen3 / fake

  domain/            capability_capsule / conversation_message / entity / episode /
                     edge_dynamics / embeddings / query / session / workflow
  storage/           见 §6.6
  embedding/          provider trait:embed_anything / openai / fake(+ instance/wire)
  worker/            见 §10.1(12 个 worker)
```

规模参考(src,~55k LOC):storage ~24k · worker ~5.9k · cli ~5.6k · pipeline ~5.5k · service ~3.7k · http ~2.1k · domain ~1.3k · embedding ~0.66k · rerank ~0.34k。

### 15.2 术语表

| 术语 | 含义 |
|------|------|
| **capsule / memory** | 一条记忆胶囊(领域类型 `capability_capsule`,旧名 `memory`,可互换) |
| **transcript** | 逐字会话档案(与 capsule 平行、零共享的管道) |
| **episode** | 事件序列,workflow 泛化的原料 |
| **version chain** | `supersedes_memory_id` 串起的记忆版本链,检索时去重到链头 |
| **decay / confidence** | 衰减分 / 置信度,反馈即时改写,参与打分 |
| **guidance** | `Preference` / `Workflow` 类型的记忆,召回时 floor-exempt(pool 限/多样性 cap 不砍它) |
| **RRF** | Reciprocal Rank Fusion,融合 BM25 + ANN 两路名次 |
| **near-dup** | 近重复,余弦 ≥ 阈值;写时/演化时触发审查提案 |
| **tunnel** | 跨项目共享 topic 派生的 `user_tunnel:topic:<X>` 图边 |
| **route-B** | 2026-06-24 迁移:删 DuckDB 读引擎、读写转 lance-native、FTS 转 Tantivy |
| **soft-degrade** | transcript 读路径捕 lance 扫描错误后降级而非 500 |
| **choke point** | 某类操作的唯一必经点(如 compress_text 是压缩输出唯一入口,便于挂 redaction/metrics) |
| **ANN** | Approximate Nearest Neighbor,近似最近邻向量检索(语义召回那一路) |
| **BM25** | 经典词法排序算法,mem 里由 Tantivy 提供(hybrid 检索的词法那一路) |
| **RRF** | Reciprocal Rank Fusion,把 BM25 + ANN 两路名次融合成一个排序 |
| **IVF_PQ** | Lance 的向量 ANN 索引类型(倒排文件 + 乘积量化);`ensure_vector_indexes` 按需建 |
| **cross-encoder** | 把 query+doc 拼一起送进模型打相关分的重排器(比双塔准但慢);I2 用 Qwen3-Reranker |
| **Tantivy / jieba** | Tantivy=纯 Rust 嵌入式倒排索引(BM25);jieba=中文分词器,给 Tantivy 的 `content` 字段分词 |
| **valid-time 时序** | 边只带 `valid_from`/`valid_to`(事实在现实中的有效期);**不含** transaction-time,故非完整 bitemporal(双时态)。本文早期草稿的"双时态"措辞已按此校正 |
| **manifest** | Lance 数据集的版本清单;每次提交产一个新 manifest,vacuum 剪旧的 |
| **Graphiti** | 一个时序知识图谱库;mem 的 functional-predicate 自动失效(G4)借鉴其"新事实取代冲突旧边"的做法 |
| **supertrait / blanket impl** | Rust 手法:`Backend` 是 11 个 sub-trait 的 supertrait,`impl<T> Backend for T where T: 全部` 是 blanket impl——满足全部即自动是 `Backend`,零样板 |

### 15.3 延伸阅读

- 存储/后端:[`backend-coupling.md`](backend-coupling.md) · [`remove-duckdb-keep-lance.md`](remove-duckdb-keep-lance.md) · [`database-schema.md`](database-schema.md) · [`postgres-backend.md`](postgres-backend.md) · [`clickhouse-backend.md`](clickhouse-backend.md)
- 接口:[`api-data-flow.md`](api-data-flow.md) · [`mcp-capabilities.md`](mcp-capabilities.md)
- 演化/召回:[`evolution-worker.md`](evolution-worker.md) · [`offline-reranker-lane.md`](offline-reranker-lane.md) · [`long-content-recall.md`](long-content-recall.md)
- 方向/路线:[`oss-memory-diff.md`](oss-memory-diff.md) · [`ROADMAP.MD`](ROADMAP.MD) · [`agent-memory-strategy-readiness.md`](agent-memory-strategy-readiness.md)
- 日常/历史:[`AGENTS.md`](../AGENTS.md) · [`CHANGELOG.md`](../CHANGELOG.md)

---

*本文档随架构演进更新。改动存储层/管道/接口的显著结构时,请同步本文对应章节与 §15.1 模块地图。*
