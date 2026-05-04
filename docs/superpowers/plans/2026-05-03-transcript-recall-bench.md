# Transcript-Recall Ablation Bench Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Quality-ablation harness for the transcript recall pipeline that quantifies each ranking signal's marginal contribution and gives an oracle-rerank upper bound, so we can decide from data whether to invest in a real cross-encoder.

**Architecture:** 4 isolated units (eval metrics in `src/pipeline/`, fixture / judgment / runner / oracle in `tests/bench/`) plug together in `tests/recall_bench.rs`. Synthetic fixture is checked-in (CI guard); real fixture is gitignored, env-var-loaded. All 10 ablation rungs share the same production `pipeline::transcript_recall::score_candidates` — rung difference is `(SourceMix, ScoringOpts, RerankPolicy)` config tuple. Metrics are self-implemented pure functions (NDCG@k / MRR / R@k / P@k); harness output is stdout pretty table + `target/bench-out/recall-{synthetic,real}.json`.

**Tech Stack:** Rust 2021, tokio test, DuckDB (bundled), `tempfile::TempDir`, `serde_json`, `rand` (StdRng / SeedableRng) for deterministic synthetic generation, `FakeEmbeddingProvider` (existing) for deterministic embeddings.

**Spec:** `docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md` (commit `32c3097`).

---

## Conventions referenced throughout

- **Pure functions in `pipeline/`**: no DB, no async, no I/O. `eval_metrics.rs` follows this.
- **Test isolation**: every test using DuckDB owns a `tempfile::TempDir`. Never share paths.
- **Deterministic randomness**: `StdRng::seed_from_u64(seed)` from `rand` crate, single source. No `thread_rng()` anywhere.
- **`FakeEmbeddingProvider`**: at `src/embedding/fake.rs`; `pub use mem::embedding::FakeEmbeddingProvider`. Use it everywhere the bench needs embeddings.
- **`tests/common/mod.rs` precedent**: that's a single-file `mod` shared across tests via `mod common;` at the top of each test file. We follow the same pattern with `tests/bench/mod.rs` + submodules.
- **Commit scope tags**: `feat(bench)`, `feat(metrics)`, `refactor(transcript)`, `test(bench)`, `docs(bench)`.
- **CI gates non-negotiable**: `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`.

---

## File Structure (locked decisions)

**Created:**
- `src/pipeline/eval_metrics.rs` — 4 pub fns + 2 helpers + 24 unit tests
- `tests/bench/mod.rs` — re-exports
- `tests/bench/fixture.rs` — `Fixture`, `SessionFixture`, `BlockFixture`, `QueryFixture`, `JudgmentMap` + Default impls
- `tests/bench/synthetic.rs` — `SyntheticConfig`, `DEFAULT_TOPICS`, `generate(...)`
- `tests/bench/real.rs` — JSON loader + version check + env-var skip
- `tests/bench/judgment.rs` — `derive_judgments` + `session_mentions_any_alias_of`
- `tests/bench/oracle.rs` — `oracle_rerank` pure fn + tests
- `tests/bench/runner.rs` — `Rung`, `RUNGS`, `RungReport`, `run_bench`, `print_pretty_table`, `write_json`
- `tests/recall_bench.rs` — `synthetic_recall_bench` + `real_recall_bench` test entries

**Modified:**
- `src/pipeline/mod.rs` — `pub mod eval_metrics;`
- `src/pipeline/transcript_recall.rs` — extend `ScoringOpts<'a>` with 3 `disable_*: bool` fields + guards in `score_candidates`
- `.gitignore` — `bench-fixtures/` + `target/bench-out/` (target/ already ignored, but be explicit for the bench subdir)
- `README.md` — bench section
- `CHANGELOG.md` — wave entry
- `docs/ROADMAP.MD` — quality bench note

**Untouched (verify in self-review):**
- `src/storage/transcript_repo.rs` (bench reads its public `bm25_transcript_candidates` API)
- `src/service/transcript_service.rs` (bench uses repo + pipeline directly, not the service layer)
- `src/http/transcripts.rs` (no HTTP shape change)
- `src/pipeline/ranking.rs` (shared pure helpers stable)
- `src/embedding/fake.rs` (consumed as-is)

---

## Task 1: Probe — end-to-end harness validation

**Files:**
- Create: `tests/recall_bench.rs` (probe only at this point)

The spec's "Concerns to Confirm" §2 flags that the bench harness chain (FakeEmbeddingProvider + transcript ingest + bm25 + HNSW + score_candidates) needs validation before building the runner. This task is a 30-min spike: ingest one fixture, retrieve via both candidate sources, confirm `score_candidates` returns ranked output. Same pattern as Task 1 of the entity-registry plan (probe before structure).

- [ ] **Step 1: Create `tests/recall_bench.rs` with probe**

```rust
//! Recall ablation bench (closes ROADMAP "quality baseline").
//! See docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md.
//!
//! ### Probe outcome (Task 1, 2026-05-03)
//! TBD — populate after Step 2 runs.

use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "probe — run with --ignored"]
async fn harness_probe_ingests_and_retrieves_via_bm25_and_hnsw() {
    use mem::config::Config;
    use mem::storage::DuckDbRepository;
    use mem::embedding::FakeEmbeddingProvider;
    use mem::domain::ConversationMessage;

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("probe.duckdb");
    let cfg = Config::local_with_db_path(db_path.clone());
    let repo = DuckDbRepository::open(&cfg).await.unwrap();

    // Ingest one block.
    let now = "00000000020260503000";
    let msg = ConversationMessage {
        message_id: "m1".to_string(),
        tenant: "t".to_string(),
        session_id: Some("s1".to_string()),
        role: "user".to_string(),
        block_type: "text".to_string(),
        content: "Tokio runtime async Rust example".to_string(),
        embed_eligible: true,
        created_at: now.to_string(),
        ..Default::default()
    };
    repo.append_conversation_message(&msg).await.unwrap();

    // Generate + commit embedding via FakeEmbeddingProvider.
    let fake = Arc::new(FakeEmbeddingProvider::new("fake", 64));
    let v = fake.embed(&msg.content).await.unwrap();
    repo.upsert_transcript_embedding("t", "m1", &v).await.unwrap();

    // BM25 retrieval.
    let bm25 = repo
        .bm25_transcript_candidates("t", "Tokio Rust", 10)
        .await
        .unwrap();
    println!("BM25 results: {} candidates", bm25.len());
    assert!(!bm25.is_empty(), "BM25 should find the ingested block");

    // HNSW retrieval via vector_index.
    let qv = fake.embed("Tokio Rust").await.unwrap();
    let hnsw = repo.search_transcript_embeddings("t", &qv, 10).await.unwrap();
    println!("HNSW results: {} candidates", hnsw.len());
    assert!(!hnsw.is_empty(), "HNSW should find the ingested block");

    println!("HARNESS PROBE PASSED — bench foundation is sound");
}
```

**Note for implementer:** the exact method names (`Config::local_with_db_path`, `repo.upsert_transcript_embedding`, `repo.search_transcript_embeddings`) may need adjustment to match the actual repository API. Check `tests/transcript_recall.rs` for the established pattern and adapt the probe to use whatever method names exist there. The point of the probe is the chain (ingest → BM25 candidates non-empty → HNSW candidates non-empty), not the exact API shape.

- [ ] **Step 2: Run the probe**

```bash
cargo test --test recall_bench harness_probe -- --ignored --nocapture
```

Expected: `BM25 results: 1 candidates`, `HNSW results: 1 candidates`, `HARNESS PROBE PASSED — bench foundation is sound`.

If either assertion fails, STOP and report `BLOCKED` — the bench foundation is broken and Task 8 (runner) won't work.

- [ ] **Step 3: Update probe outcome docstring**

Edit the file's top docstring (the `### Probe outcome` block) to record: which API names you used, any deviations from the example code above, and confirmation that both candidate sources returned non-empty.

- [ ] **Step 4: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --test recall_bench -q
```

Expected: 0 active tests run (probe is `#[ignore]`'d), build clean.

- [ ] **Step 5: Commit**

```bash
git add tests/recall_bench.rs
git commit -m "$(cat <<'EOF'
test(bench): probe transcript recall harness chain

Validates the bench foundation (FakeEmbeddingProvider + ingest + BM25
+ HNSW) before Task 9 builds the runner. #[ignore]'d. Outcome
documented at the top of tests/recall_bench.rs.
EOF
)"
```

---

## Task 2: `eval_metrics.rs` — pure metric functions

**Files:**
- Create: `src/pipeline/eval_metrics.rs`
- Modify: `src/pipeline/mod.rs` (add `pub mod eval_metrics;` alphabetically)

Self-implements 4 IR metrics as pure functions. Per spec §4 + §Q4: Rust has no canonical IR-metric crate; ~150 lines is the right size to write in-tree. Single source of truth for bench scoring.

- [ ] **Step 1: Write the failing tests in `src/pipeline/eval_metrics.rs`**

