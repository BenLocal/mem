# Transcript-Recall Quality Ablation Bench — Design

> 给 transcript 召回管道的六个信号 + 一个 oracle rerank 上限做 ablation：合成层 CI 跑、真实层本地跑，输出 NDCG@k / MRR / Recall@k / Precision@k 让数据决定下一步要不要做真 cross-encoder。约束：bench 直接调生产 `score_candidates`（不写 parallel ranker）；fixture 用 entity-aliased co-mention 自动派生 judgment（不依赖人工 / LLM）；Oracle rerank = perfect-filter（不是 perfect-ranker），代表二元 reranker 的可达上限。

## Summary

`feat/transcript-recall` 上线后，`POST /transcripts/search` 的排序由六个加性信号合成：BM25 RRF、HNSW RRF、session co-occurrence、anchor session、freshness，再叠 `score_candidates` 内的 i64 加性 stack（`pipeline/transcript_recall.rs`）。这套 stack 凭"看上去合理"上线，没有量化每个信号的边际贡献，也没有对比"加 cross-encoder rerank 还有多少 headroom"。本设计落地一份 ablation harness 回答两个问题：

1. **现有六信号每个都拉得动权重吗？** 用 leave-one-out 三档（all-minus-cooc / all-minus-anchor / all-minus-freshness）算每个信号的 ΔNDCG。
2. **要不要做 cross-encoder rerank？** 用 oracle rerank（按 gold relevance 把候选 partition 到前段）算 rerank 的理论上限。oracle 涨幅小 → 真模型不可能更好，决策是"不做"；oracle 涨幅大 → 决策升级为后续独立 spike 跑真模型。

Bench 不是性能（time）bench——是质量（rank quality）bench。`benches/`（criterion）形状不对，落点是 `tests/recall_bench.rs` + `tests/bench/` 共享模块；regression assertions（hybrid ≥ singleton 等）由 CI 每次跑 synthetic 把守。

## Goals

- 一份合成 fixture（in-tree、固定 seed、reproducible）+ 一份真实 fixture loader（gitignored、env-var 路径）
- entity-registry 加持的 co-mention judgment 派生（query 命中 entity E → session 含 E 任意 alias → relevant）
- 10 个 ablation rung：累加 7 + leave-one-out 3，全部走同一份生产 `score_candidates`
- 4 个 metric pure function（NDCG@k / MRR / Recall@k / Precision@k）落到 `src/pipeline/eval_metrics.rs`，每函数 4-6 个手算 reference test
- Bench harness 跑完输出 stdout 漂亮表 + JSON dump（`target/bench-out/recall-{synthetic,real}.json`）
- CI synthetic 一档跑 ~5s 内、做 monotone-improvement regression assert；real 档 `#[ignore]` + env var 缺失自动 skip
- 文档化 D 路径（co-mention auto-judgment）的偏 lexical bias，bench 输出 footer 常驻 bias notice

## Non-Goals

- 不集成真 cross-encoder 模型（embed_anything reranker、bge-reranker、ms-marco-MiniLM 等都不上）——oracle rerank rung 是 v1，真模型留给后续独立 spike 决策
- 不做人工 / LLM-as-judge 标注（D 选了纯自动派生）
- 不做 graded relevance（rel ∈ {0, 1}，不是 0-3）——co-mention 不可靠地区分 graded levels，强分级会引噪
- 不引入 criterion / benches/ 目录——质量 bench 不是 time bench
- 不暴露 bench 命令到 CLI / MCP——`cargo test --test recall_bench` 是入口
- 不为 bench 加 retention / 历史结果对比 / dashboard——单次跑出数即决策即结束
- 不优化 score 公式 / 调参——本设计只测量，调参是另一条独立 PR
- 不增 bench 跑 retrieve.rs（memories pipeline）——本 bench 专攻 transcripts pipeline，memories ablation 是另一份 spec

## Decisions (resolved during brainstorming)

