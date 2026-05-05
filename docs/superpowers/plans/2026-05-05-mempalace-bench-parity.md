# MemPalace LongMemEval Parity Bench Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apple-to-apple LongMemEval benchmark for mem vs mempalace's published baselines (Recall@5 = 0.966 raw / 0.894 rooms / full = varies).

**Architecture:** Rust port of mempalace's `longmemeval_bench.py`. In-process per-Q runner with fresh `DuckDbRepository` + `VectorIndex` + production EmbedAnything embedder. Three rungs map to mempalace `raw / rooms / full`. Output JSON mirrors mempalace's `results_*.json` shape, prefixed `results_mem_*` to disambiguate. Manual decision tool, env-var dataset path, not in CI.

**Tech Stack:** Rust 2021, tokio, DuckDB (bundled), tempfile, serde / serde_json, EmbedAnythingEmbeddingProvider (Qwen3 0.6B 1024-dim), `pipeline::transcript_recall::score_candidates` (Wave 5 production stack).

**Spec:** `docs/superpowers/specs/2026-05-05-mempalace-bench-parity-design.md` (commit `2720aca`).

---

## Conventions referenced throughout

- **In-process pattern** mirrors `tests/bench/runner.rs` (Wave 5 ablation runner): per-rung `TempDir + DuckDbRepository::open + VectorIndex::new_in_memory`, but here we ingest **once per question** + re-rank under 3 rungs.
- **Real embedder, not Fake.** Wave 5 used `FakeEmbeddingProvider` because it didn't care about absolute scores. This bench uses `EmbedAnythingEmbeddingProvider::from_settings(&EmbeddingSettings)` with whatever the user has configured (production = Qwen3 0.6B / 1024-dim per `.env`). If user has `EMBEDDING_PROVIDER=fake` the bench numbers are meaningless — document this loud and clear.
- **Reuse Wave 5 types where shared.** `SourceMix` is `pub` in `tests/bench/runner.rs:19` — reuse via `use super::runner::SourceMix`. `ScoringOpts` is in `pipeline::transcript_recall` and already has `disable_*` fields from Wave 5 Task 3.
- **Per-Q DB churn is acceptable.** 500 × `TempDir::new` + DuckDb open + VectorIndex init = ~25-100s overhead, swamped by embedding wall-clock.
- **No regression assertions.** This is informational. Test entry only writes JSON + prints comparison table.
- **Commit scope tags:** `feat(bench)`, `test(bench)`, `docs(bench)`, `refactor(metrics)`.
- **CI gates:** `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean. Full `cargo test -q` passes (without the `#[ignore]`'d bench).

---

## File Structure (locked decisions)

**Created:**
- `tests/bench/longmemeval_dataset.rs` — JSON deserialize structs + `load_from_env_or_skip()`
- `tests/bench/longmemeval.rs` — runner: `RUNGS` const, `LongMemEvalRung`, `BenchReport`, `run_longmemeval_bench`, `ingest_corpus`, `retrieve_for_rung`, `print_comparison_table`, `write_per_rung_json`
- `tests/mempalace_bench.rs` — single `#[tokio::test]` entry, `#[ignore]`'d

**Modified:**
- `src/pipeline/eval_metrics.rs` — add `recall_any_at_k`, `recall_all_at_k` + 8 reference unit tests
- `tests/bench/mod.rs` — register 2 new modules (longmemeval_dataset, longmemeval)
- `README.md` — bench section
- `CHANGELOG.md` — 2026-05-05 entry
- `docs/ROADMAP.MD` — add row #15

**Untouched:**
- `src/pipeline/transcript_recall.rs` (Wave 5 stable)
- `src/storage/transcript_repo.rs`, `src/service/transcript_service.rs`, `src/http/transcripts.rs` (in-process bench)
- `tests/bench/runner.rs` (Wave 5 ablation runner — independent role from LongMemEval runner)

---

## Task 1: Probe — sample mempalace results schema + LongMemEval format inspection

**Files:**
- Create: `tests/mempalace_bench.rs` (probe-only at this point)

The spec's "Plan first step" calls out: fetch a sample mempalace `results_*.jsonl` to confirm the JSON output shape we'll be mirroring, and inspect a real LongMemEval question entry to validate our deserialize types. This Task is the spike.

This task is **knowledge-grab + setup**, not TDD. The probe is a single `#[ignore]`'d test that loads a fixture from env-var (or local file), prints structural info, and documents observations at the top of the file.

- [ ] **Step 1: Inspect mempalace results sample**

If you have local network access, `curl -s` (or `WebFetch`) one of the published mempalace results files. The known files (per Wave 4 recon) are at:
- `https://raw.githubusercontent.com/MemPalace/mempalace/develop/benchmarks/results_mempal_raw_session_20260414_1629.jsonl`
- `https://raw.githubusercontent.com/MemPalace/mempalace/develop/benchmarks/results_mempal_hybrid_v4_held_out_session_20260414_1634.jsonl`

