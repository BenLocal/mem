# Transcript Recall — Design

> 把 `POST /transcripts/search` 的召回质量往 memories pipeline 看齐。三件优先：(1) BM25 hybrid，复用 master 上 `bm25_search` 的 RRF 融合；(2) session co-occurrence + anchor + recency 加权，作为 memories `freshness/decay` 的 transcript 替身；(3) 命中后 hydrate 同 session ±k blocks，搜索结果以"对话片段"返回。约束：transcripts ↔ memories **zero shared state**（仅纯 helper 共享）；verbatim 优先；MCP **不**暴露 transcript 搜索。

## Summary

`feat/conversation-archive` 已落地一条 transcript-archive 管道：`conversation_messages` 表、独立 embedding queue、独立 HNSW sidecar、`mem mine` 双 sink、三条 HTTP 路由。但召回路径仍是单维 HNSW + 空查询 fallback，**信号面对比 memories 缺三块**：

1. 没有 BM25 lexical 通道（用户搜 "Rust project" 走纯语义，词形同义命中弱、稀有专有名词命中弱）。
2. 没有 freshness/recency 信号（同等相关度下，最近的 vs 半年前的 session 一视同仁）。
3. 命中后只返回单条 block（一句话脱离上下文，对人不可读）。

本设计补齐这三块。复用 master 刚 merge 的 BM25 + RRF 基础设施（`db/schema/005_fts.sql`、`bm25_candidates`、`fts_dirty` 模式、`score_candidates_hybrid_rrf` 内的 `1/(60+rank)*1000` 公式），但只共享纯函数，数据状态完全分离。响应形态从 `{ hits: [...] }` 改为 `{ windows: [...] }`：每个 window 是一段对话片段，含若干 primary 命中和 ±k 个 context block。

## Goals

- transcripts 搜索的 BM25 通道，与 HNSW 走 RRF 融合，与 memories 同一套 RRF 公式
- session co-occurrence bonus、anchor session bonus、recency bonus 三个 transcript 特有信号，与 RRF 加性叠加为 i64 总分
- 命中后 hydrate 同 session ±k 个 block 作为 context；同 session 重叠 windows 自动合并
- transcripts 与 memories pipeline **零数据/状态共享**（独立 fts_dirty flag、独立 FTS 索引、独立 scorer 入口、独立测试矩阵）
- 仅纯函数 helper（RRF 公式 + freshness 公式）抽到 `pipeline/ranking.rs` 共享——这是去重而非耦合
- 现有 memories `score_candidates_hybrid_rrf` 重构为调用共享 helper：**zero behavior change**，所有 memories 单测保持通过
- `POST /transcripts/search` HTTP 表面变化：新增 3 个可选请求字段；响应体从 `{hits}` 换为 `{windows}`（破坏式变更）

## Non-Goals

- 不向 MCP 暴露 transcript 搜索（沿用 conversation-archive 设计承诺）
- 不做 per-block_type 加权（如"user 消息更重要"）—— YAGNI
- 不做 retention/TTL（仍属 conversation-archive 下一版）
- 不做 `mem wake-up` 联动 transcripts
- 不做跨管道（memories + transcripts）混合搜索路由
- 不实现 windows 之外的自由分页（拉整段用 `GET /transcripts?session_id=…`）
- 不引入"按字节截断 block content" —— verbatim 守住，超大 tool_result 的解决方案落在"context 默认排除 tool 块"而非"截断"
- 不在 BM25 通道里包含 tool_use / tool_result content（与 HNSW 候选面对称，仅 embed-eligible）
- 不为新流量加 metric / dashboard（observability 是另一条独立 PR 的范畴）

## Decisions (resolved during brainstorming)

