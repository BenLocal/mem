# MemPalace LongMemEval Parity Bench — Design

> Apple-to-apple benchmark vs mempalace 的 LongMemEval 公开基线（Recall@5 = 0.966 raw / 0.894 rooms / AAAK 跳过）。Rust port；in-process；3 rung 对应 mempalace `raw / rooms / full` 三档；输出 mirror mempalace 的 `results_*.json` shape 加 `mem_` 前缀；用户预下载数据集走 env-var；不在 CI 跑（与 mempalace 一致都是 manual decision tool）。

## Summary

Wave 5 的 recall ablation bench 是**内部 ablation**（10 rung × FakeEmbeddingProvider × 自家 synthetic fixture），不能与外部系统对比——所有数字都在 mem 自家坐标系里。本设计落地 mem 的第一份**对外横向 benchmark**：跑 mempalace `develop/benchmarks/longmemeval_bench.py` 同款 dataset（LongMemEval Standard）、同款评测协议（per-Q ephemeral corpus + top-K retrieval + Recall@5/10 + NDCG@10），mem 端用 production embedding stack（EmbedAnything/Qwen3）+ 三档对应的 `ScoringOpts` 配置。

输出 mirror mempalace `results_<bench>_<mode>_<timestamp>.json` 内部 shape，文件名加 `mem_` 前缀避免与 mempalace 的 results 混淆。reader 可以直接 `jq '.aggregate.recall_any_at_5' results_mem_longmemeval_raw_*.json` 与 mempalace README 的 `0.966` 比对。

mempalace 的 7 个 mode 中，3 个有 mem 干净 analog（raw / rooms / full），AAAK 与 llmrerank 跳过（mem 无对应实现，强行 port 会引入 apple-to-orange 比对噪声）。

## Goals

- mem 跑 LongMemEval 完整 500-question 集，3 rung（raw / rooms / full equivalents），每 rung 一份 results JSON 写盘
- JSON 内部 shape 镜像 mempalace 公开 `results_*.json`，文件名 `results_mem_longmemeval_<mode>_<unix_ts>.json`
- Per-Q 独立 corpus（不跨 Q 共享 DB state），ingest once + 3-rung re-rank（节约 2/3 embedding wall-clock）
- 用 mem production embedding 提供器（EmbedAnything/Qwen3），不用 FakeEmbeddingProvider —— 真分对比
- `MEM_LONGMEMEVAL_PATH` env-var 指向 `longmemeval_s_cleaned.json`，env 缺失则测试 silently skip
- `tests/mempalace_bench.rs::longmemeval` `#[ignore]`'d，`cargo test --test mempalace_bench longmemeval -- --ignored --nocapture` 触发
- `src/pipeline/eval_metrics.rs` 扩 `recall_any_at_k` + `recall_all_at_k` 两个 binary indicator 函数（mempalace 报告的就是这两个）
- Stdout pretty table 一并打印 mempalace 公开基线作 side-by-side 对比；footer 常驻 embedding-model parity caveat

## Non-Goals

- 不 port mempalace 的 5 个 bench；只做 LongMemEval（其余 LoCoMo / ConvoMem / MemBench / Mine 留作后续 wave）
- 不实现 mempalace AAAK / llmrerank / hybrid_v2 / hybrid_v3 模式（mem 无对应；apple-to-orange 不做）
- 不内置 dataset 下载（reqwest / HF Hub adapter）—— 与 mempalace 一致依赖用户预下载
- 不在 CI 跑（manual decision tool；wall-clock ~3-4 小时）
- 不引入 Python 工具链（Rust port 路径锁定）
- 不验证 mem HTTP 表面（in-process bench；HTTP 由现有 integration 测试覆盖）
- 不做 latency benchmark（mempalace 报的也只是 quality 数字 NDCG/Recall）
- 不写跨 process 比较脚本（reader 自行 `jq` 后处理，bench 不做工具）
- 不为 bench 加 retention / 历史 trace（manual 跑、写盘、比对、结束）
- 不优化 mem ranker / 调参（本设计只测量；调参看到数据后再决定）

## Decisions (resolved during brainstorming)