```rust
//! Information-retrieval evaluation metrics. Pure functions, no I/O.
//!
//! Used by the recall ablation bench (`tests/recall_bench.rs`) and
//! reusable for any future memories pipeline ablation.
//!
//! Conventions:
//! - All functions are tenant-agnostic; pass run + qrels as plain slices/sets.
//! - Generic over `I: Eq + Hash + Clone` so callers pass `String`, `&str`, or
//!   typed wrapper IDs without copying.
//! - Relevance is binary (0/1). gain = 1.0 if id ∈ qrels, else 0.0.

use std::collections::HashSet;
use std::hash::Hash;

/// Discounted cumulative gain over a `gains` list.
/// dcg = Σ gains[i] / log2(i + 2)  (i is 0-indexed)
pub fn dcg(gains: &[f64]) -> f64 {
    gains
        .iter()
        .enumerate()
        .map(|(i, g)| g / ((i + 2) as f64).log2())
        .sum()
}

/// Ideal DCG when there are `relevant_count` relevant docs and we cut at `k`.
pub fn ideal_dcg(relevant_count: usize, k: usize) -> f64 {
    let n = relevant_count.min(k);
    dcg(&vec![1.0; n])
}

/// NDCG@k. Returns 0.0 if qrels is empty (degenerate case).
pub fn ndcg_at_k<I: Eq + Hash + Clone>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    let actual: Vec<f64> = run
        .iter()
        .take(k)
        .map(|id| if qrels.contains(id) { 1.0 } else { 0.0 })
        .collect();
    let actual_dcg = dcg(&actual);
    let ideal = ideal_dcg(qrels.len(), k);
    if ideal == 0.0 {
        0.0
    } else {
        actual_dcg / ideal
    }
}

/// MRR — reciprocal rank of first relevant; 0 if none in run.
pub fn mrr<I: Eq + Hash + Clone>(run: &[I], qrels: &HashSet<I>) -> f64 {
    run.iter()
        .position(|id| qrels.contains(id))
        .map(|p| 1.0 / (p + 1) as f64)
        .unwrap_or(0.0)
}

/// Recall@k — fraction of relevant docs found in top-k.
pub fn recall_at_k<I: Eq + Hash + Clone>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    if qrels.is_empty() {
        return 0.0;
    }
    let hits = run.iter().take(k).filter(|id| qrels.contains(id)).count();
    hits as f64 / qrels.len() as f64
}

/// Precision@k — fraction of top-k that is relevant.
pub fn precision_at_k<I: Eq + Hash + Clone>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let n = run.len().min(k);
    if n == 0 {
        return 0.0;
    }
    let hits = run.iter().take(k).filter(|id| qrels.contains(id)).count();
    hits as f64 / k as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-4,
            "expected {expected}, got {actual}"
        );
    }

    fn qrels(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dcg_handles_empty() {
        assert_eq!(dcg(&[]), 0.0);
    }

    #[test]
    fn dcg_single_relevant_at_position_zero() {
        // gain at position 0 = 1 / log2(2) = 1.0
        approx(dcg(&[1.0]), 1.0);
    }

    #[test]
    fn dcg_three_relevant_handworked() {
        // [1,1,0,1] → 1/log2(2) + 1/log2(3) + 0 + 1/log2(5)
        //          = 1.0 + 0.6309297 + 0 + 0.4306765
        //          = 2.0616062
        approx(dcg(&[1.0, 1.0, 0.0, 1.0]), 2.0616062);
    }

    #[test]
    fn ideal_dcg_caps_at_k() {
        // 5 relevant, k=3 → top 3 all gain=1 → dcg of [1,1,1]
        // = 1/log2(2) + 1/log2(3) + 1/log2(4) = 1 + 0.6309 + 0.5 = 2.1309
        approx(ideal_dcg(5, 3), 2.1309297);
    }

    #[test]
    fn ndcg_at_k_handworked_partial_match() {
        // run=[a,b,c], qrels={a,c}, k=3
        // actual gains = [1,0,1] → dcg = 1/log2(2) + 0 + 1/log2(4) = 1.5
        // ideal:        [1,1]   → dcg = 1/log2(2) + 1/log2(3)     = 1.6309
        // ndcg = 1.5 / 1.6309 = 0.9197
        let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        approx(ndcg_at_k(&run, &qrels(&["a", "c"]), 3), 0.9196731);
    }

    #[test]
    fn ndcg_returns_zero_when_qrels_empty() {
        let run = vec!["a".to_string()];
        assert_eq!(ndcg_at_k(&run, &qrels(&[]), 5), 0.0);
    }

    #[test]
    fn ndcg_returns_one_when_run_is_perfect() {
        // run=[a,b,c], qrels={a,b,c}, k=3 → actual = ideal → 1.0
        let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        approx(ndcg_at_k(&run, &qrels(&["a", "b", "c"]), 3), 1.0);
    }

    #[test]
    fn mrr_first_relevant_at_position_zero() {
        let run = vec!["a".to_string(), "b".to_string()];
        approx(mrr(&run, &qrels(&["a"])), 1.0);
    }

    #[test]
    fn mrr_first_relevant_at_position_two() {
        let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        approx(mrr(&run, &qrels(&["c"])), 1.0 / 3.0);
    }

    #[test]
    fn mrr_no_relevant_returns_zero() {
        let run = vec!["a".to_string(), "b".to_string()];
        assert_eq!(mrr(&run, &qrels(&["x"])), 0.0);
    }

    #[test]
    fn recall_at_k_basic() {
        // run=[a,b], qrels={a,b,c,d}, k=2 → hits=2, denom=4 → 0.5
        let run = vec!["a".to_string(), "b".to_string()];
        approx(recall_at_k(&run, &qrels(&["a", "b", "c", "d"]), 2), 0.5);
    }

    #[test]
    fn recall_at_k_caps_at_k() {
        // run=[a,b,c,d,e], qrels={a,b,c}, k=2 → hits=2, denom=3 → 0.6667
        let run = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        approx(recall_at_k(&run, &qrels(&["a", "b", "c"]), 2), 2.0 / 3.0);
    }

    #[test]
    fn recall_returns_zero_when_qrels_empty() {
        let run = vec!["a".to_string()];
        assert_eq!(recall_at_k(&run, &qrels(&[]), 5), 0.0);
    }

    #[test]
    fn precision_at_k_basic() {
        // run=[a,b,c], qrels={a,c,e}, k=3 → hits=2, k=3 → 0.6667
        let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        approx(precision_at_k(&run, &qrels(&["a", "c", "e"]), 3), 2.0 / 3.0);
    }

    #[test]
    fn precision_at_k_zero_run_returns_zero() {
        let run: Vec<String> = vec![];
        assert_eq!(precision_at_k(&run, &qrels(&["a"]), 5), 0.0);
    }

    #[test]
    fn precision_at_k_zero_k_returns_zero() {
        let run = vec!["a".to_string()];
        assert_eq!(precision_at_k(&run, &qrels(&["a"]), 0), 0.0);
    }

    #[test]
    fn precision_handles_run_shorter_than_k() {
        // run=[a,b], qrels={a,b}, k=5 → hits=2, k=5 → 2/5 = 0.4
        let run = vec!["a".to_string(), "b".to_string()];
        approx(precision_at_k(&run, &qrels(&["a", "b"]), 5), 0.4);
    }
}
```

- [ ] **Step 2: Register module in `src/pipeline/mod.rs`**

Add `pub mod eval_metrics;` in alphabetical position (between `entity_normalize` and `ingest`).

- [ ] **Step 3: Run tests**

```bash
cargo test --lib pipeline::eval_metrics -q
```

Expected: 18 passed, 0 failed.

- [ ] **Step 4: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/pipeline/eval_metrics.rs src/pipeline/mod.rs
git commit -m "$(cat <<'EOF'
feat(metrics): self-implement IR eval metrics (NDCG@k, MRR, R@k, P@k)

Pure functions in src/pipeline/eval_metrics.rs with 18 unit tests
covering hand-worked reference values. Used by tests/recall_bench.rs
and reusable for future memories pipeline ablation.
EOF
)"
```

---

## Task 3: Extend `ScoringOpts` with disable flags

**Files:**
- Modify: `src/pipeline/transcript_recall.rs` (struct + score_candidates body)

`ScoringOpts<'a>` currently has only `anchor_session_id: Option<&'a str>`. We add three `disable_*: bool` fields (default false → zero behavior change for production callers) so the bench runner can switch signals off per-rung. Each bonus addition in `score_candidates` gets an `if !opts.disable_*` guard.

- [ ] **Step 1: Write the failing test (before changing the struct)**

Append to `src/pipeline/transcript_recall.rs`'s `#[cfg(test)] mod tests`:

```rust
#[test]
fn disable_session_cooc_zeroes_that_bonus() {
    // Two siblings in same session — without disabling, cooc bonus = 1*per_sibling
    // = 3. With disable_session_cooc=true, bonus is 0. Verify the score delta.
    let m_a = ConversationMessage {
        message_id: "a1".to_string(),
        tenant: "t".to_string(),
        session_id: Some("s1".to_string()),
        block_type: "text".to_string(),
        embed_eligible: true,
        content: "x".to_string(),
        created_at: "00000000020260503000".to_string(),
        ..Default::default()
    };
    let m_b = ConversationMessage {
        message_id: "b1".to_string(),
        ..m_a.clone()
    };

    let with_cooc = score_candidates(
        vec![m_a.clone(), m_b.clone()],
        &HashMap::new(),
        &HashMap::new(),
        ScoringOpts::default(),
    );
    let without_cooc = score_candidates(
        vec![m_a, m_b],
        &HashMap::new(),
        &HashMap::new(),
        ScoringOpts {
            disable_session_cooc: true,
            ..ScoringOpts::default()
        },
    );
    assert!(
        with_cooc[0].score > without_cooc[0].score,
        "disabling cooc must lower score (got {} vs {})",
        with_cooc[0].score,
        without_cooc[0].score
    );
    assert_eq!(
        with_cooc[0].score - without_cooc[0].score,
        SESSION_COOCC_PER_SIBLING,
        "cooc bonus delta should equal SESSION_COOCC_PER_SIBLING"
    );
}

#[test]
fn disable_anchor_zeroes_anchor_bonus() {
    let m = ConversationMessage {
        message_id: "a1".to_string(),
        tenant: "t".to_string(),
        session_id: Some("s_anchor".to_string()),
        block_type: "text".to_string(),
        embed_eligible: true,
        content: "x".to_string(),
        created_at: "00000000020260503000".to_string(),
        ..Default::default()
    };
    let with_anchor = score_candidates(
        vec![m.clone()],
        &HashMap::new(),
        &HashMap::new(),
        ScoringOpts {
            anchor_session_id: Some("s_anchor"),
            ..ScoringOpts::default()
        },
    );
    let disabled = score_candidates(
        vec![m],
        &HashMap::new(),
        &HashMap::new(),
        ScoringOpts {
            anchor_session_id: Some("s_anchor"),
            disable_anchor: true,
            ..ScoringOpts::default()
        },
    );
    assert_eq!(
        with_anchor[0].score - disabled[0].score,
        ANCHOR_SESSION_BONUS
    );
}

#[test]
fn disable_freshness_zeroes_freshness_bonus() {
    // Two timestamps; the older one's freshness < newer one's. Both should
    // converge to the same score when disable_freshness=true.
    let now = "00000000020260503000";
    let older = "00000000020260403000";
    let m_new = ConversationMessage {
        message_id: "new".to_string(),
        tenant: "t".to_string(),
        block_type: "text".to_string(),
        embed_eligible: true,
        content: "x".to_string(),
        created_at: now.to_string(),
        ..Default::default()
    };
    let m_old = ConversationMessage {
        message_id: "old".to_string(),
        created_at: older.to_string(),
        ..m_new.clone()
    };
    let with_fresh = score_candidates(
        vec![m_new.clone(), m_old.clone()],
        &HashMap::new(),
        &HashMap::new(),
        ScoringOpts::default(),
    );
    let without_fresh = score_candidates(
        vec![m_new, m_old],
        &HashMap::new(),
        &HashMap::new(),
        ScoringOpts {
            disable_freshness: true,
            ..ScoringOpts::default()
        },
    );
    let with_diff = with_fresh
        .iter()
        .find(|s| s.message.message_id == "new")
        .unwrap()
        .score
        - with_fresh
            .iter()
            .find(|s| s.message.message_id == "old")
            .unwrap()
            .score;
    let without_diff = without_fresh
        .iter()
        .find(|s| s.message.message_id == "new")
        .unwrap()
        .score
        - without_fresh
            .iter()
            .find(|s| s.message.message_id == "old")
            .unwrap()
            .score;
    assert!(with_diff > 0, "with freshness, newer must outrank older");
    assert_eq!(
        without_diff, 0,
        "with freshness disabled, both candidates must score equally"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --lib pipeline::transcript_recall::tests::disable_ -q
```