- **Q1（响应形态）**：B — `{ windows: [...] }`，同 session 重叠 windows 自动合并，`window.score = max(primary_scores)`，`primary_ids: Vec<String>` 标识哪些 block 是真命中。
- **Q2（session 加权语义）**：C — 默认 intra-session co-occurrence（候选集里同 session 的同伴多则加分）+ 可选 anchor-session boost（请求体 `anchor_session_id` 字段触发）。
- **Q3（score 合成模型）**：A — 镜像 memories 的加法 i64 模型；RRF 公式抽成纯函数 `rrf_contribution(rank: usize) -> i64` 共享；信号 magnitude 设计为可独立调参的 `pub const`。
- **Q4（hydration 默认参数）**：C — `context_window: Option<usize>` 默认 2、cap 10；`include_tool_blocks_in_context: bool` 默认 false（context 只含 text/thinking，primary 命中本身仍按原 block_type 返回）。
- **Q5（BM25 索引覆盖面）**：B — 仅索引 `embed_eligible=true` 的 block content，与 HNSW 召回面对称。tool_use / tool_result 的 content 不进 BM25 索引。
- **Q6（共享 helper 落点 / zero shared state 边界）**：纯函数（`rrf_contribution`、`freshness_score`、`RRF_K`、`RRF_SCALE`）抽到新文件 `src/pipeline/ranking.rs`；scorer 各自独立（memories 的 `score_candidates_hybrid_rrf` 在 `pipeline/retrieve.rs` 不动职责，仅替换内部调用；transcripts 的 `score_candidates` 在新文件 `src/pipeline/transcript_recall.rs`）。

## Architecture

```
POST /transcripts/search
        │
        ▼
TranscriptService::search(tenant, query, filters, limit, opts)
        │
        ├─ 空 query → recent_conversation_messages（既有路径） → 退化为按时间序的 windows
        │
        └─ 非空 query → 三路并行候选：
            ├─ HNSW (Arc<VectorIndex>::search)             → semantic_ranks
            ├─ BM25 (repo::bm25_transcript_candidates)     → lexical_ranks   ◄── 新
            └─ (可选) anchor_session_id 注入候选            → 直接 SQL 抽该 session 全部 embed-eligible 命中
                       │
                       ▼
              Union 候选 id → fetch_conversation_messages_by_ids 拉全文
                       │
                       ▼
              transcript_recall::score_candidates(candidates, lexical_ranks, semantic_ranks, opts)
                       │
                       │  对每个 candidate:
                       │    score = rrf(lex_rank) + rrf(sem_rank)
                       │          + session_co_occurrence_bonus(this, all)
                       │          + anchor_session_bonus(this.session_id, opts.anchor)
                       │          + freshness_score(newest, this.created_at)
                       │
                       ▼
              排序 desc + filter (role/block_type/time/session) → top-N 作为 primaries
                       │
                       ▼
              context hydration（同 session ±k blocks，默认仅 text/thinking）
                       │
                       ▼
              window merge（同 session 重叠/相邻合并；window_score = max(primary)）
                       │
                       ▼
              SearchResponse { windows: Vec<TranscriptWindow> }
```

**与 memories 完全隔离的边界**：
- 数据：`transcripts_fts_dirty`、`fts_main_conversation_messages` 索引、`bm25_transcript_candidates`、`transcript_recall::score_candidates`、`TranscriptService::search`——独立。
- 配置：FTS 索引建在 `embed_eligible=true` 子集；与 memories 的 `(summary, content)` 全表索引规则不同。
- 共享：仅纯函数 `pipeline::ranking::{rrf_contribution, freshness_score, RRF_K, RRF_SCALE}`——无可变状态。

## Schema

**不新建 schema 文件。** DuckDB FTS extension 已由 `005_fts.sql` install/load 一次（per-process，长连接）。`fts_main_conversation_messages` 索引由 `ensure_transcript_fts_index_fresh()` 在第一次 BM25 查询时（或 dirty flag 翻起后）lazy 创建——与 memories 同模式（`005_fts.sql` 注释明确说"index 是 lazy build"）。

留给 schema/migration 文件的物理 shape 只有：表、列、索引（声明式）。运行时 lazy build 不属于 schema。

## Storage 改动

### `DuckDbRepository` 新字段

`src/storage/duckdb.rs` 在结构体里加：

```rust
pub struct DuckDbRepository {
    conn: Arc<Mutex<Connection>>,
    vector_index: Arc<RwLock<Option<Arc<VectorIndex>>>>,
    transcript_job_provider: Arc<RwLock<Option<String>>>,
    fts_dirty: Arc<AtomicBool>,                    // 既有：memories FTS
    transcripts_fts_dirty: Arc<AtomicBool>,        // 新：transcripts FTS，零共享
}
```

`open()` 初始化为 `Arc::new(AtomicBool::new(true))`——强制冷启动后首次 BM25 查询触发一次构建（避免 stale 索引漏命中）。

`set_fts_dirty()` 等 memory-side helper 不动。新增对称的 `set_transcripts_fts_dirty()` `pub(crate)` 接口供 transcript_repo 调。

### `transcript_repo.rs` 新增