- **Q1（fixture 来源）**：C — 混合分层。`SyntheticConfig::default()` 给一份 30 session × 8 block × 24 query 的 in-tree fixture（CI 友好），`MEM_BENCH_FIXTURE_PATH` env var 指向 gitignored 真实导出 JSON（本地决策跑）。
- **Q2（judgment 派生）**：D — 全自动 co-mention，加 entity-registry 让 alias 命中也算 relevant（query 关于 entity E → 任意 alias 解析到 E 的 session 都 mark relevant）。承认偏 lexical 的 bias，bench 输出常驻 footer notice。
- **Q3（ablation 矩阵）**：B — 累加 7 档 + leave-one-out 3 档 = 10 rungs。累加答主问题（rerank yes/no），leave-one-out 答副问题（信号删不删）。
- **Q4（metric 实现）**：A — 自写 `src/pipeline/eval_metrics.rs`，纯函数，~150 行 + 单测。Rust 无 canonical IR-eval crate，写 4 个 50 行函数比起 wrap 一个 < 10k downloads 的小众 crate 更清。
- **Q5+Q6（harness 形态 + 落点）**：A — `tests/recall_bench.rs`，env-var 切 fixture 层，`#[ignore]` 闸 real 跑，输出 `--nocapture` stdout 表 + JSON 写盘。零新 bin target、零 `benches/` 目录、零 criterion 依赖。
- **Q7（cross-encoder rung）**：B — Oracle fake reranker（按 gold relevance 把候选 partition 到前段、相关内部保留原 score 序）。代表二元 reranker 上限。真模型留待后续独立 spike。

## Architecture

```
                 ┌─────────────────────────────────────────┐
 fixture layer   │  synthetic generator   real loader      │
                 │  (in-tree, det. seed)  (gitignored,     │
                 │                         JSON dump)      │
                 └────────────────┬────────────────────────┘
                                  │
                                  ▼
                  ┌──────────────────────────────────┐
                  │  Fixture { sessions, blocks,     │
                  │            queries, judgments }  │
                  └──────────────┬───────────────────┘
                                 │
                                 ▼
       ┌─────────────────── ablation runner ───────────────────────┐
       │                                                            │
       │   for each rung in 10_rungs:                               │
       │       fresh TempDir → DuckDbRepository::new                │
       │       ingest fixture → conversation_messages + entities    │
       │       for each query in queries:                           │
       │           candidates = retrieve_under_rung(query, rung)    │
       │           run = score_candidates(...) → window_ids         │
       │           per_query[query.id] = eval_metrics(run, qrels)   │
       │       agg = mean / stddev across queries                   │
       │   return RungReport[]                                      │
       └───────────────────────────────────┬────────────────────────┘
                                           │
                                           ▼
                stdout pretty table + JSON dump (target/bench-out/)
                + CI regression assertions (synthetic only)
```

**关键解耦点：**

1. **`Rung` 是 const-time 配置**，不是 10 份并行 ranker。所有 rung 调同一份 `pipeline::transcript_recall::score_candidates`。Rung 间差异 = `(SourceMix, ScoringOpts, RerankPolicy)` 三元组：
   - `SourceMix`：候选来自 BM25-only / HNSW-only / Both
   - `ScoringOpts`：现有 struct 的字段开关（disable_session_cooc / disable_anchor / disable_freshness）—— Task 4 会扩这个 struct
   - `RerankPolicy`：None / OracleByJudgment（后置 hook，runner 持有 judgments 引用）
2. **Bench 与生产路径走同一 `score_candidates`** —— bench 数字直接外推生产，不存在"bench 测了一份并行 stack 但生产实际跑的是另一份"的失配风险。
3. **判分 / 候选 / score / metric 各自纯函数化** —— 4 个独立模块拼装，每个独立可测：
   - `tests/bench/fixture.rs` (synthetic 生成器 + real loader)
   - `tests/bench/judgment.rs` (co-mention + entity-alias 派生)
   - `tests/bench/runner.rs` (rung 调度 + DB 复位 + 指标聚合)
   - `src/pipeline/eval_metrics.rs` (NDCG / MRR / R@k / P@k 纯函数 — 进 src/ 因为指标公式可能被未来 retrieve.rs ablation 复用)
4. **真实 fixture 路径**：`MEM_BENCH_FIXTURE_PATH=/abs/path/to/dump.json`，`#[ignore]`'d 测试 + env var 缺失则 `eprintln!("skip"); return;`（不 panic、不算 fail）。

## Components

### Synthetic Fixture（`tests/bench/fixture/synthetic.rs`）

**Config：**