Expected: 3 errors with "no field named `disable_session_cooc`" / "disable_anchor" / "disable_freshness".

- [ ] **Step 3: Extend the struct**

Replace the `ScoringOpts` definition in `src/pipeline/transcript_recall.rs:35-37` with:

```rust
/// Optional per-call options.
///
/// Default values produce production behavior (no signals disabled). Bench
/// callers (`tests/recall_bench.rs`) toggle the `disable_*` fields per-rung
/// to measure each signal's marginal contribution.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScoringOpts<'a> {
    pub anchor_session_id: Option<&'a str>,
    pub disable_session_cooc: bool,
    pub disable_anchor: bool,
    pub disable_freshness: bool,
}
```

- [ ] **Step 4: Guard each bonus in `score_candidates`**

In the body of `score_candidates`, find the three bonus additions (search for `session_cooccurrence_bonus`, `anchor_session_bonus`, `freshness_score`). Wrap each with `if !opts.disable_*`:

```rust
// Pseudocode for the pattern; adapt to actual variable names:
if !opts.disable_session_cooc {
    score += session_cooccurrence_bonus(...);
}
if !opts.disable_anchor {
    score += anchor_session_bonus(...);
}
if !opts.disable_freshness {
    score += freshness_score(...);
}
```

- [ ] **Step 5: Run new tests + full test suite**

```bash
cargo test --lib pipeline::transcript_recall -q
cargo test -q
```

Expected: all transcript_recall unit tests pass (existing + 3 new); full suite passes (no regression — three new flags default false).

- [ ] **Step 6: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 7: Commit**

```bash
git add src/pipeline/transcript_recall.rs
git commit -m "$(cat <<'EOF'
refactor(transcript): extend ScoringOpts with bench disable flags

Adds disable_session_cooc / disable_anchor / disable_freshness bools
(default false → zero behavior change for production). Each bonus in
score_candidates is now guarded by its disable flag so the recall
ablation bench can switch signals off per-rung.
EOF
)"
```

---

## Task 4: Fixture types

**Files:**
- Create: `tests/bench/mod.rs`
- Create: `tests/bench/fixture.rs`

Defines the data structs that fixture generators (Task 5/6) and the runner (Task 9) consume. Pure data, no logic.

- [ ] **Step 1: Create `tests/bench/mod.rs`**

```rust
//! Test-only helpers for the recall ablation bench. See
//! `docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md`.
//!
//! This module is loaded via `mod bench;` from `tests/recall_bench.rs`.
#![allow(dead_code)] // submodules build incrementally; some helpers land before callers.

pub mod fixture;
pub mod judgment;
pub mod oracle;
pub mod real;
pub mod runner;
pub mod synthetic;
```

(Submodules are filled in by Tasks 5–9. The `#![allow(dead_code)]` covers the build window; remove it in Task 11 once everything is wired.)

- [ ] **Step 2: Create `tests/bench/fixture.rs` with types**

```rust
//! Fixture data structures shared by synthetic + real loaders and consumed
//! by the bench runner.

use std::collections::{HashMap, HashSet};

pub type QueryId = String;
pub type SessionId = String;
pub type JudgmentMap = HashMap<QueryId, HashSet<SessionId>>;

#[derive(Debug, Clone)]
pub struct BlockFixture {
    pub block_id: String,
    pub role: String,        // "user" or "assistant"
    pub block_type: String,  // "text" / "thinking" — bench skips tool blocks
    pub content: String,
    pub created_at: String,  // sortable timestamp string ("00000000020260503000")
}

#[derive(Debug, Clone)]
pub struct SessionFixture {
    pub session_id: SessionId,
    pub started_at: String,
    pub blocks: Vec<BlockFixture>,
}

#[derive(Debug, Clone)]
pub struct QueryFixture {
    pub query_id: QueryId,
    pub text: String,
    pub anchor_session_id: Option<SessionId>,
    /// Aliases this query is "about". Used by judgment derivation.
    pub anchor_entities: Vec<String>,
    /// Pre-computed (synthetic) judgments. `None` for real fixtures
    /// (judgment.rs will derive via entity registry).
    pub synthetic_judgments: Option<HashSet<SessionId>>,
}

#[derive(Debug, Clone)]
pub struct Fixture {
    pub kind: FixtureKind,
    pub tenant: String,
    pub sessions: Vec<SessionFixture>,
    pub queries: Vec<QueryFixture>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureKind {
    Synthetic { seed: u64 },
    Real,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_can_be_constructed_empty() {
        let f = Fixture {
            kind: FixtureKind::Synthetic { seed: 0 },
            tenant: "t".to_string(),
            sessions: vec![],
            queries: vec![],
        };
        assert_eq!(f.tenant, "t");
        assert_eq!(f.sessions.len(), 0);
    }
}
```

- [ ] **Step 3: Wire mod into `tests/recall_bench.rs`**

At the top of `tests/recall_bench.rs` (above the existing probe), add:

```rust
mod bench;
```

- [ ] **Step 4: Run tests**

```bash
cargo test --test recall_bench -q
```

Expected: probe ignored, fixture::tests::fixture_can_be_constructed_empty passes.

- [ ] **Step 5: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add tests/bench/mod.rs tests/bench/fixture.rs tests/recall_bench.rs
git commit -m "$(cat <<'EOF'
feat(bench): fixture data types for recall ablation

Defines Fixture / SessionFixture / BlockFixture / QueryFixture /
JudgmentMap / FixtureKind. Pure data, no logic. Consumed by Task 5
(synthetic), Task 6 (real loader), Task 7 (judgments), Task 9 (runner).
EOF
)"
```

---

## Task 5: Synthetic fixture generator

**Files:**
- Create: `tests/bench/synthetic.rs`
- Modify: `Cargo.toml` if `rand` isn't already a dev-dependency

The generator produces a deterministic in-tree fixture (seed → bit-exact). 30 sessions × 8 blocks × 24 queries by default; CI runs ~5s.

- [ ] **Step 1: Verify `rand` dev-dependency**

Check `Cargo.toml`:
```bash
grep -A2 'dev-dependencies' Cargo.toml | head -20
```

If `rand` is not under `[dev-dependencies]` (it's likely under `[dependencies]` already; if so it's also usable from test crates), no action. If neither, add to `[dev-dependencies]`:

```toml
rand = "0.8"
```

- [ ] **Step 2: Write the failing test in `tests/bench/synthetic.rs`**

```rust
//! Synthetic fixture generator. Deterministic given (seed, config).

use super::fixture::*;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use std::collections::HashSet;

pub struct TopicSeed {
    pub canonical: &'static str,
    pub aliases: &'static [&'static str],
}

pub const DEFAULT_TOPICS: &[TopicSeed] = &[
    TopicSeed { canonical: "Rust async", aliases: &["tokio", "futures", "await"] },
    TopicSeed { canonical: "DuckDB", aliases: &["duckdb", "olap", "columnar"] },
    TopicSeed { canonical: "HNSW", aliases: &["usearch", "ann", "vector index"] },
    TopicSeed { canonical: "BM25", aliases: &["fts", "tantivy", "lexical"] },
    TopicSeed { canonical: "session window", aliases: &["sliding", "bucket", "auto-bucket"] },
    TopicSeed { canonical: "ranking", aliases: &["rrf", "fusion", "reranker"] },
    TopicSeed { canonical: "embedding", aliases: &["vector", "encoder", "dense"] },
    TopicSeed { canonical: "MCP", aliases: &["model context protocol", "stdio", "json-rpc"] },
    TopicSeed { canonical: "axum", aliases: &["http", "router", "tokio runtime"] },
    TopicSeed { canonical: "schema migration", aliases: &["alter table", "ddl", "ddl drift"] },
    TopicSeed { canonical: "graph edges", aliases: &["valid_from", "supersedes", "bitemporal"] },
    TopicSeed { canonical: "cross-encoder", aliases: &["bge-reranker", "ms-marco", "rerank model"] },
];

const NOISE_WORDS: &[&str] = &[
    "the", "in", "and", "to", "that", "with", "for", "is", "are", "we",
    "this", "they", "their", "from", "after", "before", "should", "would",
    "could", "discuss", "consider", "regarding", "notes", "context",
];

pub struct SyntheticConfig {
    pub seed: u64,
    pub session_count: usize,
    pub blocks_per_session: usize,
    pub topic_pool: &'static [TopicSeed],
    pub query_count: usize,
    pub noise_words_per_block: usize,
    pub tenant: &'static str,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            session_count: 30,
            blocks_per_session: 8,
            topic_pool: DEFAULT_TOPICS,
            query_count: 24,
            noise_words_per_block: 30,
            tenant: "local",
        }
    }
}