Read the first 2-3 JSONL records (they're newline-delimited JSON). Document the EXACT key names you observe in the file's top docstring. Pay attention to:
- Top-level: `{benchmark, mode, system, ...}` vs nested under `meta:`
- Per-question: `recall_any@5` (string with `@`) vs `recall_any_at_5` (snake_case with `at`)
- Aggregate position: top-level vs nested under `aggregate:` vs computed externally
- Field ordering (cosmetic but worth matching for diff-friendliness)

If you cannot reach the network, fall back to documenting what the spec says (mempalace `results_*.jsonl` per-Q has `question_id / recall_any@5 / ndcg@5 / ranked_docs` etc.) and note that schema confirmation will happen the first time the bench runs against real data. Don't block; choose a reasonable shape (snake_case, nested aggregate per spec) and commit.

- [ ] **Step 2: Create `tests/mempalace_bench.rs` with the probe**

```rust
//! MemPalace LongMemEval parity bench (closes ROADMAP #15).
//! See docs/superpowers/specs/2026-05-05-mempalace-bench-parity-design.md.
//!
//! ### Schema reverse-engineering notes (Task 1, 2026-05-05)
//!
//! Inspected mempalace `results_mempal_*.jsonl` sample to confirm output
//! shape. Findings:
//! - Per-line JSON record (jsonl format, not pretty-printed array)
//! - Per-question keys observed: <fill in after Step 1 inspection>
//! - Aggregate keys observed: <fill in>
//! - Field naming convention: <snake_case_at | with_at_sign | other>
//!
//! For our `results_mem_longmemeval_*.json` we use **snake_case_at** keys
//! (e.g. `recall_any_at_5`) since they are valid Rust identifiers and
//! play nice with `serde_json::json!` macro. If a user wants to compare
//! with mempalace `recall_any@5` keys, a `jq` rename suffices:
//!   jq 'with_entries(.key |= sub("_at_"; "@"))'
//!
//! ### LongMemEval question schema (Task 1)
//!
//! When the dataset path is available, the probe below loads the first
//! 5 questions and prints the structural shape so a future maintainer
//! can compare against `LongMemEvalQuestion` in
//! tests/bench/longmemeval_dataset.rs.

#[tokio::test(flavor = "multi_thread")]
#[ignore = "probe; set MEM_LONGMEMEVAL_PATH=... and run with --ignored"]
async fn longmemeval_format_probe() {
    let path = match std::env::var("MEM_LONGMEMEVAL_PATH") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("MEM_LONGMEMEVAL_PATH not set; skipping probe");
            return;
        }
    };
    let bytes = std::fs::read(&path).expect("read fixture");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");

    // Inspect top-level: is it an array of questions, or {questions: [...]}, or other?
    match &json {
        serde_json::Value::Array(arr) => {
            println!("LongMemEval is a top-level array of {} entries", arr.len());
            for (i, entry) in arr.iter().take(5).enumerate() {
                let keys: Vec<_> = entry.as_object()
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                println!("  Q{}: keys = {:?}", i, keys);
                if let Some(haystack) = entry.get("haystack_sessions") {
                    if let Some(arr) = haystack.as_array() {
                        println!("    haystack_sessions count = {}", arr.len());
                        if let Some(first_session) = arr.first() {
                            let sess_keys: Vec<_> = first_session.as_object()
                                .map(|m| m.keys().cloned().collect())
                                .unwrap_or_default();
                            println!("    first session keys = {:?}", sess_keys);
                            if let Some(turns) = first_session.get("turns") {
                                if let Some(t_arr) = turns.as_array() {
                                    println!("    first session has {} turns", t_arr.len());
                                    if let Some(t0) = t_arr.first() {
                                        let turn_keys: Vec<_> = t0.as_object()
                                            .map(|m| m.keys().cloned().collect())
                                            .unwrap_or_default();
                                        println!("      turn[0] keys = {:?}", turn_keys);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        serde_json::Value::Object(obj) => {
            println!("LongMemEval is an object with top-level keys: {:?}",
                     obj.keys().collect::<Vec<_>>());
        }
        other => panic!("unexpected top-level shape: {other:?}"),
    }
    println!("PROBE COMPLETE — copy the field names above into LongMemEvalQuestion struct in Task 3");
}
```

- [ ] **Step 3: Run the probe**

```bash
cargo test --test mempalace_bench longmemeval_format_probe -- --ignored --nocapture
```

If `MEM_LONGMEMEVAL_PATH` is not set, you'll see the skip message — that's OK; document the assumption in the docstring and proceed (Task 3 will resolve real schema on first dataset access).

If env is set, you'll see the structural printout. Update the file's top docstring with what you observe — concrete key names, types, nesting.

- [ ] **Step 4: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --test mempalace_bench -q
```

Expected: 0 active tests run (probe is `#[ignore]`'d), build clean.

- [ ] **Step 5: Commit**

```bash
git add tests/mempalace_bench.rs
git commit -m "$(cat <<'EOF'
test(bench): probe LongMemEval dataset + mempalace results schema

Documents the JSON shapes we'll be working with in the rest of this
plan. Probe is #[ignore]'d; outcome documented at the top of
tests/mempalace_bench.rs.
EOF
)"
```

---

## Task 2: Eval metrics — `recall_any_at_k` + `recall_all_at_k`

**Files:**
- Modify: `src/pipeline/eval_metrics.rs`

Per spec section "Eval Metrics Extension": mempalace's scoreboard reports `recall_any@k` (1.0 if at least one relevant id in top-K, else 0.0) and `recall_all@k` (1.0 if all relevant ids in top-K, else 0.0). These are different from our `recall_at_k` (fraction-based). Add both, with hand-computed reference tests.

- [ ] **Step 1: Write the failing tests**

Append to `src/pipeline/eval_metrics.rs`'s `#[cfg(test)] mod tests` block:

```rust
#[test]
fn recall_any_at_k_returns_one_when_any_relevant_in_top_k() {
    let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    approx(recall_any_at_k(&run, &qrels(&["b"]), 3), 1.0);
}

#[test]
fn recall_any_at_k_returns_zero_when_none_in_top_k() {
    let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    approx(recall_any_at_k(&run, &qrels(&["x"]), 3), 0.0);
}

#[test]
fn recall_any_at_k_caps_at_k() {
    // Relevant exists at position 4, k=3 -> none in top-3 -> 0.0
    let run = ["a", "b", "c", "d", "x"]
        .iter().map(|s| s.to_string()).collect::<Vec<_>>();
    approx(recall_any_at_k(&run, &qrels(&["x"]), 3), 0.0);
}

#[test]
fn recall_any_at_k_returns_zero_when_qrels_empty() {
    let run = vec!["a".to_string()];
    approx(recall_any_at_k(&run, &qrels(&[]), 5), 0.0);
}

#[test]
fn recall_all_at_k_returns_one_when_all_in_top_k() {
    let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    approx(recall_all_at_k(&run, &qrels(&["a", "c"]), 3), 1.0);
}

#[test]
fn recall_all_at_k_returns_zero_when_partial() {
    // qrels = {a, x}, top-3 = {a, b, c}, x missing -> 0.0 (not 0.5)
    let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    approx(recall_all_at_k(&run, &qrels(&["a", "x"]), 3), 0.0);
}

#[test]
fn recall_all_at_k_returns_zero_when_none_in_top_k() {
    let run = vec!["a".to_string(), "b".to_string()];
    approx(recall_all_at_k(&run, &qrels(&["x", "y"]), 2), 0.0);
}

#[test]
fn recall_all_at_k_returns_zero_when_qrels_empty() {
    // Empty qrels: vacuous "all" is 0 by our convention (avoids
    // divide-by-zero corner; mempalace also returns 0 here).
    let run = vec!["a".to_string()];
    approx(recall_all_at_k(&run, &qrels(&[]), 5), 0.0);
}
```

- [ ] **Step 2: Run the new tests to verify they fail**

```bash
cargo test --lib pipeline::eval_metrics::tests::recall_any -q
cargo test --lib pipeline::eval_metrics::tests::recall_all -q
```

Expected: 8 errors with "cannot find function `recall_any_at_k`" and similar.

- [ ] **Step 3: Implement the two functions**

Add to `src/pipeline/eval_metrics.rs` (after `recall_at_k`, before `precision_at_k`):

```rust
/// Mempalace-style binary recall: 1.0 if top-K contains >=1 relevant id, else 0.0.
///
/// This is a binary indicator — different from [`recall_at_k`] which returns the
/// *fraction* of relevant ids found. Use this for "did we find at least one of
/// the answer sessions" tasks (LongMemEval, ConvoMem). Returns 0.0 when qrels is
/// empty (avoids degenerate "vacuously true" 1.0).
pub fn recall_any_at_k<I: Eq + Hash>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    if qrels.is_empty() {
        return 0.0;
    }
    if run.iter().take(k).any(|id| qrels.contains(id)) {
        1.0
    } else {
        0.0
    }
}

/// Mempalace-style binary recall: 1.0 if top-K contains ALL relevant ids, else 0.0.
///
/// Stricter than [`recall_any_at_k`]; useful for multi-hop tasks where partial
/// recall is insufficient. Returns 0.0 when qrels is empty.
pub fn recall_all_at_k<I: Eq + Hash>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    if qrels.is_empty() {
        return 0.0;
    }
    let top_k: HashSet<&I> = run.iter().take(k).collect();
    if qrels.iter().all(|id| top_k.contains(id)) {
        1.0
    } else {
        0.0
    }
}
```

Update the file's module-level docstring to mention the two new functions.

- [ ] **Step 4: Run all tests**

```bash
cargo test --lib pipeline::eval_metrics -q
```

Expected: 25 passed (17 existing + 8 new).

- [ ] **Step 5: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/pipeline/eval_metrics.rs
git commit -m "$(cat <<'EOF'
refactor(metrics): add recall_any_at_k + recall_all_at_k binary indicators

Mempalace's scoreboard reports binary indicator metrics
(recall_any@k = 1.0 if at least one relevant id in top-K, recall_all@k
= 1.0 if all relevant ids in top-K) different from our standard
fraction-based recall_at_k. Adding both for the LongMemEval parity
bench, with 8 hand-computed reference tests covering hit / miss /
partial / empty-qrels paths.
EOF
)"
```

---

## Task 3: Dataset types + loader

**Files:**
- Create: `tests/bench/longmemeval_dataset.rs`
- Modify: `tests/bench/mod.rs` (add `pub mod longmemeval_dataset;`)

Per spec section "Dataset Loader": types that mirror LongMemEval's JSON, plus an env-var driven loader that returns `Some(Vec<LongMemEvalQuestion>)` or `None` (for skip).

- [ ] **Step 1: Write the loader + tests in `tests/bench/longmemeval_dataset.rs`**

```rust
//! LongMemEval dataset deserialize + env-var driven loader.
//!
//! Schema notes (Task 1 probe — update if probe revealed differences):
//! - Top-level: array of question entries
//! - Per-question keys: question_id, question, haystack_sessions,
//!   answer_session_ids, question_date
//! - Per-session keys: session_id, started_at, turns
//! - Per-turn keys: role, content
//!
//! If the on-disk schema differs (e.g., snake_case -> camelCase, or
//! `id` instead of `question_id`), update the deserialize types and
//! the schema notes in this docstring atomically.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct LongMemEvalQuestion {
    /// LongMemEval doesn't always include question_id in the JSON;
    /// loader fills with `format!("lme_q_{:04}", index)` if absent.
    #[serde(default)]
    pub question_id: String,
    pub question: String,
    pub haystack_sessions: Vec<LongMemEvalSession>,
    pub answer_session_ids: Vec<String>,
    #[serde(default)]
    pub question_date: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LongMemEvalSession {
    pub session_id: String,
    #[serde(default)]
    pub started_at: Option<String>,
    pub turns: Vec<LongMemEvalTurn>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LongMemEvalTurn {
    pub role: String, // "user" | "assistant" | possibly "system"
    pub content: String,
}

/// Load LongMemEval questions if `MEM_LONGMEMEVAL_PATH` is set.
/// Returns `None` when env var is unset (callers skip silently).
/// Panics with a clear message if the file is missing or invalid.
pub fn load_from_env_or_skip() -> Option<Vec<LongMemEvalQuestion>> {
    let path = std::env::var("MEM_LONGMEMEVAL_PATH").ok()?;
    Some(load_from_path(Path::new(&path)).expect("load LongMemEval"))
}

pub fn load_from_path(path: &Path) -> Result<Vec<LongMemEvalQuestion>, std::io::Error> {
    let bytes = std::fs::read(path)?;
    let mut questions: Vec<LongMemEvalQuestion> =
        serde_json::from_slice(&bytes).expect("invalid LongMemEval JSON");
    // Backfill question_id where absent.
    for (i, q) in questions.iter_mut().enumerate() {
        if q.question_id.is_empty() {
            q.question_id = format!("lme_q_{:04}", i);
        }
    }
    Ok(questions)
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
    fn load_from_env_or_skip_returns_none_when_unset() {
        let original = std::env::var("MEM_LONGMEMEVAL_PATH").ok();
        std::env::remove_var("MEM_LONGMEMEVAL_PATH");
        let res = load_from_env_or_skip();
        assert!(res.is_none());
        if let Some(v) = original {
            std::env::set_var("MEM_LONGMEMEVAL_PATH", v);
        }
    }

    #[test]
    fn load_from_path_parses_minimal_valid_file() {
        let json = r#"[
            {
                "question_id": "lme_q_0001",
                "question": "favourite hike?",
                "haystack_sessions": [
                    {
                        "session_id": "sess_1",
                        "started_at": "2024-03-15T00:00:00",
                        "turns": [
                            {"role": "user", "content": "I love Yosemite trails"},
                            {"role": "assistant", "content": "great!"}
                        ]
                    }
                ],
                "answer_session_ids": ["sess_1"]
            }
        ]"#;
        let f = write_fixture(json);
        let qs = load_from_path(f.path()).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question_id, "lme_q_0001");
        assert_eq!(qs[0].haystack_sessions.len(), 1);
        assert_eq!(qs[0].haystack_sessions[0].turns.len(), 2);
        assert_eq!(qs[0].answer_session_ids, vec!["sess_1"]);
    }

    #[test]
    fn missing_question_id_gets_fallback() {
        let json = r#"[
            {
                "question": "q without id",
                "haystack_sessions": [],
                "answer_session_ids": []
            },
            {
                "question": "another q without id",
                "haystack_sessions": [],
                "answer_session_ids": []
            }
        ]"#;
        let f = write_fixture(json);
        let qs = load_from_path(f.path()).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].question_id, "lme_q_0000");
        assert_eq!(qs[1].question_id, "lme_q_0001");
    }

    #[test]
    fn missing_started_at_is_none() {
        let json = r#"[
            {
                "question_id": "q1",
                "question": "q",
                "haystack_sessions": [
                    {"session_id": "s1", "turns": [{"role": "user", "content": "hi"}]}
                ],
                "answer_session_ids": []
            }
        ]"#;
        let f = write_fixture(json);
        let qs = load_from_path(f.path()).unwrap();
        assert!(qs[0].haystack_sessions[0].started_at.is_none());
    }
}
```

- [ ] **Step 2: Register module + wire `mod bench;` into the test entry**

In `tests/bench/mod.rs`, add (alphabetical):

```rust
pub mod longmemeval_dataset;
```

In `tests/mempalace_bench.rs`, add at the top (above the probe):

```rust
mod bench;
```

- [ ] **Step 3: Run tests**

```bash
cargo test --test mempalace_bench bench::longmemeval_dataset -q
```

Expected: 4 passed.

- [ ] **Step 4: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add tests/bench/longmemeval_dataset.rs tests/bench/mod.rs tests/mempalace_bench.rs
git commit -m "$(cat <<'EOF'
feat(bench): LongMemEval dataset types + env-var loader

LongMemEvalQuestion / Session / Turn deserialize structs match the
schema observed in the Task 1 probe (or are defensive against
unobserved variants via #[serde(default)]). load_from_env_or_skip()
returns Ok(None) when MEM_LONGMEMEVAL_PATH is unset; load_from_path()
panics loudly on schema mismatch. Question IDs are backfilled when
absent so per-Q reporting always has a stable handle.
EOF
)"
```

