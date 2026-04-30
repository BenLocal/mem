# Conversation Archive — Design

> 在 `2026-04-29-claude-code-integration-design.md` 之上加一条**全量原始对话归档**管道，与既有"显式抽取关键句进 memories"的管道**并存且完全隔离**。提供原文回放 + 语义搜索两种用途，但不污染 `memories` 表的 ranking / lifecycle / verbatim guard。

## Summary

mem 现状：`mem mine` 只把 transcript 里被 `<mem-save>` 标记或匹配特定中英文模式的关键句抽进 `memories` 表。原始对话本身**不存**——它们在 Claude Code 的本地 `~/.claude/projects/.../*.jsonl` 文件里自然存在，但 mem 服务对这些文件没有任何索引、检索或回放能力。

本设计新增：

1. 新表 `conversation_messages`：每条 transcript message 的每个 content block（text / tool_use / tool_result / thinking）作为一行，verbatim 落库，外键到 `sessions(session_id)`。
2. 新表 `transcript_embedding_jobs`：与 `embedding_jobs` 结构相同但完全独立的嵌入队列。
3. 新 HNSW sidecar 文件 `<MEM_DB_PATH>.transcripts.usearch`：与 memories 用的 `<MEM_DB_PATH>.usearch` **不共享向量空间**。
4. 扩展 `mem mine`：单次解析 transcript，分两路 sink ——既有抽取关键句进 `memories` 流程不变，新增"每个 block 都写入 `conversation_messages`"。
5. 新 HTTP 路由：
   - `POST /transcripts/messages` — `mem mine` 用的内部 ingest 端点
   - `POST /transcripts/search` — 语义搜索
   - `GET /transcripts?session_id=…` — 按 session 拉完整 transcript（时间序）
6. embedding worker 复制一份逻辑，跑两条独立队列（一条 memories，一条 transcripts），分别写入对应 sidecar。
7. **MCP 不暴露任何 transcript 工具**（v1 决定）。

## Goals

- 全量保存对话原文（verbatim），可按 `session_id` / 时间范围 / role / block_type 查询。
- 提供独立语义搜索通道，不与 `memory_search` 共享向量空间。
- 与既有 `memories` 管道**零侵入**：ranking、lifecycle、verbatim guard、`compress.rs` 全部不动。
- 与既有 `sessions` 表（`2026-04-29-sessions-design.md`）天然集成，session 是连接两条管道的唯一关节。
- `mem mine` 单次扫描完成两路 sink；idempotency 键扩展到 block 粒度，重跑安全。
- 失败隔离：transcript 嵌入卡死不影响 memory ingest 路径；反之亦然。

## Non-Goals

- MCP tool 暴露 transcript 搜索（v1 不开；agent 走 `memory_search` → 命中 memory 后用 `session_id` 拉对应 transcript）。
- Memory + transcript 的统一/混合搜索路由（YAGNI；两条管道独立设计的承诺不破）。
- Retention / TTL / 老 transcript 归档压缩（见 Concerns to Confirm，v1 暂不做）。
- `mem wake-up` 读取 transcript（v1 暂不改 wake-up，仍只从 memories 拉）。
- 历史 transcript 文件的回填（v1 不做；当且仅当 `mem mine` 被显式重跑才入库）。
- Codex / 其他 agent transcript 格式适配（仅支持 Claude Code JSONL；其他格式后续 PR）。
- 实时 push（hook 仍是 Stop / PreCompact 这种回合级，不做 per-message 实时 POST）。
- transcript 行被覆写后的索引清理 / FK cascade 行为（Claude Code transcript 是 append-only，不存在覆写场景）。

## Decisions (resolved during brainstorming)