```rust
pub struct SyntheticConfig {
    pub seed: u64,                  // 默认 42
    pub session_count: usize,       // 默认 30
    pub blocks_per_session: usize,  // 默认 8（4 user text + 4 assistant text，无 tool block）
    pub topic_pool: Vec<TopicSeed>, // 默认 12（见下）
    pub query_count: usize,         // 默认 24
    pub noise_words_per_block: usize, // 默认 30，控制 BM25 信噪比
}

pub struct TopicSeed {
    pub canonical: &'static str,    // 用作 EntityKind::Topic 的 canonical_name
    pub aliases: &'static [&'static str], // 进 entity_aliases
}

// 默认 topic_pool（in-tree const）
pub const DEFAULT_TOPICS: &[TopicSeed] = &[
    TopicSeed { canonical: "Rust async", aliases: &["tokio", "futures", "await"] },
    TopicSeed { canonical: "DuckDB", aliases: &["duckdb", "olap", "columnar"] },
    TopicSeed { canonical: "HNSW", aliases: &["usearch", "ann", "vector index"] },
    TopicSeed { canonical: "BM25", aliases: &["fts", "tantivy", "lexical"] },
    TopicSeed { canonical: "session window", aliases: &["sliding", "bucket", "auto-bucket"] },
    TopicSeed { canonical: "ranking", aliases: &["rrf", "fusion", "reranker"] },
    // …共 12 个
];
```

**Generator 步骤：**

1. `StdRng::seed_from_u64(config.seed)` 单一随机源——重跑 bit-exact reproducible
2. 每 session 抽 1-2 个 topic（无替换）；每 block 把 topic.canonical 或随机 alias 嵌入 content（位置随机）+ `noise_words_per_block` 个 lorem-ipsum 噪声
3. Query 由 topic.canonical / alias 选一构造（query="how do I use tokio for async Rust?"，topic_link=Rust async）
4. Judgment 由生成器**直接产出**：`(query_id, session_id) ∈ judgments` iff session 的任一 topic 与 query 的 topic 同源（synthetic 路径不需要 entity registry，因为 judgment 是 pre-computed）
5. 每 block 的 `created_at` 跨 90 天均匀分布 + 每 session 内单调递增——freshness 信号有可测信号

`SyntheticConfig::default()` 给 in-tree、CI 跑 ~5s 的 fixture（30 session × 8 block = 240 block，24 query）。

### Real Fixture Loader（`tests/bench/fixture/real.rs`）

**JSON schema（`bench-fixtures/recall-real.json`，gitignored，文件不进 git）：**

```json
{
  "loader_version": 1,
  "tenant": "local",
  "sessions": [
    {
      "session_id": "01HZ...",
      "started_at": "2026-04-15T...",
      "blocks": [
        { "block_id": "01HZ...", "role": "user", "block_type": "text", "content": "...", "created_at": "..." }
      ]
    }
  ],
  "queries": [
    {
      "query_id": "q_001",
      "text": "Rust async error handling patterns",
      "anchor_session_id": null,
      "anchor_entities": ["Rust async", "error handling"]
    }
  ]
}
```

**`anchor_entities` 含义：** 该 query 关于这些 entity；judgment 派生时把它们丢进 `EntityRegistry::resolve_or_create` 拿 entity_id，然后扫所有 session 的 block content 找任一 alias 命中。这是 D 路径的核心机制——**fixture 标的不是 (query, relevant_session_ids)，而是 (query, anchor_entities)**，relevant 由 entity registry 派生。

**Loader 行为：**

- env var `MEM_BENCH_FIXTURE_PATH` 缺失 → `eprintln!("real fixture not configured; skipping"); return;`（测试 pass）
- `loader_version != 1` → panic with clear message（避免 silent schema drift）
- 文件读不开 → panic（操作员显式给了路径却没给文件，应该响）
- `bench-fixtures/` 目录加进 `.gitignore`

### Judgment Derivation（`tests/bench/judgment.rs`）