---

## Task 4: Ingest helper — `ingest_corpus`

**Files:**
- Create: `tests/bench/longmemeval.rs` (this task creates the file with one helper; later tasks append)
- Modify: `tests/bench/mod.rs` (add `pub mod longmemeval;`)

Per spec section "Ingest Mapping": one function takes a `LongMemEvalQuestion`, a freshly-opened `DuckDbRepository`, a `VectorIndex`, and an embedding provider. It iterates haystack sessions x turns, creates `ConversationMessage` rows, generates embeddings, upserts into VectorIndex.

- [ ] **Step 1: Write the failing test + implementation in one shot**

Create `tests/bench/longmemeval.rs`:

```rust
//! LongMemEval bench runner. Per-question ingest + 3-rung re-rank.
//! See docs/superpowers/specs/2026-05-05-mempalace-bench-parity-design.md.

use super::longmemeval_dataset::*;
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::embedding::{EmbeddingProvider, FakeEmbeddingProvider};
use mem::storage::{DuckDbRepository, VectorIndex};
use std::sync::Arc;

const TENANT: &str = "bench";

/// Ingest one LongMemEval question's haystack into the given repo + index.
/// Each (session, turn) pair becomes one ConversationMessage; each
/// embed-eligible message gets embedded + upserted into the VectorIndex.
pub async fn ingest_corpus(
    repo: &DuckDbRepository,
    index: &VectorIndex,
    embedder: &Arc<dyn EmbeddingProvider>,
    question: &LongMemEvalQuestion,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut count = 0usize;
    for session in &question.haystack_sessions {
        let session_started_ms = parse_started_at_to_ms(session.started_at.as_deref())
            .unwrap_or_else(|| stable_session_seed_ms(&session.session_id));
        for (turn_idx, turn) in session.turns.iter().enumerate() {
            let block_id = format!(
                "{}_{}_{}",
                question.question_id, session.session_id, turn_idx
            );
            let role = parse_role(&turn.role);
            let content = turn.content.clone();
            let created_ms = session_started_ms + (turn_idx as u64) * 60_000;
            let msg = ConversationMessage {
                message_block_id: block_id.clone(),
                tenant: TENANT.to_string(),
                session_id: Some(session.session_id.clone()),
                role,
                block_type: BlockType::Text,
                content: content.clone(),
                embed_eligible: true,
                created_at: format!("{:020}", created_ms),
                // Bench defaults for fields not used by ranking:
                caller_agent: "bench".to_string(),
                transcript_path: format!("/tmp/lme/{}.jsonl", question.question_id),
                message_uuid: block_id.clone(),
                tool_use_id: None,
                tool_name: None,
                tool_result_status: None,
                line_number: 0,
                block_index: turn_idx as u32,
            };
            repo.create_conversation_message(&msg).await?;
            let v = embedder.embed_text(&content).await?;
            index.upsert(&block_id, &v)?;
            count += 1;
        }
    }
    Ok(count)
}

fn parse_role(s: &str) -> MessageRole {
    MessageRole::from_db_str(s).unwrap_or(MessageRole::User)
}

/// Parse an ISO-8601 timestamp like "2024-03-15T00:00:00" into ms since epoch.
/// Returns None on parse failure; caller falls back to a stable seed.
fn parse_started_at_to_ms(s: Option<&str>) -> Option<u64> {
    let s = s?;
    // Best-effort: extract YYYY-MM-DD prefix and convert to ms.
    let date = s.get(..10)?;
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let d: u64 = parts[2].parse().ok()?;
    // Naive: treat year/month/day as a unique offset (not a real epoch).
    Some(((y - 1970) * 365 + m * 30 + d) * 86_400_000)
}

/// Stable per-session millisecond seed (used when started_at is missing).
/// Hash the session_id to a u64 in a deterministic way.
fn stable_session_seed_ms(session_id: &str) -> u64 {
    let mut h: u64 = 14_695_981_039_346_656_037; // FNV-1a basis
    for b in session_id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1_099_511_628_211);
    }
    1_700_000_000_000 + (h % (90 * 86_400_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    pub(super) fn make_question(qid: &str, sessions: Vec<(&str, Vec<(&str, &str)>)>) -> LongMemEvalQuestion {
        LongMemEvalQuestion {
            question_id: qid.to_string(),
            question: format!("question for {}", qid),
            haystack_sessions: sessions
                .into_iter()
                .map(|(sid, turns)| LongMemEvalSession {
                    session_id: sid.to_string(),
                    started_at: Some("2024-03-15T00:00:00".to_string()),
                    turns: turns
                        .into_iter()
                        .map(|(role, content)| LongMemEvalTurn {
                            role: role.to_string(),
                            content: content.to_string(),
                        })
                        .collect(),
                })
                .collect(),
            answer_session_ids: vec![],
            question_date: None,
        }
    }

    #[tokio::test]
    async fn ingest_corpus_creates_message_per_turn_and_embedding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(&tmp.path().join("ingest.duckdb"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake");
        let index = VectorIndex::new_in_memory(64, "fake", "fake", 16).unwrap();
        let embedder: Arc<dyn EmbeddingProvider> =
            Arc::new(FakeEmbeddingProvider::new("fake", 64));

        let q = make_question(
            "lme_q_0001",
            vec![
                ("sess_1", vec![("user", "hello world"), ("assistant", "hi")]),
                ("sess_2", vec![("user", "tokio runtime"), ("assistant", "yes")]),
            ],
        );
        let count = ingest_corpus(&repo, &index, &embedder, &q).await.unwrap();
        assert_eq!(count, 4, "4 turns ingested");

        // Sanity check: HNSW finds something for a known query.
        let qv = embedder.embed_text("tokio").await.unwrap();
        let hits = index.search(&qv, 4).unwrap();
        assert!(!hits.is_empty(), "HNSW should return ingested blocks");
        let ids: HashSet<String> = hits.into_iter().map(|(id, _)| id).collect();
        assert!(ids.iter().all(|id| id.starts_with("lme_q_0001_")));
    }

    #[tokio::test]
    async fn ingest_corpus_handles_missing_started_at() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(&tmp.path().join("ingest.duckdb"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake");
        let index = VectorIndex::new_in_memory(64, "fake", "fake", 8).unwrap();
        let embedder: Arc<dyn EmbeddingProvider> =
            Arc::new(FakeEmbeddingProvider::new("fake", 64));

        // Session with started_at = None
        let q = LongMemEvalQuestion {
            question_id: "q1".to_string(),
            question: "q".to_string(),
            haystack_sessions: vec![LongMemEvalSession {
                session_id: "s1".to_string(),
                started_at: None,
                turns: vec![LongMemEvalTurn {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
            }],
            answer_session_ids: vec![],
            question_date: None,
        };
        let count = ingest_corpus(&repo, &index, &embedder, &q).await.unwrap();
        assert_eq!(count, 1);
    }
}
```