- **Q1 (目的)**：D — 既要原文归档又要语义搜索。
- **Q2 (存储形态)**：C — 新表 `conversation_messages` + 现有 memories 表并存；`mem mine` 同时写两路。两条管道完全独立。
- **Q3 (捕获范围)**：C — 所有 block（text / tool_use / tool_result / thinking）都 verbatim 入库，但 `embed_eligible` 字段控制是否进嵌入队列。默认 `text` / `thinking` → 嵌入；`tool_use` / `tool_result` → 不嵌入（避免大体积 tool output 把 HNSW 撑爆，但仍能被 SQL `LIKE` 命中）。
- **Q4 (捕获机制)**：A — 扩展现有 `mem mine`，不新增独立命令。复用 transcript 解析、idempotency 模式、hook 触发链。
- **Q5 (嵌入与索引)**：A — 完全独立的嵌入队列 (`transcript_embedding_jobs`) + 独立 HNSW sidecar (`<MEM_DB_PATH>.transcripts.usearch`)。worker 跑两个 task 实例，互不干扰。
- **Q6 (搜索接口)**：A — 仅 HTTP，不开 MCP。`POST /transcripts/search` 做语义搜索，`GET /transcripts?session_id=…` 做按会话回放。MCP 工具表面零变化。

## Architecture

```
Claude Code transcript (~/.claude/projects/<proj>/<session>.jsonl)
            │
            ▼
   ┌──────────────────────┐
   │  mem mine (扩展后)   │  解析一遍 transcript，分两路 sink
   └──────────┬───────────┘
              │
   ┌──────────┴────────────────────────┐
   ▼                                   ▼
[既有路径,本设计零修改]              [新路径]
显式抽取关键句                       全量原始 block 归档
 → POST /memories                     → POST /transcripts/messages
   ↓                                     ↓
 memories 表                          conversation_messages 表
 embedding_jobs 表                    transcript_embedding_jobs 表
 mem.usearch sidecar                  mem.transcripts.usearch sidecar
 GET/POST /memory_search              POST /transcripts/search
 (HTTP + MCP)                         GET /transcripts?session_id=…
                                       (HTTP only — 不暴露 MCP)
```

两条管道**唯一耦合点**：`session_id`。
- `memories.session_id` 已由 `2026-04-29-sessions-design.md` 引入。
- `conversation_messages.session_id` 新增 FK 到同一张 `sessions` 表。
- 用法：`memory_search` 命中某 memory，agent / 人类拿到 `session_id` 后通过 `GET /transcripts?session_id=…` 拉那次会话完整上下文。

## Schema

新增 `db/schema/006_conversation_messages.sql`（append-only 约定；**不**修改 001-004）：

```sql
-- Conversation archive: every block of every transcript message,
-- verbatim. Independent from memories table and its ranking/lifecycle.

create table if not exists conversation_messages (
    message_block_id text primary key,            -- UUIDv7, server-generated
    session_id text references sessions(session_id),
    tenant text not null,
    caller_agent text not null,                    -- e.g. "claude-code"
    transcript_path text not null,                 -- absolute path on disk
    line_number integer not null,                  -- 1-based line in transcript
    block_index integer not null,                  -- 0-based index within message.content[]
    message_uuid text,                             -- Claude Code's per-message uuid (if present)
    role text not null,                            -- 'user' | 'assistant' | 'system'
    block_type text not null,                      -- 'text' | 'tool_use' | 'tool_result' | 'thinking'
    content text not null,                         -- verbatim block content
    tool_name text,                                -- non-null when block_type = 'tool_use'
    tool_use_id text,                              -- correlate tool_use ↔ tool_result
    embed_eligible boolean not null,
    created_at text not null,                      -- from transcript timestamp, ISO8601
    unique(transcript_path, line_number, block_index)
);

create index if not exists idx_conv_session_time
    on conversation_messages(session_id, created_at);

create index if not exists idx_conv_tenant_agent_time
    on conversation_messages(tenant, caller_agent, created_at);

create index if not exists idx_conv_tool_use_id
    on conversation_messages(tool_use_id) where tool_use_id is not null;

-- Embedding queue: mirror of embedding_jobs but keyed to conversation_messages.
create table if not exists transcript_embedding_jobs (
    job_id text primary key,
    message_block_id text not null references conversation_messages(message_block_id),
    status text not null,                          -- 'pending' | 'processing' | 'completed' | 'failed' | 'stale'
    attempts integer not null default 0,
    last_error text,
    enqueued_at text not null,
    updated_at text not null
);

create index if not exists idx_tej_status_enqueued
    on transcript_embedding_jobs(status, enqueued_at);
```

### DuckDB caveats