pub fn generate(config: &SyntheticConfig) -> Fixture {
    let mut rng = StdRng::seed_from_u64(config.seed);

    // Step 1: Generate sessions, each tagged with 1-2 topic indices.
    let mut sessions: Vec<SessionFixture> = Vec::with_capacity(config.session_count);
    let mut session_topics: Vec<Vec<usize>> = Vec::with_capacity(config.session_count);

    for s_idx in 0..config.session_count {
        let topics_n = if rng.gen_bool(0.5) { 1 } else { 2 };
        let mut chosen: Vec<usize> = (0..config.topic_pool.len()).collect();
        chosen.shuffle(&mut rng);
        let topics: Vec<usize> = chosen.into_iter().take(topics_n).collect();
        session_topics.push(topics.clone());

        // 90 days span → each session gets a base offset; blocks within session monotonic.
        let base_day = rng.gen_range(0..90u64);
        let session_id = format!("synth_session_{:03}", s_idx);
        let started_at = format_timestamp(2026 - 0, 5, 3, base_day, 0);

        let mut blocks: Vec<BlockFixture> = Vec::with_capacity(config.blocks_per_session);
        for b_idx in 0..config.blocks_per_session {
            // Pick topic for this block (round-robin from session's topics).
            let topic_idx = topics[b_idx % topics.len()];
            let topic = &config.topic_pool[topic_idx];
            let term = if rng.gen_bool(0.4) {
                topic.canonical.to_string()
            } else {
                topic.aliases[rng.gen_range(0..topic.aliases.len())].to_string()
            };

            // Build content: shuffled mix of noise words + the term.
            let mut content_words: Vec<String> = (0..config.noise_words_per_block)
                .map(|_| NOISE_WORDS[rng.gen_range(0..NOISE_WORDS.len())].to_string())
                .collect();
            let insert_pos = rng.gen_range(0..=content_words.len());
            content_words.insert(insert_pos, term);
            let content = content_words.join(" ");

            let role = if b_idx % 2 == 0 { "user" } else { "assistant" };
            let created_at = format_timestamp(2026, 5, 3, base_day, b_idx as u64);

            blocks.push(BlockFixture {
                block_id: format!("synth_{:03}_{:02}", s_idx, b_idx),
                role: role.to_string(),
                block_type: "text".to_string(),
                content,
                created_at,
            });
        }

        sessions.push(SessionFixture {
            session_id,
            started_at,
            blocks,
        });
    }

    // Step 2: Generate queries. Each picks a topic (uniform). The query text
    // is "how do I use <alias> for <canonical>?".
    let mut queries: Vec<QueryFixture> = Vec::with_capacity(config.query_count);
    for q_idx in 0..config.query_count {
        let topic_idx = rng.gen_range(0..config.topic_pool.len());
        let topic = &config.topic_pool[topic_idx];
        let alias = topic.aliases[rng.gen_range(0..topic.aliases.len())];
        let text = format!("how do I use {} for {} in production?", alias, topic.canonical);

        // Synthetic judgments: any session whose topic list includes this topic.
        let synthetic_judgments: HashSet<String> = session_topics
            .iter()
            .enumerate()
            .filter(|(_, topics)| topics.contains(&topic_idx))
            .map(|(s_idx, _)| format!("synth_session_{:03}", s_idx))
            .collect();

        queries.push(QueryFixture {
            query_id: format!("synth_q_{:03}", q_idx),
            text,
            anchor_session_id: None,
            anchor_entities: vec![topic.canonical.to_string()],
            synthetic_judgments: Some(synthetic_judgments),
        });
    }

    Fixture {
        kind: FixtureKind::Synthetic { seed: config.seed },
        tenant: config.tenant.to_string(),
        sessions,
        queries,
    }
}