- **Q1（harness 语言 / locality）**：B — Rust port，单语言 repo。Mempalace 的 metric 公式（dcg / ndcg / recall_any / recall_all）只 6 行 Python，等价容易 verify；不引入 Python 工具链。代价：Rust port 引入 drift 风险，由 hand-computed reference test 兜底。
- **Q2（bench 覆盖面）**：B — 仅 LongMemEval。文档最完整、有 scoreboard、retrieval 形态最简单（per-Q 找 right session）；其余 mempalace bench 在后续 wave 增量。
- **Q3（dataset 获取 + CI 集成）**：A — env-var 预下载（`MEM_LONGMEMEVAL_PATH=...`），无 synthetic 兜底，不在 CI 跑。与 mempalace 操作流对齐，与 Wave 5 `real_recall_bench` 同款。
- **Q4（mode 覆盖）**：B — 3 rung 对应 mempalace 三个有效基线（raw / rooms / full equivalents）。AAAK 与 llmrerank 跳过——mem 无 analog。
- **Q5（process model）**：A — In-process direct（DuckDbRepository + VectorIndex 直调）。Mempalace in-process，mem in-process 才同条件比；ingest 不走 HTTP；启动快。
- **Q6（output format）**：A — Mirror mempalace JSON shape；文件名加 `mem_` 前缀。`jq` 后处理脚本对两份文件通用。
- **Q7（metric impl）**：A — 扩展 `src/pipeline/eval_metrics.rs` 加 `recall_any_at_k` + `recall_all_at_k`（mempalace scoreboard 报的就是 binary indicator）。Wave 5 ablation bench 后续也能用。

## Architecture

```
                +----------------------------------------------+
 dataset layer  |  MEM_LONGMEMEVAL_PATH=/path/to/.../*.json    |
                |  -> JSON file deserialized once into          |
                |     Vec<LongMemEvalQuestion>                  |
                +----------------------------------------------+
                                   |
                                   v
                +----------------------------------------------+
                |  Per-question loop (500 questions, --limit N)|
                |  +-----------------------------------------+ |
                |  | TempDir + DuckDbRepository::open        | |
                |  | VectorIndex::new_in_memory              | |
                |  | ingest corpus ONCE                       | |
                |  |   - embed via EmbedAnything provider    | |
                |  |   - create_conversation_message          | |
                |  |   - index.upsert(block_id, vec)          | |
                |  | for each of 3 rungs:                    | |
                |  |   ranked = retrieve(question, rung_cfg) | |
                |  |   metrics_per_rung[rung][q] = score(..)  | |
                |  | drop DB / index                          | |
                |  +-----------------------------------------+ |
                +----------------------------------------------+
                                   |
                                   v
                +----------------------------------------------+
                |  Aggregate across N Qs -> 3 RungReport       |
                |  Write 3 JSON files:                         |
                |    target/bench-out/                          |
                |      results_mem_longmemeval_raw_<ts>.json   |
                |      results_mem_longmemeval_rooms_<ts>.json |
                |      results_mem_longmemeval_full_<ts>.json  |
                |  + comparison stdout table with mempalace    |
                |    baselines side-by-side                     |
                +----------------------------------------------+
```

**Key decisions：**

1. **Per-question fresh DB.** LongMemEval 每 Q 有独立 `haystack_sessions` 集；跨 Q 共享 DB 会导致一个 Q 的 correct session 出现在另一个 Q 的 haystack（污染）。每 Q 独立 `TempDir` + `DuckDbRepository::open` + `VectorIndex::new_in_memory`，drop 后清理。

2. **Ingest once per Q + re-rank under 3 rungs.** 同一 Q 的 corpus 在 3 rung 间相同；只 `ScoringOpts` 不同 -> 不需要 re-embed -> 节约 2/3 wall-clock。Wall-clock 估计：500 Q × (~10 s embed + ~3 × 0.5 s rerank) ≈ ~95 min（vs 朴素 4× = ~6 hours）。

3. **复用 Wave 5 plumbing.** `tests/bench/runner.rs` 已有 per-rung TempDir + ingest + score + `oracle_rerank` + `pretty_table`；本 spec 抽相同模式到 `tests/bench/longmemeval.rs`（独立兄弟 module，不强抽公共代码）。

4. **真 embedding 模型（不是 FakeEmbeddingProvider）。** Wave 5 用 fake 因为 ablation 不在乎绝对分；本 wave vs mempalace `0.966` 必须用 production embedder。**Embedding 模型差异（mem=Qwen3 vs mempalace=MiniLM-L6 384-dim）不可消** —— spec 明示，bench output footer 常驻警示。

## Components

### Dataset Loader（`tests/bench/longmemeval_dataset.rs`）