**Note**: this file uses `FakeEmbeddingProvider` in its UNIT TESTS only — for testing the ingest helper in isolation. The actual bench (Task 8) will use `EmbedAnythingEmbeddingProvider` because numbers vs mempalace require a real model. The unit tests here aren't measuring quality, just structural correctness.

- [ ] **Step 2: Register module**

In `tests/bench/mod.rs` add:

```rust
pub mod longmemeval;
```

- [ ] **Step 3: Run tests**

```bash
cargo test --test mempalace_bench bench::longmemeval::tests -q
```

Expected: 2 passed.

If you see "no method named `set_transcript_job_provider`" or "no method named `create_conversation_message`" — the API names from Wave 5 Task 1 probe should match. If a name has drifted in the meantime, consult `tests/recall_bench.rs` (the Wave 5 probe at the top) for the exact names.

- [ ] **Step 4: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add tests/bench/longmemeval.rs tests/bench/mod.rs
git commit -m "$(cat <<'EOF'
feat(bench): ingest_corpus helper for LongMemEval

Maps a LongMemEvalQuestion's haystack_sessions x turns onto a fresh
DuckDbRepository + VectorIndex. Tests use FakeEmbeddingProvider for
structural validation; the bench harness (Task 8) wires
EmbedAnythingEmbeddingProvider for real-data runs.
EOF
)"
```

---

## Task 5: Retrieval helper — `retrieve_for_rung`

**Files:**
- Modify: `tests/bench/longmemeval.rs`

Per spec section "Retrieval Contract": one function takes the populated repo + index, the question text, and a `LongMemEvalRung`, and returns a deduped `Vec<SessionId>` ranked best-first (top-50 candidates -> session-level top-K projection).

- [ ] **Step 1: Append `LongMemEvalRung` + `RUNGS` const + `retrieve_for_rung`**

Append to `tests/bench/longmemeval.rs`:

```rust
use super::runner::SourceMix;
use mem::pipeline::transcript_recall::{score_candidates, ScoringOpts};
use std::collections::{HashMap, HashSet};

const TOP_K_CANDIDATES: usize = 50;

#[derive(Debug, Clone, Copy)]
pub struct LongMemEvalRung {
    pub rung_id: &'static str,
    pub mempalace_label: &'static str,
    pub source: SourceMix,
    pub disable_session_cooc: bool,
    pub disable_anchor: bool,
    pub disable_freshness: bool,
}

#[rustfmt::skip]
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
        disable_session_cooc: false,
        disable_anchor: true,
        disable_freshness: true,
    },
    LongMemEvalRung {
        rung_id: "longmemeval_full",
        mempalace_label: "full",
        source: SourceMix::Both,
        disable_session_cooc: false,
        disable_anchor: false,
        disable_freshness: false,
    },
];