/// Compose a sortable timestamp string compatible with the project's
/// `timestamp_score` parser (numeric prefix, microsecond resolution).
fn format_timestamp(_year: u64, _month: u64, _day: u64, day_offset: u64, seq: u64) -> String {
    // Project format: "00000000<unix-ms>". Generate a synthetic monotonic
    // millisecond by combining day_offset and seq.
    let ms = 1_700_000_000_000_u64 + day_offset * 86_400_000 + seq * 1_000;
    format!("{:020}", ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_produces_30_sessions_240_blocks_24_queries() {
        let f = generate(&SyntheticConfig::default());
        assert_eq!(f.sessions.len(), 30);
        assert_eq!(f.sessions.iter().map(|s| s.blocks.len()).sum::<usize>(), 240);
        assert_eq!(f.queries.len(), 24);
        assert_eq!(f.tenant, "local");
        assert!(matches!(f.kind, FixtureKind::Synthetic { seed: 42 }));
    }

    #[test]
    fn generation_is_deterministic_for_same_seed() {
        let f1 = generate(&SyntheticConfig::default());
        let f2 = generate(&SyntheticConfig::default());
        assert_eq!(f1.sessions[0].blocks[0].content, f2.sessions[0].blocks[0].content);
        assert_eq!(f1.queries[0].text, f2.queries[0].text);
    }

    #[test]
    fn different_seeds_produce_different_content() {
        let f1 = generate(&SyntheticConfig::default());
        let f2 = generate(&SyntheticConfig {
            seed: 999,
            ..SyntheticConfig::default()
        });
        // At least one of these should differ.
        assert!(
            f1.sessions[0].blocks[0].content != f2.sessions[0].blocks[0].content
                || f1.queries[0].text != f2.queries[0].text
        );
    }

    #[test]
    fn synthetic_judgments_are_populated() {
        let f = generate(&SyntheticConfig::default());
        for q in &f.queries {
            let j = q.synthetic_judgments.as_ref().expect("synthetic judgments must be Some");
            assert!(!j.is_empty(), "every synthetic query should have ≥1 relevant session");
        }
    }

    #[test]
    fn anchor_entities_match_topic_canonical_names() {
        let f = generate(&SyntheticConfig::default());
        let canonicals: Vec<&str> = DEFAULT_TOPICS.iter().map(|t| t.canonical).collect();
        for q in &f.queries {
            assert_eq!(q.anchor_entities.len(), 1);
            assert!(canonicals.contains(&q.anchor_entities[0].as_str()));
        }
    }

    #[test]
    fn block_content_contains_topic_term() {
        // Take the first session's first block; its topic is session_topics[0][0],
        // and the content must contain canonical OR one of the aliases.
        let f = generate(&SyntheticConfig::default());
        let first_block = &f.sessions[0].blocks[0];
        let any_topic_hit = DEFAULT_TOPICS.iter().any(|t| {
            first_block.content.contains(t.canonical)
                || t.aliases.iter().any(|a| first_block.content.contains(a))
        });
        assert!(any_topic_hit, "content should embed at least one topic term");
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --test recall_bench bench::synthetic -q
```

Expected: 6 passed.

- [ ] **Step 4: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add tests/bench/synthetic.rs Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
feat(bench): synthetic fixture generator (deterministic, 30×8×24)

Default config: seed=42, 30 sessions × 8 blocks × 24 queries with 12
topics. Bit-exact reproducible. Pre-computes synthetic_judgments by
topic linkage so judgment derivation can short-circuit on synthetic
fixtures (entity registry only used for real fixtures).
EOF
)"
```

---

## Task 6: Real fixture loader

**Files:**
- Create: `tests/bench/real.rs`
- Modify: `.gitignore`

Loader for gitignored real fixtures. Env-var path; missing var → return Ok(None) for caller to skip; bad version → panic with clear message.

- [ ] **Step 1: Add gitignore entry**

Add to `.gitignore` (at end, in a "Bench fixtures" section):

```
# Recall bench fixtures (gitignored — real transcripts may be sensitive)
bench-fixtures/
target/bench-out/
```

- [ ] **Step 2: Write the failing tests in `tests/bench/real.rs`**

```rust
//! Real fixture loader. Reads gitignored JSON file at env-var path.
//! Returns `Ok(None)` if env var is unset (callers skip silently).

use super::fixture::*;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
struct RealFixtureFile {
    loader_version: u32,
    tenant: String,
    sessions: Vec<RealSession>,
    queries: Vec<RealQuery>,
}

#[derive(Debug, Deserialize)]
struct RealSession {
    session_id: String,
    started_at: String,
    blocks: Vec<RealBlock>,
}

#[derive(Debug, Deserialize)]
struct RealBlock {
    block_id: String,
    role: String,
    block_type: String,
    content: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct RealQuery {
    query_id: String,
    text: String,
    #[serde(default)]
    anchor_session_id: Option<String>,
    #[serde(default)]
    anchor_entities: Vec<String>,
}

/// Load the real fixture if `MEM_BENCH_FIXTURE_PATH` is set.
/// Returns `Ok(None)` when env var is unset.
/// Panics with a clear message if the file is missing or invalid.
pub fn load_from_env() -> std::io::Result<Option<Fixture>> {
    let path = match std::env::var("MEM_BENCH_FIXTURE_PATH") {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    Ok(Some(load_from_path(Path::new(&path))?))
}

pub fn load_from_path(path: &Path) -> std::io::Result<Fixture> {
    let bytes = std::fs::read(path)?;
    let raw: RealFixtureFile = serde_json::from_slice(&bytes).expect("invalid JSON in real fixture");
    if raw.loader_version != SCHEMA_VERSION {
        panic!(
            "real fixture loader_version mismatch: file says {}, code expects {}. \
             Re-export the fixture or upgrade the loader.",
            raw.loader_version, SCHEMA_VERSION
        );
    }
    let sessions: Vec<SessionFixture> = raw
        .sessions
        .into_iter()
        .map(|rs| SessionFixture {
            session_id: rs.session_id,
            started_at: rs.started_at,
            blocks: rs
                .blocks
                .into_iter()
                .map(|rb| BlockFixture {
                    block_id: rb.block_id,
                    role: rb.role,
                    block_type: rb.block_type,
                    content: rb.content,
                    created_at: rb.created_at,
                })
                .collect(),
        })
        .collect();
    let queries: Vec<QueryFixture> = raw
        .queries
        .into_iter()
        .map(|rq| QueryFixture {
            query_id: rq.query_id,
            text: rq.text,
            anchor_session_id: rq.anchor_session_id,
            anchor_entities: rq.anchor_entities,
            synthetic_judgments: None, // judgment.rs derives via entity registry
        })
        .collect();
    Ok(Fixture {
        kind: FixtureKind::Real,
        tenant: raw.tenant,
        sessions,
        queries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture(json: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn load_from_env_returns_none_when_unset() {
        // Save then unset env var to ensure clean state.
        let original = std::env::var("MEM_BENCH_FIXTURE_PATH").ok();
        std::env::remove_var("MEM_BENCH_FIXTURE_PATH");
        let res = load_from_env().unwrap();
        assert!(res.is_none());
        if let Some(v) = original {
            std::env::set_var("MEM_BENCH_FIXTURE_PATH", v);
        }
    }

    #[test]
    fn load_from_path_parses_minimal_valid_file() {
        let json = r#"{
            "loader_version": 1,
            "tenant": "local",
            "sessions": [{
                "session_id": "s1",
                "started_at": "00000000020260503000",
                "blocks": [{
                    "block_id": "b1",
                    "role": "user",
                    "block_type": "text",
                    "content": "hello",
                    "created_at": "00000000020260503000"
                }]
            }],
            "queries": [{
                "query_id": "q1",
                "text": "hi",
                "anchor_entities": ["greeting"]
            }]
        }"#;
        let f = write_fixture(json);
        let fixture = load_from_path(f.path()).unwrap();
        assert_eq!(fixture.tenant, "local");
        assert_eq!(fixture.sessions.len(), 1);
        assert_eq!(fixture.queries.len(), 1);
        assert_eq!(fixture.queries[0].anchor_entities, vec!["greeting"]);
        assert!(fixture.queries[0].synthetic_judgments.is_none());
        assert_eq!(fixture.kind, FixtureKind::Real);
    }

    #[test]
    #[should_panic(expected = "loader_version mismatch")]
    fn wrong_version_panics() {
        let json = r#"{
            "loader_version": 99,
            "tenant": "local",
            "sessions": [],
            "queries": []
        }"#;
        let f = write_fixture(json);
        let _ = load_from_path(f.path()).unwrap();
    }

    #[test]
    fn missing_anchor_session_id_defaults_to_none() {
        let json = r#"{
            "loader_version": 1,
            "tenant": "local",
            "sessions": [],
            "queries": [{
                "query_id": "q1",
                "text": "hi",
                "anchor_entities": []
            }]
        }"#;
        let f = write_fixture(json);
        let fixture = load_from_path(f.path()).unwrap();
        assert_eq!(fixture.queries[0].anchor_session_id, None);
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --test recall_bench bench::real -q
```

Expected: 4 passed.

- [ ] **Step 4: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add tests/bench/real.rs .gitignore
git commit -m "$(cat <<'EOF'
feat(bench): real fixture loader with version + env-var gating

Loads JSON-serialized fixtures from MEM_BENCH_FIXTURE_PATH; returns
Ok(None) when env var is unset so the test entry can skip silently.
loader_version mismatch panics loudly to prevent silent schema drift.
bench-fixtures/ added to .gitignore.
EOF
)"
```

---

## Task 7: Judgment derivation

**Files:**
- Create: `tests/bench/judgment.rs`

Two paths inside one fn: synthetic queries already carry pre-computed `synthetic_judgments`; real queries derive via `EntityRegistry::resolve_or_create` + content scan.

- [ ] **Step 1: Write the failing tests in `tests/bench/judgment.rs`**

```rust
//! Judgment derivation. Synthetic path uses pre-computed
//! `synthetic_judgments`; real path resolves anchor_entities via
//! EntityRegistry and scans session content.

use super::fixture::*;
use mem::domain::EntityKind;
use mem::pipeline::entity_normalize::normalize_alias;
use mem::storage::EntityRegistry;
use std::collections::HashSet;

pub async fn derive_judgments(
    fixture: &Fixture,
    registry: &dyn EntityRegistry,
    now: &str,
) -> JudgmentMap {
    let mut judgments: JudgmentMap = std::collections::HashMap::new();

    for query in &fixture.queries {
        // Synthetic: use pre-computed judgments verbatim.
        if let Some(synth) = &query.synthetic_judgments {
            judgments.insert(query.query_id.clone(), synth.clone());
            continue;
        }

        // Real: resolve each anchor alias, then scan for any matching alias in content.
        let mut entity_ids: HashSet<String> = HashSet::new();
        for alias in &query.anchor_entities {
            let id = registry
                .resolve_or_create(&fixture.tenant, alias, EntityKind::Topic, now)
                .await
                .expect("registry resolve_or_create");
            entity_ids.insert(id);
        }

        let mut relevant = HashSet::new();
        for session in &fixture.sessions {
            if session_mentions_any_alias_of(session, &entity_ids, registry, &fixture.tenant).await
            {
                relevant.insert(session.session_id.clone());
            }
        }
        judgments.insert(query.query_id.clone(), relevant);
    }
    judgments
}

async fn session_mentions_any_alias_of(
    session: &SessionFixture,
    target_entity_ids: &HashSet<String>,
    registry: &dyn EntityRegistry,
    tenant: &str,
) -> bool {
    // Tokenize whole session content (text + thinking only — bench skips tool blocks).
    let mut tokens: Vec<String> = Vec::new();
    for block in &session.blocks {
        if matches!(block.block_type.as_str(), "text" | "thinking") {
            for tok in block.content.split_whitespace() {
                tokens.push(tok.to_string());
            }
        }
    }
    // Also try multi-word phrases (up to 3 grams) since aliases like "Rust async" are 2 words.
    for window_size in 1..=3 {
        for window in tokens.windows(window_size) {
            let phrase = window.join(" ");
            let normalized = normalize_alias(&phrase);
            if normalized.is_empty() {
                continue;
            }
            if let Ok(Some(entity_id)) = registry.lookup_alias(tenant, &normalized).await {
                if target_entity_ids.contains(&entity_id) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem::config::Config;
    use mem::storage::DuckDbRepository;
    use std::collections::HashSet;

    fn synthetic_fixture() -> Fixture {
        // Tiny one-session fixture with a pre-baked judgment.
        let mut precomp = HashSet::new();
        precomp.insert("s1".to_string());
        Fixture {
            kind: FixtureKind::Synthetic { seed: 0 },
            tenant: "t".to_string(),
            sessions: vec![SessionFixture {
                session_id: "s1".to_string(),
                started_at: "00000000020260503000".to_string(),
                blocks: vec![BlockFixture {
                    block_id: "b1".to_string(),
                    role: "user".to_string(),
                    block_type: "text".to_string(),
                    content: "Tokio runtime async Rust".to_string(),
                    created_at: "00000000020260503000".to_string(),
                }],
            }],
            queries: vec![QueryFixture {
                query_id: "q1".to_string(),
                text: "Rust async".to_string(),
                anchor_session_id: None,
                anchor_entities: vec!["Rust async".to_string()],
                synthetic_judgments: Some(precomp),
            }],
        }
    }

    #[tokio::test]
    async fn synthetic_path_uses_precomputed_judgments() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = Config::local_with_db_path(tmp.path().join("j.duckdb"));
        let repo = DuckDbRepository::open(&cfg).await.unwrap();
        let f = synthetic_fixture();
        let j = derive_judgments(&f, &repo, "00000000020260503000").await;
        assert_eq!(j.get("q1").unwrap().len(), 1);
        assert!(j.get("q1").unwrap().contains("s1"));
    }

    #[tokio::test]
    async fn real_path_derives_via_registry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = Config::local_with_db_path(tmp.path().join("j.duckdb"));
        let repo = DuckDbRepository::open(&cfg).await.unwrap();

        // Pre-populate the entity registry with a "Rust async" entity + alias "tokio".
        let _ = repo
            .resolve_or_create("t", "Rust async", EntityKind::Topic, "00000000020260503000")
            .await
            .unwrap();
        let entity_id = repo
            .resolve_or_create("t", "tokio", EntityKind::Topic, "00000000020260503000")
            .await
            .unwrap();
        // For a clean test, link both aliases to the same entity. If
        // resolve_or_create makes them different entities, add explicit
        // alias linkage:
        // repo.add_alias("t", &entity_id_a, "tokio", "00000000020260503000").await;
        let _ = entity_id; // suppress unused warning if linkage is implicit

        let mut fixture = synthetic_fixture();
        fixture.kind = FixtureKind::Real;
        fixture.queries[0].synthetic_judgments = None; // force real path
        fixture.queries[0].anchor_entities = vec!["tokio".to_string()];
        // Block content already contains "Tokio" — normalize_alias lowercases it.

        let j = derive_judgments(&fixture, &repo, "00000000020260503000").await;
        // The session's content contains "Tokio"; normalize_alias("Tokio") = "tokio";
        // registry.lookup_alias should resolve to the entity created above.
        assert!(
            j.get("q1").unwrap().contains("s1"),
            "real path should mark s1 relevant via tokio→Rust async link, got: {:?}",
            j.get("q1")
        );
    }
}
```

**Implementer note:** the `real_path_derives_via_registry` test depends on `resolve_or_create("Rust async") + resolve_or_create("tokio")` either landing on the same entity (if the alias chain auto-links them) or you explicitly calling `add_alias` to link "tokio" to the "Rust async" entity. Read the entity-registry implementation around `tests/entity_registry.rs:148` to see the established alias-linkage idiom; adapt this test as needed.

- [ ] **Step 2: Run tests**

```bash
cargo test --test recall_bench bench::judgment -q
```

Expected: 2 passed (synthetic + real path).

- [ ] **Step 3: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 4: Commit**

```bash
git add tests/bench/judgment.rs
git commit -m "$(cat <<'EOF'
feat(bench): judgment derivation (synthetic precomp + real co-mention)

Synthetic queries use pre-computed synthetic_judgments verbatim. Real
queries resolve anchor_entities via EntityRegistry and scan session
content for any alias of the resolved entities (1-3 grams).
EOF
)"
```

---

## Task 8: Oracle rerank

**Files:**
- Create: `tests/bench/oracle.rs`

Tiny pure function. Spec §7: partition by relevance, relevant first, irrelevant after; preserve internal order in each partition.

- [ ] **Step 1: Write the failing tests in `tests/bench/oracle.rs`**

```rust
//! Oracle "perfect filter" reranker. Partitions a run into relevant +
//! irrelevant; relevant comes first, in original score order; irrelevant
//! follows, in original order. Spec §7.

use std::collections::HashSet;
use std::hash::Hash;

pub fn oracle_rerank<I: Eq + Hash + Clone>(run: Vec<I>, qrels: &HashSet<I>) -> Vec<I> {
    let (rel, irrel): (Vec<I>, Vec<I>) = run.into_iter().partition(|id| qrels.contains(id));
    [rel, irrel].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qrels(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn oracle_promotes_relevant_to_front() {
        let run = vec!["x".to_string(), "a".to_string(), "y".to_string(), "b".to_string()];
        let result = oracle_rerank(run, &qrels(&["a", "b"]));
        assert_eq!(result, vec!["a", "b", "x", "y"]);
    }

    #[test]
    fn oracle_preserves_relative_order_within_partitions() {
        let run = vec!["a".to_string(), "x".to_string(), "b".to_string(), "y".to_string()];
        let result = oracle_rerank(run, &qrels(&["a", "b"]));
        assert_eq!(result, vec!["a", "b", "x", "y"]); // a before b; x before y
    }

    #[test]
    fn oracle_handles_all_relevant() {
        let run = vec!["a".to_string(), "b".to_string()];
        let result = oracle_rerank(run, &qrels(&["a", "b"]));
        assert_eq!(result, vec!["a", "b"]);
    }

    #[test]
    fn oracle_handles_none_relevant() {
        let run = vec!["x".to_string(), "y".to_string()];
        let result = oracle_rerank(run, &qrels(&["a"]));
        assert_eq!(result, vec!["x", "y"]);
    }

    #[test]
    fn oracle_handles_empty_run() {
        let run: Vec<String> = vec![];
        let result = oracle_rerank(run, &qrels(&["a"]));
        assert!(result.is_empty());
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test --test recall_bench bench::oracle -q
```

Expected: 5 passed.

- [ ] **Step 3: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 4: Commit**

```bash
git add tests/bench/oracle.rs
git commit -m "$(cat <<'EOF'
feat(bench): oracle rerank (perfect-filter binary reranker)

Partitions run into relevant + irrelevant by qrels membership;
relevant first, both partitions preserve original order. Represents
the upper bound for any binary cross-encoder reranker (spec §7).
EOF
)"
```

---

## Task 9: Runner (the substantive task)

**Files:**
- Create: `tests/bench/runner.rs`

Orchestrates: spin up DuckDB + entity registry per rung, ingest fixture, run retrieve+score+rerank under each rung, eval metrics, aggregate to RungReport.

This is the longest task. The implementer should pace themselves; each step's code is self-contained.

- [ ] **Step 1: Write the runner skeleton (no test yet — too large to TDD as one piece; we test via the full bench in Task 11)**

Create `tests/bench/runner.rs`:

```rust
//! Ablation runner. Loads a Fixture, ingests it into a fresh DuckDB,
//! runs each Rung (config tuple), aggregates RungReport per rung.

use super::fixture::*;
use super::judgment::derive_judgments;
use super::oracle::oracle_rerank;
use mem::config::Config;
use mem::domain::{ConversationMessage, EntityKind};
use mem::embedding::FakeEmbeddingProvider;
use mem::pipeline::eval_metrics::*;
use mem::pipeline::transcript_recall::{score_candidates, ScoringOpts};
use mem::storage::{DuckDbRepository, EntityRegistry};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const TOP_K: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceMix {
    Bm25Only,
    HnswOnly,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankPolicy {
    None,
    OracleByJudgment,
}

#[derive(Debug, Clone, Copy)]
pub struct Rung {
    pub name: &'static str,
    pub source: SourceMix,
    pub disable_session_cooc: bool,
    pub disable_anchor: bool,
    pub disable_freshness: bool,
    pub rerank: RerankPolicy,
}

pub const RUNGS: &[Rung] = &[
    Rung { name: "bm25-only",           source: SourceMix::Bm25Only, disable_session_cooc: true,  disable_anchor: true,  disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "hnsw-only",           source: SourceMix::HnswOnly, disable_session_cooc: true,  disable_anchor: true,  disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "hybrid-rrf",          source: SourceMix::Both,     disable_session_cooc: true,  disable_anchor: true,  disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "+session-cooc",       source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: true,  disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "+anchor",             source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: false, disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "+freshness (full)",   source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: false, disable_freshness: false, rerank: RerankPolicy::None },
    Rung { name: "+oracle-rerank",      source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: false, disable_freshness: false, rerank: RerankPolicy::OracleByJudgment },
    Rung { name: "all-minus-cooc",      source: SourceMix::Both,     disable_session_cooc: true,  disable_anchor: false, disable_freshness: false, rerank: RerankPolicy::None },
    Rung { name: "all-minus-anchor",    source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: true,  disable_freshness: false, rerank: RerankPolicy::None },
    Rung { name: "all-minus-freshness", source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: false, disable_freshness: true,  rerank: RerankPolicy::None },
];

#[derive(Debug, Clone, Default)]
pub struct RungReport {
    pub name: String,
    pub ndcg_at_5: f64,
    pub ndcg_at_10: f64,
    pub ndcg_at_20: f64,
    pub mrr: f64,
    pub recall_at_10: f64,
    pub precision_at_10: f64,
    pub per_query: Vec<PerQueryMetrics>,
}

#[derive(Debug, Clone)]
pub struct PerQueryMetrics {
    pub query_id: String,
    pub ndcg_at_10: f64,
    pub mrr: f64,
}

#[derive(Debug, Clone, Default)]
pub struct BenchReport {
    pub fixture_kind: String,
    pub session_count: usize,
    pub query_count: usize,
    pub rungs: Vec<RungReport>,
}

impl BenchReport {
    pub fn rung_by_name(&self, name: &str) -> &RungReport {
        self.rungs
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("rung {name} not in report"))
    }
}

pub async fn run_bench(fixture: Fixture) -> BenchReport {
    let mut report = BenchReport {
        fixture_kind: format!("{:?}", fixture.kind),
        session_count: fixture.sessions.len(),
        query_count: fixture.queries.len(),
        rungs: Vec::with_capacity(RUNGS.len()),
    };

    for rung in RUNGS {
        let rung_report = run_rung(&fixture, rung).await;
        report.rungs.push(rung_report);
    }

    report
}

async fn run_rung(fixture: &Fixture, rung: &Rung) -> RungReport {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let cfg = Config::local_with_db_path(tmp.path().join("bench.duckdb"));
    let repo = DuckDbRepository::open(&cfg).await.expect("open repo");

    // Ingest fixture: messages + embeddings.
    let fake = Arc::new(FakeEmbeddingProvider::new("fake", 64));
    for session in &fixture.sessions {
        for block in &session.blocks {
            let msg = ConversationMessage {
                message_id: block.block_id.clone(),
                tenant: fixture.tenant.clone(),
                session_id: Some(session.session_id.clone()),
                role: block.role.clone(),
                block_type: block.block_type.clone(),
                content: block.content.clone(),
                embed_eligible: matches!(block.block_type.as_str(), "text" | "thinking"),
                created_at: block.created_at.clone(),
                ..Default::default()
            };
            repo.append_conversation_message(&msg)
                .await
                .expect("append message");
            if msg.embed_eligible {
                let v = fake.embed(&msg.content).await.expect("embed");
                repo.upsert_transcript_embedding(&fixture.tenant, &msg.message_id, &v)
                    .await
                    .expect("upsert embedding");
            }
        }
    }

    // Derive judgments once per rung.
    let now = "00000000020260503999";
    let judgments = derive_judgments(fixture, &repo, now).await;

    // For each query, retrieve + score + (optional) rerank + eval.
    let mut per_query: Vec<PerQueryMetrics> = Vec::with_capacity(fixture.queries.len());
    let mut sum = MetricSum::default();

    for query in &fixture.queries {
        let qrels = judgments
            .get(&query.query_id)
            .cloned()
            .unwrap_or_default();

        let run_session_ids = retrieve_and_rank(&repo, &fake, fixture, query, rung).await;

        let final_run = match rung.rerank {
            RerankPolicy::None => run_session_ids.clone(),
            RerankPolicy::OracleByJudgment => oracle_rerank(run_session_ids.clone(), &qrels),
        };

        let ndcg5 = ndcg_at_k(&final_run, &qrels, 5);
        let ndcg10 = ndcg_at_k(&final_run, &qrels, 10);
        let ndcg20 = ndcg_at_k(&final_run, &qrels, 20);
        let mrr_val = mrr(&final_run, &qrels);
        let r10 = recall_at_k(&final_run, &qrels, 10);
        let p10 = precision_at_k(&final_run, &qrels, 10);

        sum.add(ndcg5, ndcg10, ndcg20, mrr_val, r10, p10);
        per_query.push(PerQueryMetrics {
            query_id: query.query_id.clone(),
            ndcg_at_10: ndcg10,
            mrr: mrr_val,
        });
    }

    let n = fixture.queries.len() as f64;
    RungReport {
        name: rung.name.to_string(),
        ndcg_at_5: sum.ndcg5 / n,
        ndcg_at_10: sum.ndcg10 / n,
        ndcg_at_20: sum.ndcg20 / n,
        mrr: sum.mrr / n,
        recall_at_10: sum.recall10 / n,
        precision_at_10: sum.precision10 / n,
        per_query,
    }
}

async fn retrieve_and_rank(
    repo: &DuckDbRepository,
    fake: &Arc<FakeEmbeddingProvider>,
    fixture: &Fixture,
    query: &QueryFixture,
    rung: &Rung,
) -> Vec<SessionId> {
    // Step 1: Get candidates per source mix.
    let oversample = 50;
    let bm25 = match rung.source {
        SourceMix::Bm25Only | SourceMix::Both => repo
            .bm25_transcript_candidates(&fixture.tenant, &query.text, oversample)
            .await
            .unwrap_or_default(),
        SourceMix::HnswOnly => vec![],
    };
    let hnsw = match rung.source {
        SourceMix::HnswOnly | SourceMix::Both => {
            let qv = fake.embed(&query.text).await.expect("embed query");
            repo.search_transcript_embeddings(&fixture.tenant, &qv, oversample)
                .await
                .unwrap_or_default()
        }
        SourceMix::Bm25Only => vec![],
    };

    // Step 2: Build rank maps (rank starts at 1).
    let mut lex_ranks: HashMap<String, usize> = HashMap::new();
    for (i, m) in bm25.iter().enumerate() {
        lex_ranks.insert(m.message_id.clone(), i + 1);
    }
    let mut sem_ranks: HashMap<String, usize> = HashMap::new();
    for (i, m) in hnsw.iter().enumerate() {
        sem_ranks.insert(m.message_id.clone(), i + 1);
    }

    // Step 3: Union of candidates, deduplicated by message_id.
    let mut by_id: HashMap<String, ConversationMessage> = HashMap::new();
    for m in bm25.into_iter().chain(hnsw.into_iter()) {
        by_id.entry(m.message_id.clone()).or_insert(m);
    }
    let candidates: Vec<ConversationMessage> = by_id.into_values().collect();

    // Step 4: Score via production pipeline.
    let opts = ScoringOpts {
        anchor_session_id: query.anchor_session_id.as_deref(),
        disable_session_cooc: rung.disable_session_cooc,
        disable_anchor: rung.disable_anchor,
        disable_freshness: rung.disable_freshness,
    };
    let scored = score_candidates(candidates, &lex_ranks, &sem_ranks, opts);

    // Step 5: Project to session-level ranking. Take highest-scoring block per session,
    // dedup, take top-K sessions.
    let mut session_seen: HashSet<String> = HashSet::new();
    let mut run: Vec<SessionId> = Vec::with_capacity(TOP_K);
    for sb in scored {
        if let Some(sid) = sb.message.session_id.clone() {
            if session_seen.insert(sid.clone()) {
                run.push(sid);
                if run.len() >= TOP_K {
                    break;
                }
            }
        }
    }
    run
}

#[derive(Default)]
struct MetricSum {
    ndcg5: f64,
    ndcg10: f64,
    ndcg20: f64,
    mrr: f64,
    recall10: f64,
    precision10: f64,
}

impl MetricSum {
    fn add(&mut self, n5: f64, n10: f64, n20: f64, m: f64, r10: f64, p10: f64) {
        self.ndcg5 += n5;
        self.ndcg10 += n10;
        self.ndcg20 += n20;
        self.mrr += m;
        self.recall10 += r10;
        self.precision10 += p10;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::synthetic::{generate, SyntheticConfig};

    #[tokio::test]
    async fn run_bench_on_tiny_synthetic_returns_10_rungs() {
        let cfg = SyntheticConfig {
            session_count: 5,
            blocks_per_session: 4,
            query_count: 6,
            ..SyntheticConfig::default()
        };
        let fixture = generate(&cfg);
        let report = run_bench(fixture).await;
        assert_eq!(report.rungs.len(), 10);
        assert_eq!(report.rungs[0].name, "bm25-only");
        // Oracle must be ≥ full stack on every metric.
        let full = report.rung_by_name("+freshness (full)");
        let oracle = report.rung_by_name("+oracle-rerank");
        assert!(
            oracle.ndcg_at_10 >= full.ndcg_at_10,
            "oracle ({}) must ≥ full stack ({})",
            oracle.ndcg_at_10,
            full.ndcg_at_10
        );
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test --test recall_bench bench::runner -q -- --test-threads=1
```

(Single-threaded because each rung opens a TempDir; on contention this avoids "address in use" or temp-file thrash.)

Expected: 1 passed.

If it fails with API-name mismatches, this is where the implementer adapts to what `tests/transcript_recall.rs` actually uses — the names like `upsert_transcript_embedding`, `search_transcript_embeddings`, `append_conversation_message`, and `bm25_transcript_candidates` are best-guess shapes; the real ones may differ slightly. Read the existing test file and adjust.

- [ ] **Step 3: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 4: Commit**

```bash
git add tests/bench/runner.rs
git commit -m "$(cat <<'EOF'
feat(bench): ablation runner orchestrates 10 rungs through production pipeline

Each rung is a (SourceMix, disable flags, RerankPolicy) tuple feeding
into the same pipeline::transcript_recall::score_candidates that
production uses — bench numbers extrapolate directly.

Per-rung: fresh TempDir → DuckDb → ingest fixture → derive judgments →
retrieve via BM25/HNSW/both → score → optional oracle rerank →
session-level top-20 run → eval metrics → average across queries.
EOF
)"
```

---

## Task 10: Output formatting

**Files:**
- Modify: `tests/bench/runner.rs` (append `print_pretty_table` + `write_json` + tests)

Pretty stdout table for `--nocapture` review; JSON dump to `target/bench-out/recall-{kind}.json` for offline analysis.

- [ ] **Step 1: Append to `tests/bench/runner.rs`**

```rust
use std::fmt::Write as _;
use std::path::Path;

pub fn pretty_table(report: &BenchReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        &mut out,
        "=== Recall Bench ({}, {} sessions × {} queries) ===",
        report.fixture_kind, report.session_count, report.query_count
    );
    let _ = writeln!(
        &mut out,
        "{:<22}  NDCG@5  NDCG@10 NDCG@20  MRR    R@10   P@10",
        ""
    );
    let baseline = report
        .rungs
        .iter()
        .find(|r| r.name == "+freshness (full)")
        .map(|r| r.ndcg_at_10);
    for r in &report.rungs {
        let delta = match (r.name.starts_with("all-minus-"), baseline) {
            (true, Some(b)) => format!("  (Δ {:+.3})", r.ndcg_at_10 - b),
            _ => String::new(),
        };
        let _ = writeln!(
            &mut out,
            "{:<22}  {:.3}   {:.3}   {:.3}   {:.3}  {:.3}  {:.3}{}",
            r.name,
            r.ndcg_at_5,
            r.ndcg_at_10,
            r.ndcg_at_20,
            r.mrr,
            r.recall_at_10,
            r.precision_at_10,
            delta
        );
    }
    let _ = writeln!(&mut out);
    let _ = writeln!(
        &mut out,
        "⚠ Bias notice: judgments derived from co-mention + entity aliases."
    );
    let _ = writeln!(
        &mut out,
        "  HNSW absolute scores under-counted; relative deltas reliable."
    );
    let _ = writeln!(
        &mut out,
        "  See docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md §3."
    );
    out
}

pub fn write_json(report: &BenchReport, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::json!({
        "fixture_meta": {
            "kind": report.fixture_kind,
            "session_count": report.session_count,
            "query_count": report.query_count,
        },
        "rungs": report.rungs.iter().map(|r| {
            serde_json::json!({
                "name": r.name,
                "ndcg_at_5": r.ndcg_at_5,
                "ndcg_at_10": r.ndcg_at_10,
                "ndcg_at_20": r.ndcg_at_20,
                "mrr": r.mrr,
                "recall_at_10": r.recall_at_10,
                "precision_at_10": r.precision_at_10,
                "per_query": r.per_query.iter().map(|q| {
                    serde_json::json!({
                        "query_id": q.query_id,
                        "ndcg_at_10": q.ndcg_at_10,
                        "mrr": q.mrr,
                    })
                }).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    });
    std::fs::write(path, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

#[cfg(test)]
mod output_tests {
    use super::*;

    fn fixture_report() -> BenchReport {
        BenchReport {
            fixture_kind: "Synthetic { seed: 42 }".to_string(),
            session_count: 30,
            query_count: 24,
            rungs: vec![RungReport {
                name: "bm25-only".to_string(),
                ndcg_at_5: 0.612,
                ndcg_at_10: 0.658,
                ndcg_at_20: 0.701,
                mrr: 0.721,
                recall_at_10: 0.583,
                precision_at_10: 0.290,
                per_query: vec![],
            }, RungReport {
                name: "+freshness (full)".to_string(),
                ndcg_at_5: 0.741,
                ndcg_at_10: 0.782,
                ndcg_at_20: 0.815,
                mrr: 0.844,
                recall_at_10: 0.697,
                precision_at_10: 0.358,
                per_query: vec![],
            }, RungReport {
                name: "all-minus-cooc".to_string(),
                ndcg_at_5: 0.735,
                ndcg_at_10: 0.776,
                ndcg_at_20: 0.811,
                mrr: 0.838,
                recall_at_10: 0.692,
                precision_at_10: 0.355,
                per_query: vec![],
            }],
        }
    }

    #[test]
    fn pretty_table_contains_header_and_bias_notice() {
        let s = pretty_table(&fixture_report());
        assert!(s.contains("=== Recall Bench (Synthetic"));
        assert!(s.contains("bm25-only"));
        assert!(s.contains("Bias notice"));
        assert!(s.contains("co-mention"));
    }

    #[test]
    fn pretty_table_emits_delta_for_leave_one_out_rungs() {
        let s = pretty_table(&fixture_report());
        // all-minus-cooc Δ = 0.776 - 0.782 = -0.006
        assert!(s.contains("(Δ -0.006)"), "expected leave-one-out delta in output: {s}");
    }

    #[test]
    fn write_json_produces_well_formed_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("out").join("recall.json");
        write_json(&fixture_report(), &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["fixture_meta"]["session_count"], 30);
        assert_eq!(parsed["rungs"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["rungs"][0]["name"], "bm25-only");
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test --test recall_bench bench::runner::output_tests -q
```

Expected: 3 passed.

- [ ] **Step 3: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 4: Commit**

```bash
git add tests/bench/runner.rs
git commit -m "$(cat <<'EOF'
feat(bench): pretty stdout table + JSON dump for bench output

print_pretty_table renders a 6-metric × 10-rung table with leave-one-out
deltas and a bias notice footer. write_json emits target/bench-out/
JSON for offline post-processing.
EOF
)"
```

---

## Task 11: Test entries + regression assertions

**Files:**
- Modify: `tests/recall_bench.rs` (add the two real entries; remove the `#![allow(dead_code)]` from `tests/bench/mod.rs`)
- Modify: `tests/bench/mod.rs` (drop the dead_code allow now that everything is wired)

- [ ] **Step 1: Update `tests/bench/mod.rs`**

Remove `#![allow(dead_code)]` from `tests/bench/mod.rs`. The file now reads:

```rust
//! Test-only helpers for the recall ablation bench. See
//! `docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md`.
//!
//! This module is loaded via `mod bench;` from `tests/recall_bench.rs`.

pub mod fixture;
pub mod judgment;
pub mod oracle;
pub mod real;
pub mod runner;
pub mod synthetic;
```

If clippy now complains about specific unused items, add `#[allow(dead_code)]` only to those — don't blanket-allow.

- [ ] **Step 2: Append the test entries to `tests/recall_bench.rs`**

```rust
use bench::runner::{pretty_table, run_bench, write_json};
use bench::synthetic::{generate, SyntheticConfig};
use std::path::PathBuf;

#[tokio::test(flavor = "multi_thread")]
async fn synthetic_recall_bench() {
    let fixture = generate(&SyntheticConfig::default());
    let report = run_bench(fixture).await;

    println!("{}", pretty_table(&report));

    let out_path = PathBuf::from("target/bench-out/recall-synthetic.json");
    write_json(&report, &out_path).expect("write json");

    // CI regression assertions.
    let r = |n| report.rung_by_name(n);
    assert!(
        r("hybrid-rrf").ndcg_at_10 >= r("bm25-only").ndcg_at_10 - 0.01,
        "hybrid should not regress ≥0.01 vs BM25-only ({} vs {})",
        r("hybrid-rrf").ndcg_at_10,
        r("bm25-only").ndcg_at_10
    );
    assert!(
        r("hybrid-rrf").ndcg_at_10 >= r("hnsw-only").ndcg_at_10 - 0.01,
        "hybrid should not regress ≥0.01 vs HNSW-only"
    );
    assert!(
        r("+freshness (full)").ndcg_at_10 >= r("hybrid-rrf").ndcg_at_10 - 0.02,
        "full stack should not regress >0.02 vs hybrid-rrf"
    );
    assert!(
        r("+oracle-rerank").ndcg_at_10 >= r("+freshness (full)").ndcg_at_10,
        "oracle is an upper bound; must ≥ full stack"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real fixture; set MEM_BENCH_FIXTURE_PATH=…"]
async fn real_recall_bench() {
    let fixture = match bench::real::load_from_env().expect("load real fixture") {
        Some(f) => f,
        None => {
            eprintln!("MEM_BENCH_FIXTURE_PATH not set; skipping real bench");
            return;
        }
    };
    let report = run_bench(fixture).await;
    println!("{}", pretty_table(&report));
    let out_path = PathBuf::from("target/bench-out/recall-real.json");
    write_json(&report, &out_path).expect("write json");
    // No assertions — informational only.
}
```

- [ ] **Step 3: Run synthetic bench end-to-end**

```bash
cargo test --test recall_bench synthetic_recall_bench -- --nocapture
```

Expected: passes; stdout shows the 10-rung pretty table; `target/bench-out/recall-synthetic.json` exists.

If any of the regression assertions fail with the actual numbers (which depends on `FakeEmbeddingProvider` behavior on the synthetic fixture), tighten or relax them based on observed values — the goal is monotone improvement guards, not specific numerical bounds. Document any tweak in the commit message.

- [ ] **Step 4: Verify the real bench is properly skipped**

```bash
cargo test --test recall_bench real_recall_bench -- --nocapture
```

Expected: ignored. With env var set:

```bash
MEM_BENCH_FIXTURE_PATH=/nonexistent/path cargo test --test recall_bench real_recall_bench -- --ignored --nocapture
```

Expected: panics with "No such file or directory" (informational error, not a bug).

- [ ] **Step 5: Verify all CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -q
```

- [ ] **Step 6: Commit**

```bash
git add tests/recall_bench.rs tests/bench/mod.rs
git commit -m "$(cat <<'EOF'
feat(bench): synthetic + real bench entries with CI regression guards

synthetic_recall_bench runs the full 10-rung ablation on the in-tree
fixture and asserts monotone-improvement invariants. real_recall_bench
is #[ignore]'d and reads MEM_BENCH_FIXTURE_PATH. Both write JSON to
target/bench-out/.
EOF
)"
```

---

## Task 12: Documentation + smoke

**Files:**
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `docs/ROADMAP.MD`

- [ ] **Step 1: Append README section**

Add a new section to `README.md` (after the "Entity Registry" section that landed in the prior wave, or after "Transcript Archive" if Entity Registry isn't a section there yet):

```markdown
## Recall Quality Bench (transcripts)

A 10-rung ablation harness for the transcript recall pipeline. Quantifies
each ranking signal's marginal NDCG@k contribution and gives an oracle
upper bound for binary cross-encoder rerankers.

### Synthetic (CI / regression smoke)

Runs on a deterministic in-tree fixture (`SyntheticConfig::default()`,
seed=42, 30 sessions × 8 blocks × 24 queries):

```bash
cargo test --test recall_bench synthetic_recall_bench -- --nocapture
```

Prints the 10-rung table to stdout; writes `target/bench-out/recall-synthetic.json`.

### Real (local decision pull)

Set `MEM_BENCH_FIXTURE_PATH` to a JSON dump of your own transcripts
(see `docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md` §Real Fixture for schema):

```bash
MEM_BENCH_FIXTURE_PATH=/path/to/recall-real.json \
  cargo test --test recall_bench real_recall_bench -- --ignored --nocapture
```

### Notes

- Judgments are derived automatically (co-mention + entity-alias). Absolute
  NDCG values under-count HNSW (synonym hits hidden by the heuristic);
  relative deltas across rungs are reliable.
- The bench shares `pipeline::transcript_recall::score_candidates` with
  production — rung differences are config tuples, not parallel rankers.
- Output JSON shape: see `tests/bench/runner.rs::write_json`.
```

- [ ] **Step 2: Append CHANGELOG entry**

Add to `CHANGELOG.md` under a new section dated 2026-05-03 (or 2026-05-04 if implementation lands a day later):

```markdown
## 2026-05-03 — Transcript Recall Quality Bench

### Added

- 10-rung ablation harness for the transcript recall pipeline (`tests/recall_bench.rs`)
- `src/pipeline/eval_metrics.rs` — pure NDCG@k / MRR / Recall@k / Precision@k
- `tests/bench/` — fixture / synthetic generator / real loader / judgment / oracle / runner modules
- Synthetic CI guard: monotone-improvement assertions across rungs
- Real fixture loader: `MEM_BENCH_FIXTURE_PATH` env-var, JSON schema v1, `#[ignore]`'d

### Changed

- `pipeline::transcript_recall::ScoringOpts` extended with `disable_session_cooc /
  disable_anchor / disable_freshness` bools (default false → zero behavior change)

### Notes

- Bench answers two questions: (1) does each existing signal carry weight?
  (2) is a real cross-encoder worth pursuing? Oracle rerank rung gives the
  binary-reranker upper bound.
- Co-mention + entity-alias auto-judgment biases toward lexical hits;
  absolute scores not directly comparable across BM25/HNSW, but Δ across rungs
  is reliable.
```

- [ ] **Step 3: Update ROADMAP**

Add a row (or update an existing "quality baseline" row) in `docs/ROADMAP.MD`:

```markdown
| 14 | 🔍 | ✅ **Recall quality bench (transcripts)**（10-rung ablation, synthetic+real fixtures, oracle-rerank upper bound; closes spec `2026-05-03-transcript-recall-bench-design`） | 🟡 决策基础设施 | M（半天） | 低 | `src/pipeline/eval_metrics.rs`, `tests/bench/`, `tests/recall_bench.rs` |
```

If the ROADMAP already has a "评估" / "quality baseline" row, update it ✅ instead.

- [ ] **Step 4: Smoke run**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -q
cargo build --release  # final smoke gate; release build catches optimization-only issues
```

All clean.

- [ ] **Step 5: Commit**

```bash
git add README.md CHANGELOG.md docs/ROADMAP.MD
git commit -m "$(cat <<'EOF'
docs(bench): document the recall ablation bench surface

README: how to run synthetic vs real, expected output paths, and
the bias-notice warning.
CHANGELOG: 2026-05-03 wave entry.
ROADMAP: quality bench item ✅.
EOF
)"
```

---

## Self-Review

**1. Spec coverage**

| Spec section | Plan task |
|---|---|
| §Architecture | Task 4 (fixture types) + Task 9 (runner) |
| §Synthetic Fixture | Task 5 |
| §Real Fixture Loader | Task 6 |
| §Judgment Derivation | Task 7 |
| §Eval Metrics | Task 2 |
| §Ablation Runner (Rung config) | Task 9 |
| §Oracle Rerank | Task 8 |
| §Test Harness | Task 11 |
| §File Layout | All tasks (file paths exact) |
| §Risks (ScoringOpts non-breaking change) | Task 3 |
| §Bias Notice | Task 10 (pretty_table) |
| §Documentation | Task 12 |

All sections covered. The probe (Task 1) is plan-added scaffolding for the spec's "Concerns to Confirm #2".

**2. Placeholder scan**

No "TBD" / "TODO" / "implement later" in step bodies. Where API names are guesses (Task 1, Task 9 step 2 for `upsert_transcript_embedding`/`search_transcript_embeddings`), an explicit "implementer note" tells the implementer to verify against `tests/transcript_recall.rs` and adapt. This is honest scaffolding, not a placeholder — the plan can't pre-validate these names without reading the file.

**3. Type consistency**

- `ScoringOpts<'a>` extension (Task 3) defines fields used in Task 9 runner — names match: `disable_session_cooc`, `disable_anchor`, `disable_freshness`.
- `Fixture` struct (Task 4) is consumed unchanged by Tasks 5, 6, 7, 9.
- `Rung` (Task 9) is referenced in `RUNGS` const and used by `run_rung`. All field names consistent.
- `BenchReport` / `RungReport` shape used consistently in Tasks 9, 10, 11.
- `EntityKind::Topic` in Task 7 — verified against the entity-registry plan; this variant ships there.
- `pipeline::eval_metrics::*` re-exported correctly in Task 9 (`use mem::pipeline::eval_metrics::*;`).

**4. One known unknown**

`bm25_transcript_candidates` returns `Vec<ConversationMessage>` per the existing `service::transcript_service` call site grep result — Task 9 step 2 assumes that. If the actual return is a different type (e.g., `Vec<(ConversationMessage, ScoreInfo)>`), the runner's rank-extraction loop needs adapting. Implementer should `cargo check` the runner sketch first and adjust before running tests.