LongMemEval JSON 结构（每个 question 一条 entry）：

```json
{
  "question_id": "lme_q_001",
  "question": "Tell me about my favorite hiking trail in Yosemite",
  "haystack_sessions": [
    {
      "session_id": "sess_2024_03_15",
      "started_at": "2024-03-15T...",
      "turns": [
        { "role": "user", "content": "..." },
        { "role": "assistant", "content": "..." }
      ]
    }
  ],
  "answer_session_ids": ["sess_2024_06_22", "sess_2024_09_03"],
  "question_date": "2024-12-01T..."
}
```

**Loader 行为：**
- `MEM_LONGMEMEVAL_PATH` 缺失 -> `eprintln!("MEM_LONGMEMEVAL_PATH not set; skipping"); return;`（测试 pass）
- `serde_json::from_slice` 解析整个文件；schema 不匹配 -> panic with clear message
- 缺 `question_id` -> fallback `format!("lme_q_{:04}", index)`（plan 阶段先 inspect 一份 sample 文件确认；如果 100% 都有就删 fallback）
- 缺 `started_at` -> fallback to deterministic timestamp（用 question_id 做 seed）
- 返回 `Vec<LongMemEvalQuestion>`，下游 runner 消费

**JSON deserialize types：**

```rust
pub struct LongMemEvalQuestion {
    pub question_id: String,
    pub question: String,
    pub haystack_sessions: Vec<LongMemEvalSession>,
    pub answer_session_ids: Vec<String>,
    pub question_date: Option<String>,
}

pub struct LongMemEvalSession {
    pub session_id: String,
    pub started_at: String,
    pub turns: Vec<LongMemEvalTurn>,
}

pub struct LongMemEvalTurn {
    pub role: String,    // "user" | "assistant" | maybe "system"
    pub content: String,
}
```

### Ingest Mapping（`tests/bench/longmemeval.rs::ingest_corpus`）

每个 haystack_session -> 一组 `ConversationMessage` 行：

```rust
ConversationMessage {
    message_block_id: format!("{}_{}_{}", question_id, session.session_id, turn_idx),
    tenant: "bench".to_string(),
    session_id: Some(session.session_id.clone()),  // <- 用 LongMemEval 自带 ID
    role: parse_role(&turn.role),
    block_type: BlockType::Text,
    content: turn.content.clone(),
    embed_eligible: true,
    created_at: format!("{:020}", session_started_ms + turn_idx as u64 * 60_000),
    // ... rest of required fields filled with bench defaults
}
```

每条消息 embed via `EmbedAnythingEmbeddingProvider`（默认 production 配置；不是 FakeEmbeddingProvider）；对 embed 后的向量用 `index.upsert(&block_id, &vec)` 直接放入 `VectorIndex`。

### Rung Definitions

```rust
pub struct LongMemEvalRung {
    pub rung_id: &'static str,
    pub mempalace_label: &'static str,
    pub source: SourceMix,
    pub disable_session_cooc: bool,
    pub disable_anchor: bool,
    pub disable_freshness: bool,
}

pub const RUNGS: &[LongMemEvalRung] = &[
    LongMemEvalRung {
        rung_id: "longmemeval_raw",
        mempalace_label: "raw",
        source: SourceMix::HnswOnly,
        disable_session_cooc: true,
        disable_anchor: true,
        disable_freshness: true,
    },
    LongMemEvalRung {
        rung_id: "longmemeval_rooms",
        mempalace_label: "rooms",
        source: SourceMix::HnswOnly,
        disable_session_cooc: false,  // session_cooc <-> mempalace's room boost
        disable_anchor: true,
        disable_freshness: true,
    },
    LongMemEvalRung {
        rung_id: "longmemeval_full",
        mempalace_label: "full",
        source: SourceMix::Both,       // BM25 + HNSW
        disable_session_cooc: false,
        disable_anchor: false,
        disable_freshness: false,
    },
];
```

`SourceMix`、`disable_*` 字段沿用 Wave 5 ScoringOpts 设计（`src/pipeline/transcript_recall.rs`）。

### Retrieval Contract

- Query string = `question.question`
- Anchor session id = `None`（LongMemEval 不带 anchor 概念；rung 配置已 `disable_anchor=true` 在 raw / rooms 档；full 档也 None，bonus 自然为 0）
- Top-K candidates = 50（mempalace 默认）；最终 metric 在 top-5 / top-10 cuts
- Run = 排序后 deduped session_ids（每 session 取最高 score block 代表，保留前 K 个 session）—— 与 Wave 5 runner 同款投影