/// Retrieve and rank under the given rung's config; project to session-level
/// top-K. Caller supplies an already-populated repo + vector index for the
/// current question's corpus.
pub async fn retrieve_for_rung(
    repo: &DuckDbRepository,
    index: &VectorIndex,
    embedder: &Arc<dyn EmbeddingProvider>,
    query_text: &str,
    rung: &LongMemEvalRung,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // 1. Get candidates per source mix.
    let bm25 = match rung.source {
        SourceMix::Bm25Only | SourceMix::Both => {
            repo.bm25_transcript_candidates(TENANT, query_text, TOP_K_CANDIDATES)
                .await
                .unwrap_or_default()
        }
        SourceMix::HnswOnly => vec![],
    };
    let hnsw_ids: Vec<(String, f32)> = match rung.source {
        SourceMix::HnswOnly | SourceMix::Both => {
            let qv = embedder.embed_text(query_text).await?;
            index.search(&qv, TOP_K_CANDIDATES).unwrap_or_default()
        }
        SourceMix::Bm25Only => vec![],
    };

    // 2. Build rank maps (rank starts at 1).
    let mut lex_ranks: HashMap<String, usize> = HashMap::new();
    for (i, m) in bm25.iter().enumerate() {
        lex_ranks.insert(m.message_block_id.clone(), i + 1);
    }
    let mut sem_ranks: HashMap<String, usize> = HashMap::new();
    for (i, (id, _)) in hnsw_ids.iter().enumerate() {
        sem_ranks.insert(id.clone(), i + 1);
    }

    // 3. Hydrate HNSW candidates back to ConversationMessage. BM25 already
    // returned full records; for HNSW-only ids, fetch from repo by id.
    let mut by_id: HashMap<String, ConversationMessage> = HashMap::new();
    for m in bm25.into_iter() {
        by_id.entry(m.message_block_id.clone()).or_insert(m);
    }
    for (id, _) in &hnsw_ids {
        if !by_id.contains_key(id) {
            if let Ok(Some(m)) =
                repo.get_conversation_message_by_block_id(TENANT, id).await
            {
                by_id.insert(id.clone(), m);
            }
        }
    }
    let candidates: Vec<ConversationMessage> = by_id.into_values().collect();

    // 4. Score via production pipeline.
    let opts = ScoringOpts {
        anchor_session_id: None, // LongMemEval has no anchor concept
        disable_session_cooc: rung.disable_session_cooc,
        disable_anchor: rung.disable_anchor,
        disable_freshness: rung.disable_freshness,
    };
    let scored = score_candidates(candidates, &lex_ranks, &sem_ranks, opts);

    // 5. Project to session-level top-K (highest-score block per session).
    let mut session_seen: HashSet<String> = HashSet::new();
    let mut run: Vec<String> = Vec::with_capacity(20);
    for sb in scored {
        if let Some(sid) = sb.message.session_id.clone() {
            if session_seen.insert(sid.clone()) {
                run.push(sid);
                if run.len() >= 20 {
                    break;
                }
            }
        }
    }
    Ok(run)
}
```

- [ ] **Step 2: Add a unit test for `retrieve_for_rung`**

Append to the existing `mod tests`:

```rust
#[tokio::test]
async fn retrieve_for_rung_returns_session_level_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = DuckDbRepository::open(&tmp.path().join("ret.duckdb"))
        .await
        .unwrap();
    repo.set_transcript_job_provider("fake");
    let index = VectorIndex::new_in_memory(64, "fake", "fake", 16).unwrap();
    let embedder: Arc<dyn EmbeddingProvider> =
        Arc::new(FakeEmbeddingProvider::new("fake", 64));

    let q = make_question(
        "lme_q_test",
        vec![
            ("sess_a", vec![("user", "tokio rust async runtime")]),
            ("sess_b", vec![("user", "duckdb columnar storage")]),
            ("sess_c", vec![("user", "tokio futures")]),
        ],
    );
    ingest_corpus(&repo, &index, &embedder, &q).await.unwrap();

    let rung = RUNGS[0]; // longmemeval_raw
    let run = retrieve_for_rung(&repo, &index, &embedder, "tokio runtime", &rung)
        .await
        .unwrap();
    assert!(!run.is_empty(), "raw rung should return some sessions");
    assert!(
        run.iter().all(|s| ["sess_a", "sess_b", "sess_c"].contains(&s.as_str())),
        "all returned ids should be from the ingested sessions, got {:?}",
        run
    );
    // Session ids are unique (no duplicates from session-level projection).
    let unique: HashSet<&String> = run.iter().collect();
    assert_eq!(unique.len(), run.len());
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --test mempalace_bench bench::longmemeval::tests -q
```

Expected: 3 passed (2 ingest + 1 retrieve).

If you see "no method named `get_conversation_message_by_block_id`" — that exact method name may not exist on `DuckDbRepository`; check `src/storage/transcript_repo.rs` for the actual lookup-by-id method (could be `fetch_conversation_message_by_id` / `get_message_by_id` / similar) and adjust the call site. The point is: retrieve a `ConversationMessage` by its primary key string. If no such public method exists, write one — but only as a thin wrapper, no logic.

- [ ] **Step 4: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add tests/bench/longmemeval.rs
git commit -m "$(cat <<'EOF'
feat(bench): retrieve_for_rung — 3-rung retrieval + session-level projection

Mirrors Wave 5 ablation runner's retrieve_and_rank shape but adapted
for LongMemEval's session-level scoring: returns Vec<SessionId>
(deduped, top-20 sessions). The 3 rungs (raw / rooms / full) each
have distinct (SourceMix, ScoringOpts) tuples that map to
mempalace's three published baselines.
EOF
)"
```

---

## Task 6: Per-question + bench-level orchestration

**Files:**
- Modify: `tests/bench/longmemeval.rs`

Per spec section "Architecture Key Decision 2": one function ingests ONCE per question, then re-ranks under all 3 rungs. Aggregate across all questions at the end.

- [ ] **Step 1: Append types + orchestrator**

```rust
use mem::config::Config;
use mem::embedding::instance::create_embedding_provider;
use mem::pipeline::eval_metrics::{ndcg_at_k, recall_all_at_k, recall_any_at_k};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct PerQuestionMetrics {
    pub question_id: String,
    pub recall_any_at_5: f64,
    pub recall_any_at_10: f64,
    pub recall_all_at_5: f64,
    pub recall_all_at_10: f64,
    pub ndcg_at_10: f64,
    pub ranked_session_ids: Vec<String>,
    pub answer_session_ids: Vec<String>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub struct RungReport {
    pub rung_id: String,
    pub mempalace_label: String,
    pub aggregate_recall_any_at_5: f64,
    pub aggregate_recall_any_at_10: f64,
    pub aggregate_recall_all_at_5: f64,
    pub aggregate_recall_all_at_10: f64,
    pub aggregate_ndcg_at_10: f64,
    pub per_question: Vec<PerQuestionMetrics>,
}

#[derive(Debug, Clone)]
pub struct BenchReport {
    pub system_version: String,
    pub embedding_model: String,
    pub timestamp_ms: u128,
    pub limit: usize,
    pub rungs: Vec<RungReport>,
}

/// Run the full LongMemEval bench across the given questions.
/// For each question: ingest once, retrieve under each of the 3 rungs,
/// score against the gold answer_session_ids, aggregate into RungReports.
pub async fn run_longmemeval_bench(
    questions: Vec<LongMemEvalQuestion>,
) -> Result<BenchReport, Box<dyn std::error::Error>> {
    // Build the production embedding provider from env-var config.
    let cfg = Config::from_env()?;
    let embedder: Arc<dyn EmbeddingProvider> = create_embedding_provider(&cfg.embedding)?;
    let embedding_dim = cfg.embedding.dim;
    let embedding_model = cfg.embedding.model.clone();
    let warn_if_fake = matches!(
        cfg.embedding.provider,
        mem::config::EmbeddingProviderKind::Fake
    );
    if warn_if_fake {
        eprintln!(
            "WARNING: EMBEDDING_PROVIDER=fake — bench numbers will be \
             meaningless for cross-system comparison. Set \
             EMBEDDING_PROVIDER=embedanything (or similar) before running."
        );
    }

    let mut per_rung_metrics: Vec<Vec<PerQuestionMetrics>> = vec![vec![]; RUNGS.len()];
    let total_qs = questions.len();
    eprintln!("[bench] running LongMemEval over {} questions x 3 rungs", total_qs);

    for (q_idx, question) in questions.iter().enumerate() {
        if q_idx % 25 == 0 {
            eprintln!("[bench] progress: {}/{}", q_idx, total_qs);
        }
        let q_start = Instant::now();

        // Per-question fresh DB + index. Ingest once.
        let tmp = tempfile::TempDir::new()?;
        let repo = DuckDbRepository::open(&tmp.path().join("bench.duckdb")).await?;
        repo.set_transcript_job_provider(&cfg.embedding.provider.to_string());
        let total_blocks: usize = question.haystack_sessions.iter().map(|s| s.turns.len()).sum();
        let index = VectorIndex::new_in_memory(
            embedding_dim,
            "bench",
            &embedding_model,
            total_blocks.max(8),
        )?;
        ingest_corpus(&repo, &index, &embedder, question).await?;

        // Re-rank under each rung.
        let qrels: HashSet<String> = question.answer_session_ids.iter().cloned().collect();
        for (rung_idx, rung) in RUNGS.iter().enumerate() {
            let run = retrieve_for_rung(&repo, &index, &embedder, &question.question, rung).await?;
            let elapsed_ms = q_start.elapsed().as_millis();
            let metrics = PerQuestionMetrics {
                question_id: question.question_id.clone(),
                recall_any_at_5: recall_any_at_k(&run, &qrels, 5),
                recall_any_at_10: recall_any_at_k(&run, &qrels, 10),
                recall_all_at_5: recall_all_at_k(&run, &qrels, 5),
                recall_all_at_10: recall_all_at_k(&run, &qrels, 10),
                ndcg_at_10: ndcg_at_k(&run, &qrels, 10),
                ranked_session_ids: run.clone(),
                answer_session_ids: question.answer_session_ids.clone(),
                elapsed_ms,
            };
            per_rung_metrics[rung_idx].push(metrics);
        }
    }

    let mut rung_reports: Vec<RungReport> = Vec::with_capacity(RUNGS.len());
    for (rung_idx, rung) in RUNGS.iter().enumerate() {
        let pqs = &per_rung_metrics[rung_idx];
        let n = pqs.len() as f64;
        let mean = |sel: fn(&PerQuestionMetrics) -> f64| -> f64 {
            if n == 0.0 {
                0.0
            } else {
                pqs.iter().map(sel).sum::<f64>() / n
            }
        };
        rung_reports.push(RungReport {
            rung_id: rung.rung_id.to_string(),
            mempalace_label: rung.mempalace_label.to_string(),
            aggregate_recall_any_at_5: mean(|p| p.recall_any_at_5),
            aggregate_recall_any_at_10: mean(|p| p.recall_any_at_10),
            aggregate_recall_all_at_5: mean(|p| p.recall_all_at_5),
            aggregate_recall_all_at_10: mean(|p| p.recall_all_at_10),
            aggregate_ndcg_at_10: mean(|p| p.ndcg_at_10),
            per_question: pqs.clone(),
        });
    }

    Ok(BenchReport {
        system_version: git_short_sha().unwrap_or_else(|| "unknown".to_string()),
        embedding_model,
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        limit: total_qs,
        rungs: rung_reports,
    })
}

/// Best-effort git short SHA via `git rev-parse`. Returns None if git
/// isn't available or this isn't a repo.
fn git_short_sha() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
```