```rust
impl DuckDbRepository {
    /// BM25 候选拉取。仅检索 `embed_eligible=true` 的 block content。
    /// 返回按 BM25 score desc 排序的 `ConversationMessage` 列表。
    pub async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> { /* see plan */ }

    /// Lazy 重建 FTS 索引。仅在 transcripts_fts_dirty == true 时实际跑 PRAGMA。
    /// drop-then-create 模式（与 memories 的 ensure_fts_index_fresh 一致）。
    fn ensure_transcript_fts_index_fresh(&self) -> Result<(), StorageError> { /* see plan */ }

    /// 拉取 primary 周围 ±k 个 block。一次 SQL（LATERAL 子查询）。
    /// 时间序排序；返回 (before, primary, after) 三段。
    pub async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> { /* see plan */ }
}
```

`create_conversation_message` 末尾的"成功插入分支"翻 `transcripts_fts_dirty = true`（仅当 `INSERT OR IGNORE` 实际写入了一行；冲突时不翻）。这是唯一的写翻转点——transcript 行不更新不删除，所以无其他 dirty 触发位置。

### FTS 索引创建——含 `where` 谓词的可行性风险

计划用：

```sql
PRAGMA create_fts_index(
    'conversation_messages',
    'message_block_id',
    'content',
    where := 'embed_eligible = true'
);
```

让索引天然只含 embed-eligible 行。**DuckDB FTS extension 的 `where` 命名参数支持需要在实现时用真实环境验证**（v1.x 文档列出但版本特定）。

**Fallback 方案**（在 plan 阶段写入 task 验收清单）：若该参数未支持，建索引到全表，`bm25_transcript_candidates` 的 SQL 加 `AND embed_eligible = true` 后过滤。索引体积膨胀 1.5×–3×（tool_use 短，tool_result 偶有大 content），但正确性不变；风险可控。

## Ranking 改动

### 新建 `src/pipeline/ranking.rs`

```rust
//! Pure ranking helpers shared by the memories and transcripts retrieval
//! pipelines. No pipeline-specific types — only rank/timestamp arithmetic.
//! Adding code here is a deliberate decision to share math; do NOT add types
//! that name memory or transcript domain concepts.

pub const RRF_K: usize = 60;
pub const RRF_SCALE: f64 = 1000.0;

/// Reciprocal Rank Fusion contribution for a single ranked appearance:
/// `RRF(rank) = SCALE / (K + rank)`. Returns i64 after `.round()` so it
/// composes with the existing additive integer scoring convention.
pub fn rrf_contribution(rank: usize) -> i64 {
    ((RRF_SCALE / (RRF_K as f64 + rank as f64)).round()) as i64
}

/// Freshness score relative to the newest timestamp in a candidate pool.
/// Identical formula to the existing memories implementation; extracted
/// so transcripts can use the same curve.
pub fn freshness_score(newest: u128, current: u128) -> i64 {
    /* port body verbatim from retrieve.rs */
}
```

unit tests at file bottom：`rrf_contribution_rank_1_is_16`（保护 magnitude 16）、`rrf_contribution_decreases_with_rank`、`freshness_at_newest_is_max`、`freshness_decays_with_age`。

### `pipeline/retrieve.rs` 重构（zero behavior change）

- 删除文件内的 `RRF_K`、`RRF_SCALE` 常量定义、`freshness_score` 函数定义
- `score_candidates_hybrid_rrf` 内部调用从 `((rrf_lex + rrf_sem) * RRF_SCALE).round() as i64` 改为：
  ```rust
  let rrf_lex = lexical_ranks.get(&memory.memory_id).map(|&r| rrf_contribution(r)).unwrap_or(0);
  let rrf_sem = semantic_ranks.get(&memory.memory_id).map(|&r| rrf_contribution(r)).unwrap_or(0);
  score += rrf_lex + rrf_sem;
  ```
- 全部调用 `freshness_score` 处加 `use crate::pipeline::ranking::freshness_score;`

**验收**：`cargo test --test search_api`、`cargo test --test bm25_search`、`cargo test --test hybrid_search`、`cargo test --lib pipeline::retrieve` 全部仍然通过，分数比对 fixture 不变。

### 新建 `src/pipeline/transcript_recall.rs`

完整的 transcript scorer（pure function）+ window merge 算法。

