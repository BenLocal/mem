# Recall ablation bench rebuild — capsule retrieval (design)

> Date: 2026-06-01 · Status: approved, pre-implementation
> Workstream: quantitative recall baseline (see [`ROADMAP.MD`](../../ROADMAP.MD) §下一阶段; replaces the `#14/#15` benches deleted in `4df527b`).
> Related: ③ [`long-content-recall.md`](../../long-content-recall.md), K9/K10 [`mempalace-diff-v4.md`](../../mempalace-diff-v4.md).

## 1. Problem & goal

`4df527b` deleted the old recall bench (9 modules, ~2.4k lines: transcript-recall ablation + LongMemEval parity + LLM judgment) because it was built on the removed `DuckDbRepository` + `VectorIndex` (usearch) sidecar. The metric functions in `src/pipeline/eval_metrics.rs` survived; everything else is gone, `bench-fixtures/` is empty.

Consequence: **K9 (edge dynamics), K10 (entity co-occurrence graph), and ③ (long-content chunking) all shipped with no quantitative recall evaluation.** Ranking changes are currently judged by unit tests (does signal X fire?) not by recall@k deltas.

**Goal:** a deterministic, offline ablation harness over **capsule retrieval** (`pipeline/retrieve.rs`) that reports recall/ndcg/mrr per retrieval-signal configuration, so K9/K10/③ get a numeric baseline and future ranking changes are measured, not guessed.

**Non-goals (v1):** LongMemEval parity (needs an external dataset), real-embedder fixtures, LLM judgment, transcript-path ablation. All deferred; the module boundaries stay generic enough to add them later.

## 2. Key design decision — public API, zero production change

The production ranker entry is already public and takes every ablation axis as a parameter:

```rust
// src/pipeline/retrieve.rs (pub)
pub async fn rank_with_hybrid_and_graph(
    pool: Vec<CapabilityCapsuleRecord>,                 // = Store::search_candidates(tenant)
    hybrid_hits: Vec<(CapabilityCapsuleRecord, f32)>,   // = Store::hybrid_candidates(tenant, text, vec, k)
    query: &SearchCapabilityCapsuleRequest,             // .expand_graph gates graph (K10)
    graph: &dyn GraphStore,                             // Store impls GraphStore
    dynamics: Option<&EdgeDynamicsCtx>,                 // Some => K9 decayed-strength weighting
) -> Result<Vec<CapabilityCapsuleRecord>, GraphError>;
```

`Store::{search_candidates, hybrid_candidates}` are `pub`. `eval_metrics::{ndcg_at_k, mrr, recall_at_k, precision_at_k}` are `pub`. Therefore the bench composes the **real** ranker from `tests/` with **no new production surface and no prod-code edits** — each rung is purely an input variation. This is the central reason the rebuild is cheap and faithful.

## 3. Module layout

```
tests/recall_bench.rs     # entrypoint; #[ignore]'d; `mod bench;`; runs the ladder, prints table, writes JSON
tests/bench/mod.rs        # pub mod fixture, geometry, synthetic, runner
tests/bench/fixture.rs    # Fixture / CapsuleFixture / QueryFixture / Qrels types (capsule-flavored)
tests/bench/geometry.rs   # GeometryProvider: impl mem::embedding::EmbeddingProvider
tests/bench/synthetic.rs  # generate(seed, cfg) -> Fixture  (deterministic)
tests/bench/runner.rs     # ingest fixture into fresh Store, run each Rung, aggregate Report; pretty_table + write_json
```
Reuses `src/pipeline/eval_metrics.rs` unchanged.

### 3.1 `GeometryProvider` (deterministic designed geometry)
Implements `mem::embedding::EmbeddingProvider`. Each topic owns an orthogonal basis vector in R^d (d small, e.g. 16; topics ≤ d). A text's embedding = L2-normalized Σ(basis[t] for each topic t whose canonical term appears in the text) + a tiny deterministic per-text jitter (hash-seeded, magnitude « inter-topic gap). The **same** function embeds capsule content and query text, so same-topic items are nearest neighbors by construction; cross-topic cosine is low. `embed_batch` maps `embed_text` over inputs. `dim()` = d.

This makes semantic recall meaningful and fully reproducible without a model, and lets ③ be exercised faithfully. The provider embeds whatever text it is handed, so the embedder's silent context-window truncation is modeled by **feeding it only the head window**: a long capsule (head topic A, tail topic B) embedded that way yields a single vector near A only, and a B-query misses it. The chunked path instead embeds each window's text separately, so the tail chunk's vector lands near B and the B-query recovers the capsule. (Note: the truncation must be modeled at the input — handing the provider the *full* long string would include B's term regardless of chunking and would not reproduce the bug.)