### Eval Metrics Extension（`src/pipeline/eval_metrics.rs`）

新增 2 个 pub fn：

```rust
/// Mempalace-style binary recall: 1.0 if top-K contains >=1 relevant id, else 0.0.
/// Call this for "did we find at least one of the answer sessions" tasks.
pub fn recall_any_at_k<I: Eq + Hash>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    if qrels.is_empty() { return 0.0; }
    if run.iter().take(k).any(|id| qrels.contains(id)) { 1.0 } else { 0.0 }
}

/// Mempalace-style binary recall: 1.0 if top-K contains ALL relevant ids, else 0.0.
/// Stricter; useful for multi-hop tasks where partial recall is insufficient.
pub fn recall_all_at_k<I: Eq + Hash>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    if qrels.is_empty() { return 0.0; }
    let top_k: HashSet<&I> = run.iter().take(k).collect();
    if qrels.iter().all(|id| top_k.contains(id)) { 1.0 } else { 0.0 }
}
```

每函数 4 个 hand-computed reference test（hit / miss / partial / empty-qrels）。

### Output Format

每 rung 一份 JSON：

```json
{
  "benchmark": "longmemeval",
  "mode": "raw",
  "system": "mem",
  "embedding_model": "EmbedAnything/Qwen3",
  "system_version": "<git short SHA>",
  "timestamp_ms": 1730000000000,
  "limit": 500,
  "aggregate": {
    "recall_any_at_5": 0.xxx,
    "recall_any_at_10": 0.xxx,
    "recall_all_at_5": 0.xxx,
    "recall_all_at_10": 0.xxx,
    "ndcg_at_10": 0.xxx
  },
  "per_question": [
    {
      "question_id": "lme_q_001",
      "recall_any_at_5": 0.0,
      "recall_any_at_10": 1.0,
      "recall_all_at_5": 0.0,
      "recall_all_at_10": 0.5,
      "ndcg_at_10": 0.6309,
      "ranked_session_ids": ["sess_xxx"],
      "answer_session_ids": ["sess_yyy"],
      "elapsed_ms": 320
    }
  ]
}
```

**Plan 阶段第一步**：fetch 一份 mempalace `results_mempal_*.jsonl` sample 文件 reverse-engineer 实际 key 名 / 嵌套结构 / 字段顺序。如果 mempalace 实际 schema 与本 spec 描述差异（比如 mempalace 用 `recall_any@5` 字符串 key 含 `@`），plan 调整为 mempalace 同款。

### Stdout Pretty Table

```
=== Mem vs MemPalace LongMemEval (500 questions, run <unix_ts>) ===
                    R@5(any) R@10(any) NDCG@10  | mempalace baseline
longmemeval-raw       0.xxx     0.xxx    0.xxx  | mempalace raw    = 0.966 R@5
longmemeval-rooms     0.xxx     0.xxx    0.xxx  | mempalace rooms  = 0.894 R@5
longmemeval-full      0.xxx     0.xxx    0.xxx  | mempalace full   = (varies)

! Embedding-model parity caveat: mem uses EmbedAnything/Qwen3 while
  mempalace uses all-MiniLM-L6-v2 (384-dim). The Δ between rungs IS
  reliable; absolute Δ vs mempalace baselines includes both ranking
  and embedding-model contributions.
```

### Test Harness（`tests/mempalace_bench.rs`）

```rust
mod bench;
use bench::longmemeval::{run_longmemeval_bench, RungReport};
use bench::longmemeval_dataset::load_from_env_or_skip;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "external dataset; set MEM_LONGMEMEVAL_PATH=..."]
async fn longmemeval() {
    let questions = match load_from_env_or_skip().await {
        Some(qs) => qs,
        None => { eprintln!("MEM_LONGMEMEVAL_PATH not set; skipping"); return; }
    };
    let limit = std::env::var("MEM_LONGMEMEVAL_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let questions: Vec<_> = questions.into_iter().take(limit).collect();
    let reports = run_longmemeval_bench(questions).await;
    print_comparison_table(&reports);
    write_per_rung_json(&reports).expect("write json");
    // No assertions - informational only (manual decision tool).
}
```

## Risks & Mitigations