```rust
//! Transcript candidate scoring and window assembly. Separate from
//! pipeline/retrieve.rs (memories scoring) — zero shared state. Shares
//! only the pure helpers in pipeline/ranking.rs.

use std::collections::{HashMap, HashSet};
use crate::domain::ConversationMessage;
use crate::pipeline::ranking::{freshness_score, rrf_contribution};

// ── Scoring magnitude (tuneable; documented next to constants)
pub const SESSION_COOCC_PER_SIBLING: i64 = 3;     // +3 per session-sibling beyond self
pub const SESSION_COOCC_CAP_SIBLINGS: i64 = 4;    // cap at 4 siblings → max +12
pub const ANCHOR_SESSION_BONUS: i64 = 20;         // > RRF top contribution (~16)

#[derive(Debug, Clone, Copy)]
pub struct ScoringOpts<'a> {
    pub anchor_session_id: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct ScoredBlock {
    pub message: ConversationMessage,
    pub score: i64,
}

pub fn score_candidates(
    candidates: Vec<ConversationMessage>,
    lexical_ranks: &HashMap<String, usize>,
    semantic_ranks: &HashMap<String, usize>,
    opts: ScoringOpts<'_>,
) -> Vec<ScoredBlock> { /* see plan */ }

// ── Window assembly
pub struct PrimaryWithContext {
    pub primary: ScoredBlock,
    pub before: Vec<ConversationMessage>,
    pub after: Vec<ConversationMessage>,
}

pub struct MergedWindow {
    pub session_id: Option<String>,
    pub blocks: Vec<ConversationMessage>,         // 时间序，去重
    pub primary_ids: Vec<String>,
    pub primary_scores: HashMap<String, i64>,
    pub score: i64,                                // = max(primary_scores.values())
}

/// 把 (primary, before, after) 三元组合并成 windows。同 session 内 context 时间区间
/// 重叠或相邻的 primaries 合并为一个 window。
pub fn merge_windows(items: Vec<PrimaryWithContext>) -> Vec<MergedWindow> { /* see plan */ }
```

### Magnitude 表（决策日志）

| 信号 | 满分量级 | 触发条件 |
|---|---|---|
| RRF (BM25) | ~16 | rank 1；rank 60 时 ~8；rank 1000 时 ~1 |
| RRF (semantic) | ~16 | 同上 |
| Session co-occurrence | +12 | 候选集里同 session 4+ 个同伴 |
| Anchor session boost | +20 | 显式传 `anchor_session_id` 且匹配 |
| Freshness | 0 ~ ~10 | 越新越高，log decay；与 memories 同曲线 |
| **典型总分** | **30–80** | 强 RRF + 同会话聚集 + 锚点 + 新近 |

**Magnitude rationale**：
- RRF 是基础——topical 相关是必要条件
- session 聚集和 anchor 各自能"抵一个 RRF 命中"——影响排序但不主导
- recency 最弱——只是 tie-break 性质
- 全部参数定义为 `pub const`，调起来一行改动

## Service 改动

`src/service/transcript_service.rs::search` 重写：

- **空 query 路径**：候选集来自 `recent_conversation_messages`，所有候选得分 0；filter / hydration / window merge 逻辑**完全相同**（每条 recent block 仍然 hydrate ±k context，同 session 重叠合并）。最终响应仍是 `{ windows }`，按时间序排（score 全部相等时）。这保证响应形态与非空 query 一致。
- **非空 query 三路候选**：
  1. `self.index.search(&q_vec, oversample)` → `semantic_ranks`
  2. `self.repo.bm25_transcript_candidates(tenant, query, oversample)` → `lexical_ranks`
  3. 若 `opts.anchor_session_id.is_some()`：`SELECT message_block_id FROM conversation_messages WHERE tenant=? AND session_id=? AND embed_eligible=true ORDER BY created_at DESC LIMIT oversample` 注入候选集。**两个机制要分清**：(a) anchor **注入**确保该 session 的 block 进入候选池（即使无任何 topical 命中）；(b) anchor **bonus** 是 `score_candidates` 内对所有候选检查 `this.session_id == opts.anchor` 后加的 `+20`——无论该候选怎么进的池。anchor 注入的候选 RRF 部分为 0（既无 lex rank 也无 sem rank），故仅靠 anchor bonus + freshness + 可能的 co-occurrence 评分。设计意图：让 anchor session 的中等强度命中浮起来，但不主导 ranking（被 RRF 32 + freshness 的强 topical 命中压住）。