```rust
pub async fn derive_judgments(
    fixture: &Fixture,
    registry: &dyn EntityRegistry,
    tenant: &str,
) -> JudgmentMap {
    let mut judgments: JudgmentMap = HashMap::new();
    for query in &fixture.queries {
        // 路径分叉：
        // - synthetic：generator 已经把 (query_id, relevant_session_ids) 塞到 fixture.queries[i].synthetic_judgments
        //   if let Some(synth) = &query.synthetic_judgments { judgments.insert(query.id, synth.clone()); continue; }
        // - real：通过 anchor_entities 派生
        let mut entity_ids: HashSet<String> = HashSet::new();
        for alias in &query.anchor_entities {
            let id = registry.resolve_or_create(tenant, alias, EntityKind::Topic, &now()).await?;
            entity_ids.insert(id);
        }
        let mut relevant = HashSet::new();
        for session in &fixture.sessions {
            if session_mentions_any_alias_of(session, &entity_ids, registry, tenant).await {
                relevant.insert(session.id.clone());
            }
        }
        judgments.insert(query.id.clone(), relevant);
    }
    judgments
}

async fn session_mentions_any_alias_of(
    session: &SessionFixture,
    entity_ids: &HashSet<String>,
    registry: &dyn EntityRegistry,
    tenant: &str,
) -> bool {
    // 拼 session.blocks[*].content（仅 text/thinking，不含 tool_use/tool_result——与召回口径对称）
    // 对每个非空白 token：normalize_alias → registry.lookup_alias(tenant, normalized) → 若返回的 entity_id 在 entity_ids 中 → true
    // 优化路径：实际实现可一次扫所有 alias 列表（拉 list_entities + aliases），but the spec不规定具体优化
}
```

**Relevance 是二元（0/1）。** NDCG 用 binary gain（rel=1 → gain=1）。

**Bias notice（spec 强制要求 bench 输出 footer 包含此文本）：**

> ⚠ Bias notice: judgments derived from co-mention + entity aliases.
>   HNSW absolute scores under-counted; relative deltas (+anchor, -anchor) reliable.
>   See spec §3 for details.

### Eval Metrics（`src/pipeline/eval_metrics.rs`）

Pub fn 列表：

```rust
pub fn dcg(gains: &[f64]) -> f64;
pub fn ideal_dcg(relevant_count: usize, k: usize) -> f64;
pub fn ndcg_at_k<I: Eq + Hash>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64;
pub fn mrr<I: Eq + Hash>(run: &[I], qrels: &HashSet<I>) -> f64;
pub fn recall_at_k<I: Eq + Hash>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64;
pub fn precision_at_k<I: Eq + Hash>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64;
```

每函数 4-6 个手算 reference test：

- `dcg([1,1,0,1])` 手算 ≈ `1/log2(2) + 1/log2(3) + 0 + 1/log2(5) ≈ 1 + 0.6309 + 0 + 0.4307 ≈ 2.0616`
- `ndcg_at_k(run=[a,b,c], qrels={a,c}, k=3)` 手算 ≈ `(1/log2(2) + 0 + 1/log2(4)) / (1/log2(2) + 1/log2(3))` ≈ 0.7654
- `mrr(run=[a,b,c], qrels={c})` = 1/3
- `recall_at_k(run=[a,b], qrels={a,b,c,d}, k=2)` = 2/4 = 0.5
- `precision_at_k(run=[a,b,c], qrels={a,c,e}, k=3)` = 2/3

`k` 默认 10，bench 输出报 k=5 / 10 / 20 三档。

### Ablation Runner（`tests/bench/runner.rs`）

```rust
pub struct Rung { pub name: &'static str, pub source: SourceMix, pub opts: ScoringOpts, pub rerank: RerankPolicy }
pub enum SourceMix { Bm25Only, HnswOnly, Both }
pub enum RerankPolicy { None, OracleByJudgment }

pub const RUNGS: &[Rung] = &[
    Rung { name: "bm25-only",            source: SourceMix::Bm25Only, opts: NO_BONUSES,            rerank: None },
    Rung { name: "hnsw-only",            source: SourceMix::HnswOnly, opts: NO_BONUSES,            rerank: None },
    Rung { name: "hybrid-rrf",           source: SourceMix::Both,     opts: NO_BONUSES,            rerank: None },
    Rung { name: "+session-cooc",        source: SourceMix::Both,     opts: COOC_ONLY,             rerank: None },
    Rung { name: "+anchor",              source: SourceMix::Both,     opts: COOC_AND_ANCHOR,       rerank: None },
    Rung { name: "+freshness (full)",    source: SourceMix::Both,     opts: ALL_BONUSES,           rerank: None },
    Rung { name: "+oracle-rerank",       source: SourceMix::Both,     opts: ALL_BONUSES,           rerank: OracleByJudgment },
    Rung { name: "all-minus-cooc",       source: SourceMix::Both,     opts: ALL_BONUSES_NO_COOC,   rerank: None },
    Rung { name: "all-minus-anchor",     source: SourceMix::Both,     opts: ALL_BONUSES_NO_ANCHOR, rerank: None },
    Rung { name: "all-minus-freshness",  source: SourceMix::Both,     opts: ALL_BONUSES_NO_FRESH,  rerank: None },
];
```