The use of `mem::embedding::instance::create_embedding_provider` is the production factory — check `src/embedding/instance.rs` for the exact pub function name (it's likely just `create_embedding_provider` or similar). Adjust if the name is different.

The `cfg.embedding.provider.to_string()` call requires `Display` on `EmbeddingProviderKind` — if it doesn't have one, use a small match: `match cfg.embedding.provider { EmbeddingProviderKind::Fake => "fake", EmbeddingProviderKind::EmbedAnything => "embedanything", EmbeddingProviderKind::OpenAi => "openai" }`.

- [ ] **Step 2: Run a sanity-check unit test**

Append to the existing `mod tests`:

```rust
#[tokio::test]
async fn run_longmemeval_bench_returns_3_rungs_for_tiny_input() {
    // Uses production Config::from_env. Set EMBEDDING_PROVIDER=fake for
    // this test to avoid model download. Other env vars must be valid.
    std::env::set_var("EMBEDDING_PROVIDER", "fake");
    std::env::set_var("EMBEDDING_MODEL", "fake");
    std::env::set_var("EMBEDDING_DIM", "64");

    let mut q = make_question(
        "lme_q_smoke",
        vec![
            ("sess_a", vec![("user", "tokio rust async")]),
            ("sess_b", vec![("user", "duckdb columnar")]),
        ],
    );
    q.answer_session_ids = vec!["sess_a".to_string()];
    let report = run_longmemeval_bench(vec![q]).await.unwrap();
    assert_eq!(report.rungs.len(), 3);
    for rung_report in &report.rungs {
        assert_eq!(rung_report.per_question.len(), 1);
    }
    assert_eq!(report.limit, 1);
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --test mempalace_bench bench::longmemeval::tests::run_longmemeval_bench -q
```

Expected: 1 passed (plus the 3 prior tests).

If you see "no function named `create_embedding_provider`" — find the actual factory function in `src/embedding/instance.rs`. There's only one or two pub fns there; pick the one that takes `&EmbeddingSettings` and returns `Result<Arc<dyn EmbeddingProvider>, _>`.

- [ ] **Step 4: Verify all four bench tests pass + CI gates**

```bash
cargo test --test mempalace_bench bench::longmemeval::tests -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: 4 passed; lints clean.

- [ ] **Step 5: Commit**

```bash
git add tests/bench/longmemeval.rs
git commit -m "$(cat <<'EOF'
feat(bench): per-Q ingest + 3-rung re-rank orchestration

run_longmemeval_bench loops over questions, opens fresh
DuckDbRepository + VectorIndex per question, ingests once, then
re-ranks under each of the 3 rungs. Aggregates per-rung means across
questions; emits BenchReport with system_version (git SHA) +
embedding_model (from Config::from_env). Warns loudly when
EMBEDDING_PROVIDER=fake (numbers are meaningless for comparison).
EOF
)"
```

---

## Task 7: Output formatters — `print_comparison_table` + `write_per_rung_json`

**Files:**
- Modify: `tests/bench/longmemeval.rs`

Per spec sections "Output Format" + "Stdout Pretty Table": one function renders the comparison table to stdout (with mempalace baselines side-by-side); another writes 3 separate JSON files, one per rung, mirroring mempalace's `results_*.json` shape.

- [ ] **Step 1: Append the formatters**

```rust
use std::fmt::Write as _;
use std::path::Path;

const MEMPALACE_BASELINES: &[(&str, &str)] = &[
    ("longmemeval_raw", "raw    = 0.966 R@5"),
    ("longmemeval_rooms", "rooms  = 0.894 R@5"),
    ("longmemeval_full", "full   = (per README)"),
];

pub fn print_comparison_table(report: &BenchReport) {
    let mut out = String::new();
    let _ = writeln!(
        &mut out,
        "=== Mem vs MemPalace LongMemEval ({} questions, run {}) ===",
        report.limit, report.timestamp_ms
    );
    let _ = writeln!(
        &mut out,
        "                    R@5(any) R@10(any) NDCG@10  | mempalace baseline"
    );
    for r in &report.rungs {
        let baseline = MEMPALACE_BASELINES
            .iter()
            .find(|(id, _)| *id == r.rung_id)
            .map(|(_, b)| *b)
            .unwrap_or("(no baseline)");
        let _ = writeln!(
            &mut out,
            "{:<19}   {:.3}     {:.3}    {:.3}  | mempalace {}",
            r.rung_id,
            r.aggregate_recall_any_at_5,
            r.aggregate_recall_any_at_10,
            r.aggregate_ndcg_at_10,
            baseline
        );
    }
    let _ = writeln!(&mut out);
    let _ = writeln!(
        &mut out,
        "! Embedding-model parity caveat: mem uses {} while mempalace",
        report.embedding_model
    );
    let _ = writeln!(
        &mut out,
        "  uses all-MiniLM-L6-v2 (384-dim). The Δ between rungs IS reliable;"
    );
    let _ = writeln!(
        &mut out,
        "  absolute Δ vs mempalace baselines includes both ranking and"
    );
    let _ = writeln!(
        &mut out,
        "  embedding-model contributions."
    );
    print!("{}", out);
}