- Union 候选 ids → `fetch_conversation_messages_by_ids` 拉全文
- `transcript_recall::score_candidates(candidates, &lexical_ranks, &semantic_ranks, opts)` → `Vec<ScoredBlock>`
- filter（既有：role/block_type/time_from/time_to/session_id）
- 取 top-`limit` 作为 primaries
- 对每个 primary 调 `repo.context_window_for_block(...)` → `Vec<PrimaryWithContext>`
- `transcript_recall::merge_windows(...)` → `Vec<MergedWindow>`
- 返回（HTTP 层把 `MergedWindow` 序列化为 `TranscriptWindow` DTO）

`limit` 在 service 层 cap 到 100（防御 N² window 合并的 pathological 情况）；超出由 caller 用 `GET /transcripts?session_id=…` 拉整段。

## HTTP 改动

`src/http/transcripts.rs::SearchRequest` 加 3 个可选字段：

```rust
pub anchor_session_id: Option<String>,
pub context_window: Option<usize>,                     // default 2, cap 10
#[serde(default)]
pub include_tool_blocks_in_context: bool,
```

`SearchResponse` 改为：

```rust
pub struct SearchResponse {
    pub windows: Vec<TranscriptWindow>,
}

pub struct TranscriptWindow {
    pub session_id: Option<String>,
    pub blocks: Vec<TranscriptWindowBlock>,
    pub primary_ids: Vec<String>,
    pub score: i64,
}

pub struct TranscriptWindowBlock {
    pub message_block_id: String,
    pub session_id: Option<String>,
    pub line_number: u64,
    pub block_index: u32,
    pub role: MessageRole,
    pub block_type: BlockType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub created_at: String,
    pub is_primary: bool,
    pub primary_score: Option<i64>,
}
```

**Breaking change 说明**：v1 `{hits}` 形态被 `{windows}` 替换。已知调用方仅两处 in-tree 测试（`tests/conversation_archive.rs::post_transcripts_search_filters_by_role_and_block_type`、`tests/integration_claude_code.rs::end_to_end_mine_then_search_then_get`），改 assertion 即可。无外部调用方（MCP 不暴露、`mem mine` 不调）。文档/README 一并更新。

`POST /transcripts/messages`、`GET /transcripts?session_id=…` 不动。

## Testing Strategy

### Unit (`src/pipeline/ranking.rs::tests`)

1. `rrf_contribution_rank_1_is_16` — `rrf_contribution(1) == 16`，保护 magnitude
2. `rrf_contribution_decreases_with_rank`
3. `freshness_at_newest_is_max`
4. `freshness_decays_with_age`

### Unit (`src/pipeline/transcript_recall.rs::tests`)

Scoring：
1. `rrf_only_no_session_no_anchor` — 单 candidate，仅 lex/sem rank → 总分 == `rrf(lex) + rrf(sem) + freshness`
2. `session_cooccurrence_caps_at_4` — 同 session 6 候选 → 每个 +12（不 +18）
3. `anchor_session_boost_applies_only_when_match`
4. `magnitude_invariant_anchor_dominates_cooccurrence` — `ANCHOR_SESSION_BONUS > SESSION_COOCC_PER_SIBLING * SESSION_COOCC_CAP_SIBLINGS`，常量改坏时红灯
5. `freshness_decays_old_below_new_at_equal_rrf`

Window merge：
6. `single_primary_no_overlap_one_window`
7. `two_primaries_same_session_overlapping_merge` — primary_ids.len() == 2
8. `two_primaries_different_session_dont_merge`
9. `two_primaries_same_session_far_apart_dont_merge`
10. `merged_window_score_is_max_of_primary_scores`
11. `merged_window_blocks_dedup_and_time_sorted`

### Integration (`tests/transcript_recall.rs` 新文件)

1. `bm25_only_candidate_appears_in_results` — fixture 中 semantic 找不到的强 lexical 命中必须出现
2. `hnsw_only_candidate_appears_in_results` — 反向：BM25 找不到的语义命中也召回
3. `tool_blocks_excluded_from_bm25_index` — tool_use input JSON 中独特词搜不到（验证 embed_eligible 过滤）
4. `anchor_session_boost_lifts_matching_session_to_top` — seed 两个 session 各 1 弱命中，传 anchor → window[0] 在 anchor session
5. `context_window_includes_neighboring_text_blocks` — 5 个连续 text，命中中间 → window 含 5 个 block
6. `context_window_excludes_tool_blocks_by_default` — text/tool_use/text/tool_result/text，命中第 3 个 → 默认 context 只含 text
7. `context_window_includes_tool_blocks_when_opted_in` — 同 6 但 `include_tool_blocks_in_context=true` → 5 个全在
8. `windows_merge_when_primaries_share_session_and_overlap` — primary_ids.len() == 2
9. `empty_query_returns_recent_time_windows` — 空 query 兼容路径
10. `mem_repair_unaffected_by_fts_dirty` — 翻转 transcripts_fts_dirty 不影响 `mem repair --check` 输出