**`ScoringOpts` 扩字段（task 4）：**

现 `pipeline/transcript_recall.rs::ScoringOpts` 已有 `anchor_session_id` 字段。需扩三个 `disable_*` bool（默认 false→不破坏现有调用）：

```rust
pub struct ScoringOpts {
    pub anchor_session_id: Option<String>,
    pub disable_session_cooc: bool,    // NEW，默认 false
    pub disable_anchor: bool,          // NEW，默认 false
    pub disable_freshness: bool,       // NEW，默认 false
}
```

`score_candidates` 内部对每个 bonus 加判：`if !opts.disable_session_cooc { score += session_cooc_bonus(...); }`。生产路径 default ScoringOpts → 三个 disable 全 false → zero behavior change。

**Runner 步骤（每 rung）：**

1. 新 `TempDir` → `DuckDbRepository::new(path)` → schema bootstrap → 建空 `EntityRegistry` 实例
2. Ingest fixture：每 block 走 `repo.append_conversation_message`（与生产 `POST /transcripts/messages` 同入口）
3. Fixture 全量 ingest 完，让 transcript embedding worker 同步索引：bench 用 `FakeEmbeddingProvider`（`src/embedding/fake.rs`，已有）以确保 deterministic + 不依赖外部 API
4. Per query：
   - 按 `rung.source` 决定候选源（`bm25_transcript_candidates` / `vector_index.search` / both）
   - 调 `score_candidates(candidates, lex_ranks, sem_ranks, &rung.opts)`
   - 排序后取 top-K（K=20）作为 `run: Vec<SessionId>`（bench 在 session 粒度评，不在 block 粒度——与召回输出 `windows` 对齐）
   - `RerankPolicy::OracleByJudgment` 触发：`run = oracle_rerank(run, judgments[query.id])`
   - `eval_metrics(run, qrels)` 算 NDCG@5/10/20、MRR、R@10、P@10
5. 跨 query 取均值 + stddev → `RungReport`

### Oracle Rerank（`tests/bench/runner.rs::oracle_rerank`）

```rust
pub fn oracle_rerank<I: Eq + Hash>(run: Vec<I>, qrels: &HashSet<I>) -> Vec<I> {
    let (rel, irrel): (Vec<_>, Vec<_>) = run.into_iter().partition(|id| qrels.contains(id));
    [rel, irrel].concat()  // 相关全部上前；相关内部保留原 score 序
}
```

代表"perfect filter"——把 relevant 全推前段、irrelevant 全压后段，**不**重排相关内部顺序。这给 binary reranker 的可达上限（real cross-encoder 也只能区分 relevant / irrelevant，graded 排序需要 graded judgment 我们没有）。比"perfect ranker"略保守，是更诚实的"最好可能的二元 reranker"。

## Test Harness（`tests/recall_bench.rs`）

```rust
mod bench;
use bench::*;

#[tokio::test(flavor = "multi_thread")]
async fn synthetic_recall_bench() {
    let fixture = Fixture::synthetic(SyntheticConfig::default());
    let report = run_bench(fixture).await;

    print_pretty_table(&report);  // stdout via --nocapture
    write_json(&report, "target/bench-out/recall-synthetic.json").unwrap();

    // CI regression assertions（仅 synthetic 跑；real 是 informational only）
    let r = |name| report.rung_by_name(name);
    assert!(r("hybrid-rrf").ndcg_at_10 >= r("bm25-only").ndcg_at_10,
            "hybrid should not regress vs BM25-only");
    assert!(r("hybrid-rrf").ndcg_at_10 >= r("hnsw-only").ndcg_at_10,
            "hybrid should not regress vs HNSW-only");
    assert!(r("+freshness (full)").ndcg_at_10 >= r("hybrid-rrf").ndcg_at_10 - 0.01,
            "full stack should not regress >0.01 vs hybrid-rrf");
    assert!(r("+oracle-rerank").ndcg_at_10 >= r("+freshness (full)").ndcg_at_10,
            "oracle must be ≥ full stack (it's an upper bound)");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real fixture; set MEM_BENCH_FIXTURE_PATH=…"]
async fn real_recall_bench() {
    let path = match std::env::var("MEM_BENCH_FIXTURE_PATH") {
        Ok(p) => p,
        Err(_) => { eprintln!("MEM_BENCH_FIXTURE_PATH not set; skipping real bench"); return; }
    };
    let fixture = Fixture::load_real(&path).expect("invalid real fixture");
    let report = run_bench(fixture).await;

    print_pretty_table(&report);
    write_json(&report, "target/bench-out/recall-real.json").unwrap();
    // No assertions — purely informational
}
```