pub fn write_per_rung_json(
    report: &BenchReport,
    out_dir: &Path,
) -> Result<Vec<PathBuf>, std::io::Error> {
    std::fs::create_dir_all(out_dir)?;
    let mut paths = Vec::with_capacity(report.rungs.len());
    let ts = report.timestamp_ms;
    for r in &report.rungs {
        let filename = format!("results_mem_{}_{}.json", r.rung_id, ts);
        let path = out_dir.join(&filename);
        let payload = serde_json::json!({
            "benchmark": "longmemeval",
            "mode": r.mempalace_label,
            "system": "mem",
            "embedding_model": report.embedding_model,
            "system_version": report.system_version,
            "timestamp_ms": report.timestamp_ms,
            "limit": report.limit,
            "aggregate": {
                "recall_any_at_5": r.aggregate_recall_any_at_5,
                "recall_any_at_10": r.aggregate_recall_any_at_10,
                "recall_all_at_5": r.aggregate_recall_all_at_5,
                "recall_all_at_10": r.aggregate_recall_all_at_10,
                "ndcg_at_10": r.aggregate_ndcg_at_10,
            },
            "per_question": r.per_question.iter().map(|p| serde_json::json!({
                "question_id": p.question_id,
                "recall_any_at_5": p.recall_any_at_5,
                "recall_any_at_10": p.recall_any_at_10,
                "recall_all_at_5": p.recall_all_at_5,
                "recall_all_at_10": p.recall_all_at_10,
                "ndcg_at_10": p.ndcg_at_10,
                "ranked_session_ids": p.ranked_session_ids,
                "answer_session_ids": p.answer_session_ids,
                "elapsed_ms": p.elapsed_ms,
            })).collect::<Vec<_>>(),
        });
        std::fs::write(&path, serde_json::to_string_pretty(&payload)?)?;
        paths.push(path);
    }
    Ok(paths)
}
```

- [ ] **Step 2: Add unit tests for both formatters**

```rust
#[cfg(test)]
mod output_tests {
    use super::*;

    fn fixture_report() -> BenchReport {
        BenchReport {
            system_version: "abcd1234".to_string(),
            embedding_model: "Qwen3-test".to_string(),
            timestamp_ms: 1730000000000,
            limit: 50,
            rungs: vec![
                RungReport {
                    rung_id: "longmemeval_raw".to_string(),
                    mempalace_label: "raw".to_string(),
                    aggregate_recall_any_at_5: 0.876,
                    aggregate_recall_any_at_10: 0.912,
                    aggregate_recall_all_at_5: 0.500,
                    aggregate_recall_all_at_10: 0.640,
                    aggregate_ndcg_at_10: 0.821,
                    per_question: vec![PerQuestionMetrics {
                        question_id: "lme_q_0001".to_string(),
                        recall_any_at_5: 1.0,
                        recall_any_at_10: 1.0,
                        recall_all_at_5: 0.0,
                        recall_all_at_10: 1.0,
                        ndcg_at_10: 0.85,
                        ranked_session_ids: vec!["s1".into(), "s2".into()],
                        answer_session_ids: vec!["s2".into()],
                        elapsed_ms: 320,
                    }],
                },
            ],
        }
    }

    #[test]
    fn print_comparison_table_does_not_panic() {
        let report = fixture_report();
        // Smoke-test the print path; the function writes to stdout so we
        // can't easily capture, but at minimum it should not panic.
        print_comparison_table(&report);
    }

    #[test]
    fn write_per_rung_json_creates_one_file_per_rung() {
        let report = fixture_report();
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = write_per_rung_json(&report, tmp.path()).unwrap();
        assert_eq!(paths.len(), 1);
        let bytes = std::fs::read(&paths[0]).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["benchmark"], "longmemeval");
        assert_eq!(parsed["mode"], "raw");
        assert_eq!(parsed["system"], "mem");
        assert_eq!(parsed["aggregate"]["recall_any_at_5"], 0.876);
        assert_eq!(parsed["per_question"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["per_question"][0]["question_id"], "lme_q_0001");
    }