### Memories regression (`tests/search_api.rs` + `tests/bm25_search.rs` + `tests/hybrid_search.rs`)

不新增；既有测试在 `pipeline/retrieve.rs` 重构后必须**全部通过、分数 fixture 不变**——这是 zero-behavior-change 重构的验收。

## Concerns to Confirm Before Implementing

1. **DuckDB FTS `where := '...'` 谓词支持** — 第一个 plan 任务先用真实环境验证。若不支持，转用 fallback（全表索引 + SQL 后过滤）；spec 不重写、plan 文档化。
2. **`pragma drop_fts_index` 在 transcripts 表上的语义** — memories 那边已用，但 transcripts 表可能不存在该 PRAGMA 的支持快路径。实现时先跑一次 drop-then-create 看错误码。
3. **Memories 重构 zero-behavior-change 的 fixture 校验** — `cargo test --test search_api -q` 的输出 fixture 在重构前后逐字节比对（plan 阶段加显式校验步骤）。
4. **Window merge 的 N² 边角** — `limit` cap 到 100 在 service 层；plan 阶段加 cap 测试。
5. **anchor_session_id 的安全验收** — 调用方传任意 session_id，服务端不校验该 session 是否真的存在；如果不存在，候选集就是空的 anchor 注入路径——所有 anchor bonus 自动失效，无负面副作用。这是设计选择（caller 自负），plan 阶段在测试中明示。

## Out of Scope (this PR)

- MCP transcript 工具暴露
- Memories + transcripts 跨管道混合搜索
- Per-block_type / per-role 加权
- transcript retention / TTL
- `mem wake-up` 联动 transcripts
- Search response 分页（`offset`/`cursor`）
- 任何 metric / observability 改动
- 优化 N² window merge（`limit≤100` 完全可接受）

## Verification Checklist (pre-merge)

- `cargo test -q` 在分支上全绿（除了 conversation-archive 已记录的 8 个 EmbedAnything-runtime 依赖失败——本设计不引入新失败）
- `cargo fmt --check` 干净
- `cargo clippy --all-targets -- -D warnings` 干净
- `cargo build --release` 干净
- 手动冒烟：
  1. `cargo run -- serve`
  2. `cargo run -- mine ~/.claude/projects/<some-project>/<some-session>.jsonl --agent claude-code`
  3. `curl -s -X POST localhost:3000/transcripts/search -H 'content-type: application/json' -d '{"query":"vector index", "tenant":"local", "limit":3, "context_window":2}' | jq` 返回 `{ windows: [...] }` 且每个 window 含 primary_ids、blocks 时间序
  4. 加 `"anchor_session_id":"<某 session>"` 验证该 session 排到前列
  5. 加 `"include_tool_blocks_in_context":true` 验证 context 含 tool_* blocks
- Memories 侧：`cargo test --test search_api --test bm25_search --test hybrid_search` 全过；分数与重构前逐字节一致

## References

- `docs/superpowers/specs/2026-04-30-conversation-archive-design.md` — 上一版 transcript-archive 设计；本设计在其搜索路径上加质
- `docs/superpowers/plans/2026-04-30-conversation-archive.md` — 上一版实现计划；helper 抽取的"是去重不是耦合"原则在那边的 Task 19 follow-up 中被识别
- `db/schema/005_fts.sql` — DuckDB FTS extension install/load；本设计共用
- `src/pipeline/retrieve.rs` — memories scorer，本设计的 RRF 公式来源
- `src/storage/duckdb.rs::bm25_candidates` + `ensure_fts_index_fresh` — memories BM25 模式，本设计镜像
- `src/service/transcript_service.rs::search` — 当前 transcript search 实现，本设计的修改起点
- `CLAUDE.md` "Architecture (non-obvious bits)" 中 transcript-archive 段——本设计严守"两条管道 zero shared state，仅纯 helper 共享"