**Pretty 表样例（synthetic 实跑后会更新）：**

```
=== Recall Bench (synthetic, seed=42, 24 queries × 30 sessions) ===
                       NDCG@5  NDCG@10 NDCG@20  MRR    R@10  P@10
bm25-only              0.612   0.658   0.701   0.721  0.583  0.290
hnsw-only              0.589   0.641   0.690   0.704  0.566  0.275
hybrid-rrf             0.704   0.748   0.789   0.812  0.671  0.341
+session-cooc          0.721   0.762   0.798   0.825  0.683  0.348
+anchor                0.738   0.778   0.812   0.840  0.694  0.355
+freshness (full)      0.741   0.782   0.815   0.844  0.697  0.358
+oracle-rerank         0.882   0.911   0.928   0.952  0.812  0.418
all-minus-cooc         0.735   0.776   0.811   0.838  0.692  0.355  (Δ -0.006)
all-minus-anchor       0.717   0.761   0.797   0.821  0.681  0.348  (Δ -0.021)
all-minus-freshness    0.738   0.778   0.812   0.840  0.694  0.355  (Δ -0.004)

⚠ Bias notice: judgments derived from co-mention + entity aliases.
  HNSW absolute scores under-counted; relative deltas (+anchor, -anchor)
  are reliable. See spec §3.
```

**JSON dump shape：**

```json
{
  "fixture_meta": { "kind": "synthetic", "seed": 42, "session_count": 30, "query_count": 24 },
  "rungs": [
    {
      "name": "bm25-only",
      "ndcg_at_5": 0.612, "ndcg_at_10": 0.658, "ndcg_at_20": 0.701,
      "mrr": 0.721, "recall_at_10": 0.583, "precision_at_10": 0.290,
      "per_query": [{ "query_id": "q_001", "ndcg_at_10": 0.65, ... }, ...]
    }
  ]
}
```

## File Layout

**Created:**

- `src/pipeline/eval_metrics.rs` — 4 pub fn + dcg/ideal_dcg helpers + 24 unit tests
- `tests/bench/mod.rs` — re-exports (`pub use fixture::*; pub use judgment::*; pub use runner::*;`)
- `tests/bench/fixture/mod.rs` — `Fixture` struct + builder
- `tests/bench/fixture/synthetic.rs` — `SyntheticConfig`, `DEFAULT_TOPICS`, generator
- `tests/bench/fixture/real.rs` — JSON loader + schema check
- `tests/bench/judgment.rs` — `derive_judgments` + `session_mentions_any_alias_of`
- `tests/bench/runner.rs` — `Rung`, `RUNGS`, `run_bench`, `oracle_rerank`, `RungReport`, `print_pretty_table`, `write_json`
- `tests/recall_bench.rs` — 2 `#[tokio::test]` entries
- `bench-fixtures/.gitkeep` — 占位（目录加进 `.gitignore`，文件本身不入库）
- `docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md` — 本设计

**Modified:**

- `src/pipeline/mod.rs` — `pub mod eval_metrics;`
- `src/pipeline/transcript_recall.rs::ScoringOpts` — 加三个 `disable_*: bool` 字段（默认 false → zero behavior change）
- `src/pipeline/transcript_recall.rs::score_candidates` — bonus 加 `if !opts.disable_*` 守卫
- `.gitignore` — 加 `bench-fixtures/` 和 `target/bench-out/`
- `README.md` — bench 命令小节（怎么跑 synthetic、怎么配 real fixture path、怎么读输出）
- `CHANGELOG.md` — 新条目
- `docs/ROADMAP.MD` — 视情况新增"评估"维度（如果 ROADMAP 还没"质量基线"项就借这次新增；否则就在 #11 后追加备注）

**Untouched (verify in self-review):**