- 同 sessions 设计里说过的：`alter table` 不带 `if not exists`，但本设计**不需要 alter**——只新建表。migration runner 重跑时只要能容忍"`create table if not exists`"，就没问题。
- `unique(transcript_path, line_number, block_index)` 保证 idempotency；`mem mine` 重跑时不需要先 SELECT 检查，直接 INSERT，靠 ON CONFLICT 或 try/catch 吃掉冲突（具体写法跟现有 `embedding_jobs` 写入路径对齐）。

## Domain Types

### `domain/conversation_message.rs` (new file)

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationMessage {
    pub message_block_id: String,
    pub session_id: Option<String>,
    pub tenant: String,
    pub caller_agent: String,
    pub transcript_path: String,
    pub line_number: u64,
    pub block_index: u32,
    pub message_uuid: Option<String>,
    pub role: MessageRole,
    pub block_type: BlockType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub embed_eligible: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockType {
    Text,
    ToolUse,
    ToolResult,
    Thinking,
}

impl BlockType {
    pub fn embed_eligible_default(self) -> bool {
        matches!(self, BlockType::Text | BlockType::Thinking)
    }
}
```

### `MemoryRecord` 不变

`memories` 表 / `MemoryRecord` 域类型本设计**完全不动**。

## Repository Surface

新增 trait 方法（放在 `storage/repository.rs` 或新增 `storage/transcript_repo.rs`，跟 `graph_store.rs` 同级，决于实现者偏好）：

```rust
async fn create_conversation_message(
    &self,
    msg: &ConversationMessage,
) -> Result<(), StorageError>;

async fn get_conversation_messages_by_session(
    &self,
    session_id: &str,
    tenant: &str,
) -> Result<Vec<ConversationMessage>, StorageError>;

async fn search_conversation_messages_by_vector(
    &self,
    tenant: &str,
    candidates: &[(String /* message_block_id */, f32 /* score */)],
) -> Result<Vec<(ConversationMessage, f32)>, StorageError>;

// Embedding queue
async fn enqueue_transcript_embedding_job(
    &self,
    job_id: &str,
    message_block_id: &str,
    enqueued_at: &str,
) -> Result<(), StorageError>;

async fn next_pending_transcript_job(
    &self,
) -> Result<Option<TranscriptJobRow>, StorageError>;

async fn mark_transcript_job_completed(&self, job_id: &str, now: &str) -> Result<(), StorageError>;
async fn mark_transcript_job_failed(&self, job_id: &str, err: &str, now: &str) -> Result<(), StorageError>;
```

`create_conversation_message` 必须在**单次 `Arc<Mutex<Connection>>` 锁占用**期间内完成两条写：
1. INSERT INTO conversation_messages（unique 约束保证幂等；冲突时 swallow）
2. 仅当步骤 1 真的写入了一行（`affected_rows == 1`）且 `embed_eligible = true` 时，紧接着 INSERT INTO transcript_embedding_jobs

不要先 SELECT 再 INSERT（与现有单写锁约束对齐，避免多余 round trip）。两个 INSERT 之间不允许释放锁，否则可能出现"消息行写入但 job 未入队"的竞态。

## Service / Pipeline 改动

### `cli/mine.rs` 扩展

伪代码：

```rust
for (line_number, line_str) in transcript_jsonl.lines().enumerate() {
    let line_number = line_number + 1; // 1-based
    let entry: TranscriptLine = parse_or_skip(line_str);

    // [既有] 抽取关键句 → POST /memories（不变）
    for extract in extract_key_phrases(&entry) {
        post_memories(extract);
    }

    // [新增] 全量 block → POST /transcripts/messages
    for (block_index, block) in entry.message.content.iter().enumerate() {
        let msg = ConversationMessage {
            message_block_id: uuid::Uuid::now_v7().to_string(),
            session_id: Some(entry.session_id.clone()),
            tenant: cfg.tenant.clone(),
            caller_agent: cfg.agent.clone(),
            transcript_path: transcript_path.to_string(),
            line_number: line_number as u64,
            block_index: block_index as u32,
            message_uuid: entry.message.uuid.clone(),
            role: entry.message.role.into(),
            block_type: block.kind.into(),
            content: block.verbatim_text(),
            tool_name: block.tool_name(),
            tool_use_id: block.tool_use_id(),
            embed_eligible: block.kind.embed_eligible_default(),
            created_at: entry.timestamp.clone(),
        };
        post_transcripts_messages(msg);
    }
}
```

### 新增 `service/transcript_service.rs`

职责对应 HTTP 路由：

- `ingest_message(msg)` — `create_conversation_message` + 触发 transcript embedding job 入队（一次事务）。
- `search(query, filters, limit)` — 调用 embedding provider 拿 query 向量 → 在 transcript HNSW 取 top-N candidates → repo 拉原文 → 合并 score + filter（session_id / role / block_type / time_from / time_to）→ 返回。
- `get_by_session(session_id, tenant)` — 直接 SQL，不过 HNSW。

### 新增 `service/transcript_embedding_worker.rs`

照抄 `service/embedding_worker.rs` 的循环、重试、backoff、状态机模板，但：
- 队列读 `transcript_embedding_jobs`
- 写入 transcript 那个 `VectorIndex` 实例（指向 `<MEM_DB_PATH>.transcripts.usearch`）
- 失败行不影响 memories worker

两个 worker 在 `main.rs` 启动时各起一个 tokio task。

### 新增 HTTP 路由 (`http/transcript.rs`)

```rust
// POST /transcripts/messages
async fn post_message(State(svc): State<TranscriptService>, Json(req): Json<IngestRequest>)
    -> Result<Json<IngestResponse>, ApiError>;

// POST /transcripts/search
async fn post_search(State(svc): State<TranscriptService>, Json(req): Json<SearchRequest>)
    -> Result<Json<SearchResponse>, ApiError>;
// SearchRequest: { query, session_id?, role?, block_type?, time_from?, time_to?, limit }
// SearchResponse: { hits: [{ message_block_id, content, role, block_type, session_id, score, created_at }] }

// GET /transcripts?session_id=…&tenant=…
async fn get_by_session(
    State(svc): State<TranscriptService>,
    Query(q): Query<BySessionQuery>,
) -> Result<Json<TranscriptResponse>, ApiError>;
// TranscriptResponse: time-ordered list of all blocks for that session
```

`http/mod.rs` 在 `serve` 路由表里挂载这三条。

### `vector_index.rs` 改动

最小化：把单例 `VectorIndex::open(path)` 调两次，得到两个独立实例，分别由 memories worker / transcripts worker 持有。**不**改 `VectorIndex` 内部 API，**不**引入"namespace / collection"概念。

`mem repair --check|--rebuild` 也要扫两个 sidecar。`cli/repair.rs` 加一个迭代两个 `(table, sidecar_path)` 元组的循环。

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `MEM_TRANSCRIPT_EMBED_BATCH` | 1 | transcript embedding worker 单次拉取 job 数；保守默认避免 provider rate limit |
| `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY` | 256 | transcript HNSW sidecar 落盘频率；低于 memories（默认 1024）因为单 transcript session 写入 burst 更大 |

无新 idle / TTL 配置（retention 暂不做）。

## Testing Strategy

### Unit tests

- `domain/conversation_message.rs::tests`
  - `block_type::embed_eligible_default` 真值表
- `cli/mine.rs::tests`
  - 解析 fixture transcript：assistant message 含 text + tool_use + tool_result 三个 block → 产出 3 条 `ConversationMessage`，role / block_type / tool_use_id 正确
  - 同一行 block 重复解析：第二次产出的 message 应该被 unique 约束在 service 层吃掉（mock repository 验证调用次数）
- `service/transcript_service.rs::tests`
  - search filter 组合：mock repo 返回固定 candidates，验证 `role` / `block_type` filter 在 service 层的过滤逻辑

### Integration tests in `tests/conversation_archive.rs`

走 ephemeral DuckDB + 真实 axum app（同 `tests/ingest_api.rs` 模板）：

1. **`mine_writes_all_blocks_to_conversation_messages`**
   - Fixture transcript 4 行：1 user text、1 assistant text、1 assistant tool_use、1 user tool_result
   - 跑 `mem mine` → assert 4 行写入；session_id 正确；embed_eligible 仅 text 为 true
   - `embedding_jobs` 不应该有新行；`transcript_embedding_jobs` 应该有 2 条（两个 text block）

2. **`mine_is_idempotent_at_block_level`**
   - 跑两次 `mem mine`，第二次 `conversation_messages` 行数与第一次相同；`transcript_embedding_jobs` 也不重复

3. **`get_by_session_returns_time_ordered_blocks`**
   - seed conversation_messages，时间戳乱序
   - GET `/transcripts?session_id=X` → 按 created_at ASC 排序；包含全部 block_type

4. **`search_filters_by_role_and_block_type`**
   - seed 多种 role × block_type 组合
   - POST `/transcripts/search` 带 `role=user` → 只返回 user
   - POST 带 `block_type=text` → 只返回 text

5. **`memories_pipeline_unaffected`**
   - 跑 `mem mine` 在含 `<mem-save>` 标记的 fixture 上
   - 既要进 `memories`（既有路径）也要进 `conversation_messages`（新路径）
   - 两边 `session_id` 应该相同（来自同一次 ingest）

6. **`transcript_embedding_isolation_from_memories`**
   - 模拟 transcript embedding provider 全部 fail（注入 fake provider）
   - memory ingest 路径 ingest_request 应该正常完成、写入 `embedding_jobs` 成功
   - transcript 那条管道全部 `failed`，不阻断主流程

### `mem repair`

- 扩展 `tests/repair_smoke.rs`（如已存在）或新增：跑 repair 时两个 sidecar 都被检查 / 重建。

## Risk Assessment

- **DuckDB 单写锁竞争加剧**：每次 `mem mine` 现在写 N 倍行（N = 平均每 message 的 block 数，通常 2-5）。`Arc<Mutex<Connection>>` 序列化所有写，可能让大 transcript 的 mine 变慢。缓解：mine 在 hook 里已经是 `&` 后台执行，不阻塞 AI；真出现性能问题时考虑 batch insert。
- **HNSW sidecar 体积膨胀**：transcript 嵌入数量远超 memories（每个 text block 一个 vector）。usearch 单文件 GB 级是常见的，目前没人压测过。缓解：`MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY` 调低做更频繁 flush；retention 策略未来 PR 考虑。
- **隐私风险放大**：原本 mem 只存"有意保留"的事实；新管道默认全收。Claude Code transcript 里可能有 secrets、token、PII。缓解：v1 只是本地存储（DuckDB 文件 + sidecar 都在 `MEM_DB_PATH` 边上），网络面零暴露；文档里**强调** `MEM_DB_PATH` 应当不上传备份。
- **transcript_path 漂移**：transcript 文件可能被用户重命名/移动，导致 idempotency key 失效，重跑产生重复行。缓解：v1 不解决；rely on `unique(transcript_path, line_number, block_index)` 在原路径下幂等，移动后视作新文件（重复入库一次）。下一版可考虑用 `message_uuid` 做主幂等键（如果 Claude Code 保证全局唯一）。
- **`tool_result` 巨大 block 撑爆 DuckDB 单行**：DuckDB 对单行 text 没有硬上限但极大值会拖慢全表扫描。缓解：v1 verbatim 存（坚守"📦 storage 层不裁剪"原则）；如果出现性能问题，迁移到 BLOB / 单独 `large_blocks` 副表。
- **embedding worker 双倍 provider 调用**：transcript 那条管道默认每个 text block 一次嵌入。OpenAI 计费用户成本翻倍。缓解：默认 provider 是 EmbedAnything 本地（`.env` 已配置）；用 OpenAI 的用户需要自己关 transcript embedding（**未实现**——见 Concerns）。

## Concerns to Confirm Before Implementing

1. **Q7 — Retention / TTL**：v1 不做。conversation_messages 无界增长。是否在 spec 里就加一个最简单的 `MEM_TRANSCRIPT_MAX_AGE_DAYS` env（值为空时不裁），还是彻底推到下一版？倾向**彻底推到下一版**——本 PR 已经够大；retention 策略设计单独立项更稳。
2. **Q8 — `mem wake-up` 是否拉 transcript**：v1 不改 `mem wake-up`，仍只读 memories。理由：wake-up 注入到 SessionStart，token 预算 800 很紧；transcript 大量低密度行进去等于浪费。等 transcript 搜索接口稳定后，再单独决定是否做"上次会话尾声 N 行 → wake-up"。
3. **Disable transcript embedding 的开关**：上面 Risk 提到的"OpenAI 用户成本翻倍"，需要一个 `MEM_TRANSCRIPT_EMBED_DISABLED=1` 让 worker 直接停。本 spec 未列入 Configuration——是否纳入？倾向**纳入**，加一行配置成本极低，避免上线后用户被动吃账单。
4. **`message_uuid` 来源稳定性**：Claude Code transcript schema 里的每条 message 是否一定有 `uuid` 字段？如果有，将来可以替换 `(transcript_path, line_number, block_index)` 作为更稳健的 idempotency 键。实现者读 `~/.claude/projects/.../*.jsonl` 几个真实文件验证一下；如果存在率 100%，spec 升级；如果不稳，保持当前 (path, line, block) 方案。
5. **`caller_agent` 取值统一性**：sessions 设计已经讨论过 `source_agent` vs `caller_agent` 的歧义；transcript 这条路径继续沿用 sessions 表选定的方案（实现者从 sessions PR 那里拿到当时的结论）。
6. **HTTP 路由命名**：本设计用 `/transcripts/messages` 做 POST、`/transcripts/search` 做搜索、`GET /transcripts?session_id=…` 做拉取。也可以用 `/transcripts` 单数 / `/transcript_messages` 之类。**实现者可以根据现有 `/memories` 命名风格自行定夺**，本 spec 不强约。

## Out of Scope (this PR)

- Retention / TTL / archive 压缩
- `mem wake-up` 集成 transcript
- MCP transcript 工具
- 跨表搜索（unified memory + transcript search）
- 历史 transcript 回填扫描
- transcript 数据导出 / dump 命令
- 跨 caller_agent 的 transcript 合并视图
- 加密存储 / secrets 自动 redact
- Codex / 其他 agent transcript 适配

## Verification Checklist (pre-merge)

- `cargo test -q` — 全部通过（含新增 `tests/conversation_archive.rs`）
- `cargo fmt --check` — 干净
- `cargo clippy --all-targets -- -D warnings` — 干净
- `cargo build --release` — 干净
- 手动冒烟：
  1. `cargo run -- serve`
  2. 用真实 Claude Code transcript 文件 `cargo run -- mine /path/to/foo.jsonl --agent claude-code`
  3. `select count(*) from conversation_messages` > 0
  4. `select count(*) from transcript_embedding_jobs where status = 'completed'` 等于 text + thinking block 数
  5. `curl -X POST localhost:3000/transcripts/search -d '{"query":"…", "limit":5}'` 返回结果
  6. `curl 'localhost:3000/transcripts?session_id=<X>'` 按时间序返回完整对话
  7. `cargo run -- repair --check` 两个 sidecar 都报告状态
- 失败注入冒烟：临时改本地 transcript embedding provider 让其报错；确认 `embedding_jobs` 仍正常处理 memories ingest，新 ingest 不被阻断

## References

- `docs/superpowers/specs/2026-04-29-claude-code-integration-design.md` — 既有 `mem mine` 设计，本 spec 的扩展起点
- `docs/superpowers/specs/2026-04-29-sessions-design.md` — `sessions` 表，本 spec FK 依赖
- `docs/superpowers/specs/2026-04-27-vector-index-sidecar-design.md` — HNSW sidecar 设计，本 spec 复制其模式
- `docs/superpowers/specs/2026-04-29-verbatim-guard-design.md` — verbatim 原则，本 spec `content` 字段遵循
- `docs/mempalace-diff.md` — MemPalace 对比 / roadmap，本 spec 可视为该 roadmap 的扩展项
- `db/schema/001_init.sql` ~ `004_sessions.sql` — schema 模式参考
- `src/cli/mine.rs`（待实现 / 已实现）— 扩展点
- `src/service/embedding_worker.rs` — transcript worker 复制模板
- `src/storage/vector_index.rs` — 双 sidecar 实例化点