    #[test]
    fn write_per_rung_json_filename_has_mem_prefix() {
        let report = fixture_report();
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = write_per_rung_json(&report, tmp.path()).unwrap();
        let filename = paths[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            filename.starts_with("results_mem_longmemeval_raw_"),
            "expected results_mem_ prefix, got {filename}"
        );
        assert!(filename.ends_with(".json"));
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --test mempalace_bench bench::longmemeval::output_tests -q
```

Expected: 3 passed.

- [ ] **Step 4: Verify CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --test mempalace_bench -q
```

Expected: 7+ tests pass (including dataset + ingest + retrieve + run + 3 output), probe `#[ignore]`'d.

- [ ] **Step 5: Commit**

```bash
git add tests/bench/longmemeval.rs
git commit -m "$(cat <<'EOF'
feat(bench): print_comparison_table + write_per_rung_json

Stdout table includes mempalace baselines side-by-side and the
embedding-model parity caveat as footer. JSON output is one file
per rung (results_mem_longmemeval_<mode>_<unix_ts>.json) mirroring
mempalace's results_*.json shape so jq cross-system diff works.
EOF
)"
```

---

## Task 8: Test entry — `mempalace_bench::longmemeval`

**Files:**
- Modify: `tests/mempalace_bench.rs` (replace probe-only with full entry; keep probe as `#[ignore]`'d)

Per spec section "Test Harness": single `#[ignore]`'d test entry that loads dataset, runs bench, prints + writes results. No assertions — informational only.

- [ ] **Step 1: Append the entry to `tests/mempalace_bench.rs`**

The file currently has the Task 1 probe + `mod bench;`. Add:

```rust
use bench::longmemeval::{print_comparison_table, run_longmemeval_bench, write_per_rung_json};
use bench::longmemeval_dataset::load_from_env_or_skip;
use std::path::PathBuf;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "external dataset; set MEM_LONGMEMEVAL_PATH=..."]
async fn longmemeval() {
    let questions = match load_from_env_or_skip() {
        Some(qs) => qs,
        None => {
            eprintln!("MEM_LONGMEMEVAL_PATH not set; skipping bench");
            return;
        }
    };
    let limit = std::env::var("MEM_LONGMEMEVAL_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let questions: Vec<_> = questions.into_iter().take(limit).collect();
    eprintln!(
        "[bench] loaded {} questions (limit applied)",
        questions.len()
    );

    let report = run_longmemeval_bench(questions)
        .await
        .expect("run_longmemeval_bench");

    print_comparison_table(&report);

    let out_dir = PathBuf::from("target/bench-out");
    let paths = write_per_rung_json(&report, &out_dir).expect("write json");
    for p in &paths {
        eprintln!("[bench] wrote {}", p.display());
    }
    // No assertions — informational only (manual decision tool).
}
```

- [ ] **Step 2: Verify test entry compiles**

```bash
cargo build --tests
cargo test --test mempalace_bench longmemeval -- --ignored 2>&1 | head -20
```

Expected: builds clean. Running with `--ignored` will start `Config::from_env()` which depending on env may either skip (if `MEM_LONGMEMEVAL_PATH` unset) or attempt to load the model (if set + EMBEDDING_PROVIDER=embedanything). For this step, just ensure it COMPILES; full run is post-merge manual.

- [ ] **Step 3: Verify the bench is properly skipped without env vars**

```bash
unset MEM_LONGMEMEVAL_PATH
cargo test --test mempalace_bench longmemeval -- --ignored --nocapture 2>&1 | tail -5
```

Expected: prints "MEM_LONGMEMEVAL_PATH not set; skipping bench" and exits OK.

- [ ] **Step 4: Verify all CI gates**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -q
```

Expected: full suite clean (probe + bench both `#[ignore]`'d).

- [ ] **Step 5: Commit**

```bash
git add tests/mempalace_bench.rs
git commit -m "$(cat <<'EOF'
feat(bench): mempalace_bench::longmemeval test entry

#[ignore]'d tokio test that loads MEM_LONGMEMEVAL_PATH dataset (if
set), respects MEM_LONGMEMEVAL_LIMIT for smoke runs, runs the
3-rung LongMemEval bench, prints the comparison table to stdout,
and writes 3 results_mem_longmemeval_<mode>_<ts>.json files to
target/bench-out/. No assertions — informational only.
EOF
)"
```

---

## Task 9: Documentation — README + CHANGELOG + ROADMAP

**Files:**
- Modify: `README.md` (add bench section)
- Modify: `CHANGELOG.md` (add 2026-05-05 entry)
- Modify: `docs/ROADMAP.MD` (add row #15)

- [ ] **Step 1: Append README section**

Insert after the existing "Recall Quality Bench (transcripts)" section (Wave 5):

```markdown
## MemPalace LongMemEval Parity Bench

External-comparison benchmark for mem vs mempalace's published
LongMemEval baselines. Apple-to-apple at the protocol level: same
dataset (LongMemEval Standard), same per-Q ephemeral corpus, same
top-K retrieval, same Recall@5/Recall@10/NDCG@10 metrics. mem runs
its own ranking stack (BM25 + HNSW + ScoringOpts) under three
rungs (raw / rooms / full equivalents).

### Run

Pre-download `longmemeval_s_cleaned.json` from the LongMemEval
upstream repo (https://github.com/xiaowu0162/LongMemEval). Set
`EMBEDDING_PROVIDER=embedanything`, `EMBEDDING_MODEL=...`,
`EMBEDDING_DIM=...` per `.env.example`. Then:

    MEM_LONGMEMEVAL_PATH=/path/to/longmemeval_s_cleaned.json \
      cargo test --test mempalace_bench longmemeval -- --ignored --nocapture

For a smoke (50 questions instead of 500):

    MEM_LONGMEMEVAL_PATH=/path/... \
    MEM_LONGMEMEVAL_LIMIT=50 \
      cargo test --test mempalace_bench longmemeval -- --ignored --nocapture

Wall-clock: ~1.5-3 hours for 500 questions x 3 rungs (the embedding
ingest dominates; rung re-rank is fast).

### Reading the output

Three JSON files written to `target/bench-out/`:
- `results_mem_longmemeval_raw_<unix_ts>.json` (vs mempalace `raw` ≈ 0.966 R@5)
- `results_mem_longmemeval_rooms_<unix_ts>.json` (vs mempalace `rooms` ≈ 0.894 R@5)
- `results_mem_longmemeval_full_<unix_ts>.json` (vs mempalace `full` per their README)

Plus a stdout comparison table. The `! Embedding-model parity caveat`
footer notes that mem uses Qwen3 1024-dim while mempalace uses
all-MiniLM-L6-v2 384-dim — absolute mem-vs-mempalace deltas include
both ranking-algorithm AND embedding-model contributions.
```

- [ ] **Step 2: Append CHANGELOG entry**

Insert as the most-recent entry (above the 2026-05-03 recall-bench wave):

```markdown
## 2026-05-05 — MemPalace LongMemEval Parity Bench

### Added

- `tests/mempalace_bench.rs` — `#[ignore]`'d entry that runs LongMemEval against
  mem's stack and emits results JSON in mempalace-mirror shape
- `tests/bench/longmemeval.rs` — runner: 3 rungs (raw / rooms / full), per-Q
  ingest + re-rank, aggregate, output formatters
- `tests/bench/longmemeval_dataset.rs` — JSON loader with env-var skip semantics
- `src/pipeline/eval_metrics.rs` — `recall_any_at_k`, `recall_all_at_k` (mempalace-style binary indicators)

### Notes

- Manual decision tool, not in CI (matches mempalace's manual operation flow)
- Uses production embedding stack (configured via `EMBEDDING_*` env vars);
  warns if `EMBEDDING_PROVIDER=fake`
- `MEM_LONGMEMEVAL_PATH` env var points to a pre-downloaded dataset
- Mempalace's AAAK and llmrerank modes skipped (mem has no analog)
- `1.5-3 hour` wall-clock for full 500-question run; `MEM_LONGMEMEVAL_LIMIT=50`
  for smoke
```

- [ ] **Step 3: Append ROADMAP row**

In `docs/ROADMAP.MD`, after the existing #14 (Recall quality bench), add:

```markdown
| 15 | 🔍 | ✅ **MemPalace LongMemEval parity bench** (Rust port of `longmemeval_bench.py`; 3 rungs map to mempalace `raw / rooms / full`; output mirrors `results_*.json` shape; closes spec `2026-05-05-mempalace-bench-parity-design`) | 🟡 横向对比基础设施 | M（半天） | 低 | `src/pipeline/eval_metrics.rs`, `tests/bench/longmemeval*.rs`, `tests/mempalace_bench.rs` |
```

- [ ] **Step 4: Smoke run**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -q
cargo build --release
```

All clean. Release build is the meaningful smoke gate.

- [ ] **Step 5: Commit**

```bash
git add README.md CHANGELOG.md docs/ROADMAP.MD
git commit -m "$(cat <<'EOF'
docs(bench): document LongMemEval parity bench surface

README: how to download dataset, run full vs smoke, read output.
CHANGELOG: 2026-05-05 entry for the wave.
ROADMAP: row #15 for the bench.
EOF
)"
```

---

## Self-Review

**1. Spec coverage**

| Spec section | Plan task |
|---|---|
| Architecture (per-Q ephemeral corpus, ingest-once + 3-rung re-rank) | Task 4 + Task 6 |
| Dataset Loader (env-var, schema check, skip semantics) | Task 3 |
| Ingest Mapping (haystack_sessions x turns -> ConversationMessage) | Task 4 |
| Rung Definitions (raw / rooms / full = SourceMix + ScoringOpts triples) | Task 5 |
| Retrieval Contract (top-50 candidates, session-level top-K, anchor=None) | Task 5 |
| Eval Metrics Extension (recall_any_at_k, recall_all_at_k) | Task 2 |
| Output Format (results_mem_*.json mirror shape, mem_ prefix) | Task 7 |
| Stdout Pretty Table (mempalace baseline side-by-side, caveat footer) | Task 7 |
| Test Harness (#[ignore]'d, env-var driven, no assertions) | Task 8 |
| Risks (probe + sample inspection) | Task 1 (probe) |
| File Layout | All tasks (paths exact) |
| Documentation | Task 9 |

All sections covered.

**2. Placeholder scan**

No "TBD" / "TODO" / "implement later" inside step bodies. Where API names are best-guess (Task 4's `set_transcript_job_provider` / `create_conversation_message`, Task 5's `get_conversation_message_by_block_id`, Task 6's `create_embedding_provider`), an explicit "if name doesn't match, look here" pointer is provided to the implementer. This is honest scaffolding for unverifiable-from-plan facts, not a placeholder.

**3. Type consistency**

- `LongMemEvalQuestion` / `LongMemEvalSession` / `LongMemEvalTurn` (Task 3) -> consumed unchanged by Tasks 4, 5, 6.
- `LongMemEvalRung` / `RUNGS` / `SourceMix` (Task 5) -> consumed unchanged by Task 6.
- `PerQuestionMetrics` / `RungReport` / `BenchReport` (Task 6) -> consumed unchanged by Task 7 (formatters) and Task 8 (test entry).
- `recall_any_at_k` / `recall_all_at_k` / `ndcg_at_k` (Task 2 + existing) -> called from Task 6.
- `print_comparison_table` / `write_per_rung_json` (Task 7) -> called from Task 8.

All types consistent across tasks. No naming drift.

**4. Known unknowns flagged for implementer**

- Mempalace `results_*.jsonl` exact key naming (Task 1 probe: snake_case_at vs `@`)
- LongMemEval question_id presence (Task 3: backfill if absent — confirmed correct via `#[serde(default)]`)
- `DuckDbRepository::get_conversation_message_by_block_id` exact name (Task 5: pointer to check repo)
- `create_embedding_provider` factory function exact name (Task 6: pointer to check `instance.rs`)

If any of these resolve differently than the plan assumes, Tasks 1, 4, 5, or 6 may need a 1-line adjustment. The structural shape doesn't change.