- `src/storage/transcript_repo.rs`、`src/service/transcript_service.rs`、`src/http/transcripts.rs` —— bench 读生产 ingest API，不改生产代码
- `src/pipeline/ranking.rs` —— 共享纯函数稳定不动
- `src/storage/duckdb.rs::EntityRegistry` —— 生产实现 lookup_alias 已在 entity-registry wave shipped；bench 直接 reuse
- 现有 25+ 份 `tests/*.rs` —— bench 是新增 2 个 entry；不改老 test

## Risks & Mitigations

1. **D 路径 ground truth 偏 lexical** —— HNSW 同义词命中得不到 credit。Mitigation：entity-alias 解析（已选）+ bench 输出常驻 footer notice + spec 显式说明"绝对分数不可信，相对 delta 可信"。
2. **`FakeEmbeddingProvider` 与生产 embed_anything 输出空间不同** —— bench 数字与"换真模型后的生产数字"会 shift。Mitigation：bench 决策 frame 是 ablation rung 间的 ΔNDCG（信号是否拉得动权重），不是绝对值；fake 在所有 rung 都同分布，相对结论保留。
3. **Synthetic fixture 太小可能不显著** —— 30 session × 24 query 单 query 标准误差大，stddev 可能 swallow 微小信号差异。Mitigation：CI 跑 synthetic 是 regression smoke（保 monotone improvement），决策跑用 real fixture（更大语料）；spec 留扩 `session_count` 旋钮但默认值控成本。
4. **Oracle rerank 是上限不是真上限** —— oracle 不重排相关内部顺序，真"perfect ranker"本应能更高。Mitigation：spec 明确 oracle 语义"perfect filter"而非"perfect ranker"；这是更保守的上限，在 ground truth 是二元的前提下也是对真模型上限的诚实估计。
5. **Real fixture 隐私风险** —— 用户自己的 transcripts 含敏感对话。Mitigation：fixture 文件 gitignored、env-var 路径、loader_version 防 schema drift；spec 不提供 fixture 内容样本。
6. **Bench 跑时间膨胀** —— 10 rung × 24 query × FakeEmbedder + DuckDB roundtrip 单次跑 ~5s 估计；real fixture 200 session 可能 ~30s。Mitigation：synthetic 进 CI 默认跑、real `#[ignore]` 闸；JSON 输出便于跨次比对（不在 bench 内做"重复跑取均值"——criterion 那一套是给 time bench 的，质量 bench 单次跑足够）。
7. **`ScoringOpts` 加三个 bool 字段是非破坏性变更但仍触及生产 struct** —— 所有现有 `ScoringOpts::default()` / `ScoringOpts { ... }` 调用点要审。Mitigation：默认值 false 保 zero behavior change；现有 tests `cargo test -q` 全绿是合并门禁；commit 单独成步（refactor(transcript): extend ScoringOpts with disable flags）便于 code review。

## Concerns to Confirm Before Implementation

1. **`tests/bench/` 共享模块路径**：`tests/common/mod.rs` 已有共享模式，bench 用同样的 `tests/<dir>/mod.rs` 风格是否被项目接受？—— 计划阶段如果发现 `tests/common/` 是 root-level mod 而非 dir，则改用平铺命名 `tests/bench_fixture.rs` / `tests/bench_judgment.rs` / `tests/bench_runner.rs` + `mod` declarations 在 entry 里。
2. **Bench 是否会 trip 现有的 transcript-recall integration tests**：synthetic ingest 路径调 `repo.append_conversation_message` 会触发 `transcript_embedding_jobs` 入队；FakeEmbedder 配置正确（bench harness 显式注入）应该与现有 `tests/transcript_recall.rs` 行为一致。计划阶段第一个 task 应该 spike 一个最小 ingest + retrieve 闭环验证 harness 接得通。

## Out of Scope（明确不做）

- 真 cross-encoder 模型集成 —— 留给后续 spike，spec 引用本 bench 的 oracle rung 数字作为 motivating data
- 跨 commit 的 metric 历史 trace（"每个 PR 跑一次 bench 比对"）—— observability 是另一条 PR
- Memories pipeline ablation —— 本 spec 只覆盖 transcripts pipeline；memories 用一份对应的独立 spec
- Adversarial query / paraphrase robustness benchmark —— 用 D 路径的 fixture 做不了，需要专门的 paraphrased query 集
- Latency / throughput 维度 —— `cargo bench` + criterion 是另一回事，本 bench 不掺