### 3.2 Fixtures (`fixture.rs` + `synthetic.rs`)
- `CapsuleFixture { id, content, topics: Vec<String>, scope/project/repo, long: bool }`.
- `QueryFixture { id, text, topic, expand_graph: bool, anchor_topic: Option<String> }`.
- `Qrels` = map `query_id -> HashSet<capsule_id>`, **derived by construction**: a query's relevant capsules are those whose `topics` contain the query topic (and, for graph queries, capsules reachable via a co-occurrence edge from the anchor topic).
- `generate(seed, cfg)` deterministically emits N topics, M capsules per topic (content seeded with the topic's canonical term + filler for BM25), a slice of `long` multi-topic capsules (head topic ≠ tail topic) for ③, co-occurrence edges among topics that share capsules, and one query per topic (+ a few graph-anchored and tail-targeted queries). Same seed ⇒ identical fixture.

### 3.3 Ingest (runner)
Per run: fresh tempdir `Store`; configure `MemoryService::with_providers(store, GeometryProvider, GeometryProvider)`; ingest capsules via the service; run `embedding_worker::tick` to completion so embeddings (chunked for `long` capsules) are populated by the geometry provider; write co-occurrence graph edges via `add_edge_direct` with controlled `strength`/`stability`/`last_activated` so the K9 rung has non-trivial decayed strengths to weight.

## 4. Rung ladder

Each rung produces a ranked capsule-id list per query, then averages metrics over queries. All driven by input variation through the public ranker.

| # | rung | mechanism | isolates |
|---|------|-----------|----------|
| 1 | lexical-only | `hybrid_candidates(text, vec=[])` → rank, `expand_graph=false`, `dynamics=None` | BM25 channel |
| 2 | semantic-only | `hybrid_candidates(text="", vec)` → rank | ANN channel |
| 3 | hybrid (baseline) | `hybrid_candidates(text, vec)` → rank, `expand_graph=false` | RRF fusion + lifecycle |
| 4 | + graph **(K10)** | rung 3 + `expand_graph=true`, fixture has co-occurrence edges | `graph_boost` |
| 5 | + dynamics **(K9)** | rung 4 + `dynamics=Some(ctx)`, edges carry strength | decayed-strength weighting |
| 6 | chunking on/off **(③)** | rung 3 over `long` fixtures: chunked vs head-window-only embeddings | long-content tail recall |
| 7 | oracle upper bound | rerank the candidate union by qrels | achievable ceiling |

`EdgeDynamicsCtx` for rung 5 is built with `current_timestamp()` and a throwaway mpsc sender (co-access enqueues are drained/ignored — the bench measures ranking, not potentiation side effects). For rung 5 to differ measurably from rung 4, the fixture's co-occurrence edges must carry **strengths that vary across edges** (set via `add_edge_direct`), so the decayed-strength weighting reorders graph-boosted capsules relative to the flat boost — otherwise rung 5 would equal rung 4. Rung 6 toggles chunking at embed time: the **off** variant stores a single embedding computed over only the first window (`DEFAULT_CHUNK_TOKENS`) of each `long` capsule's content (modeling the embedder silently dropping the tail), while the **on** variant uses the normal chunked worker path (one vector per window). Tail-targeted queries (topic = the capsule's tail topic) are recalled only in the on variant.

## 5. Metrics & output
Per rung, averaged over queries: ndcg@{5,10,20}, mrr, recall@10, precision@10 (from `eval_metrics`). `runner::pretty_table` prints rungs as rows with `Δ ndcg@10` vs the rung-3 hybrid baseline; `runner::write_json` writes a structured report to `target/recall_bench/<seed>.json`. Reporting shape mirrors the deleted runner (`RungReport` / `BenchReport`).

## 6. Determinism & entrypoint
Fully offline and reproducible: `GeometryProvider` is pure, fixed seed, fresh tempdir per run, no network or model download. Entrypoint `tests/recall_bench.rs` is `#[ignore]`'d (run via `cargo test --test recall_bench -- --ignored`; a `make bench-recall` target wraps it), so CI's default `cargo test` never executes the (slower) full bench — matching the old pattern.

## 7. Testing (the bench's own tests, run in default CI)
- `geometry`: same-topic cosine > cross-topic cosine; a long capsule's head vs tail single-window vectors point at different topics.
- `synthetic`: same seed ⇒ byte-identical fixture; different seed ⇒ different content; qrels populated and consistent with topic assignment.
- `runner` (on a tiny fixture): emits all 7 rungs; oracle ndcg@10 ≥ every other rung; **chunking-on recall@10 > chunking-off recall@10 for tail-targeted queries** (the ③ assertion that would have caught the original truncation bug); graph rung ndcg ≥ hybrid for graph-anchored queries.

## 8. Out of scope → future rungs
Transcript-path ablation (add a transcript loader + a `semantic_search_transcripts`/`bm25_transcript_candidates` rung), real-embedder fixture set (swap `GeometryProvider` for `embedanything`), LongMemEval parity, LLM judgment. `fixture`/`runner` interfaces stay generic (Fixture + Rung) so these slot in without restructuring.

## 9. Known limitation (v1)

On the v1 designed-geometry fixture, the Graph (K10) and Dynamics (K9) rungs show Δ≈0 vs Hybrid because every capsule shares the same `project`/`repo` entity (uniform 1-hop boost) and topic-entity links only reinforce already-top capsules. The rungs execute the real K9/K10 code paths, but discriminating these features requires a v1.1 fixture with graph-bridge capsules relevant only via graph reachability + strength-bearing co-occurrence edges. The ③ chunking and Oracle rungs are fully discriminating in v1.