1. **Embedding 模型差异不可消** —— mem=Qwen3 vs mempalace=MiniLM-L6 384-dim。绝对分数差异中，无法分离 ranking 算法贡献 vs 模型贡献。**Mitigation**：spec 明示，bench output footer 常驻警示，README 用同样措辞。Reader 把数字当作"mem 整套 stack 的成绩"，不是"mem ranking 算法 vs mempalace ranking 算法"的纯净对比。

2. **Wall-clock ~95 min × 3 rung re-rank ≈ 1.5-3 小时** —— manual decision tool，与 mempalace 操作流对齐；spec 注明可用 `MEM_LONGMEMEVAL_LIMIT=50` 跑 smoke。

3. **Mempalace 输出 schema 未完全文档化** —— Plan 阶段第一 task fetch 一份 mempalace sample `results_*.json` 反推精确 schema。如发现 spec 描述与实际差异，plan 调整为 mempalace 实际 shape。

4. **LongMemEval 数据集 license** —— 上游研究用途，下载 / 引用规则随时间变化；spec 不内嵌数据，env-var 路径让用户自己处理 license + 下载。

5. **Per-Q DB churn cost** —— 500 次 `TempDir::new` + `DuckDbRepository::open` + `VectorIndex::new_in_memory` 创建/销毁，单次 ~50-200ms；500 次 = 25-100s 净开销。可接受。如果 plan 实施时发现 bottleneck，优化用 `":memory:"` DuckDB 替代 file-backed temp（in-mem DuckDB 没 file lock 开销、即开即用）。

6. **Question ID 缺失风险** —— LongMemEval JSON 是否每条都有 `question_id`？Plan 阶段第一 task `--limit 5` 跑一次 inspect dataset 头几条；如果某些条没有 `question_id`，loader 用 `format!("lme_q_{:04}", index)` fallback。Plan 此 task 作 sanity check。

7. **Dataset schema drift** —— LongMemEval 上游 schema 变化导致字段名 / 嵌套不匹配。Mitigation：loader 内 `serde_json` panic with clear message；用户重新下载 / 报 issue 升级 loader。不内嵌 schema-version，因为 LongMemEval 本身没有 versioned schema。

8. **Production embedding provider 配置** —— bench 跑时需要确认 `EMBED_*` env-vars 配置正确指向 EmbedAnything/Qwen3。Plan 第一 task probe step 跑一个最小 ingest+retrieve（mirror Wave 5 Task 1 probe）确认 chain 通；如果模型未 cache 第一次会 download，预期 wall-clock cold-start +几分钟。

## File Layout

**Created:**
- `tests/bench/longmemeval_dataset.rs` —— JSON deserialize types + `load_from_env_or_skip()`
- `tests/bench/longmemeval.rs` —— `run_longmemeval_bench`, `RungReport`, `BenchReport`, `print_comparison_table`, `write_per_rung_json`
- `tests/mempalace_bench.rs` —— single `#[tokio::test]` entry, `#[ignore]`'d
- `docs/superpowers/specs/2026-05-05-mempalace-bench-parity-design.md` —— this design

**Modified:**
- `src/pipeline/eval_metrics.rs` —— add `recall_any_at_k`, `recall_all_at_k` + 8 reference tests (4 each)
- `tests/bench/mod.rs` —— register `longmemeval`, `longmemeval_dataset` modules
- `README.md` —— bench section
- `CHANGELOG.md` —— 2026-05-05 entry
- `docs/ROADMAP.MD` —— add row #15

**Untouched (verify in self-review):**
- `src/pipeline/transcript_recall.rs` —— Wave 5 后稳定，本 wave 不动
- `src/storage/transcript_repo.rs`、`src/service/transcript_service.rs`、`src/http/transcripts.rs` —— in-process bench
- `tests/bench/runner.rs`（Wave 5 ablation runner）—— 独立的 LongMemEval runner 不强抽公共代码

## Out of Scope

- LoCoMo / ConvoMem / MemBench / Mine 4 个 mempalace bench port —— 后续独立 wave
- AAAK / llmrerank / hybrid_v2 / hybrid_v3 mode 的 mem 端实现
- Cross-process bench harness（HTTP 通过 `mem serve`）
- Latency benchmark（mempalace 没报这个）
- Dataset 内嵌下载（reqwest / HF Hub adapter）
- CI 集成（manual decision tool，与 mempalace 一致）
- Per-PR regression guard（synthetic recall-bench 已经是 CI gate）
- 跨系统比对工具（`jq` 后处理足够，不写 sidecar tool）
