# Transcript Recall Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lift the recall quality of `POST /transcripts/search` to parity with the memories pipeline by adding (1) a BM25 lexical channel fused with HNSW via RRF, (2) intra-session co-occurrence + anchor + recency bonuses as freshness/decay substitutes, and (3) hydration of ±k context blocks around each primary hit, returned as merged conversation windows.

**Architecture:** Pure ranking helpers (RRF formula + freshness curve + timestamp parser) extract to `src/pipeline/ranking.rs` and are shared with the existing memories scorer (zero-behavior-change refactor). Transcript-specific scoring lives in a new `src/pipeline/transcript_recall.rs`; transcripts get their own `transcripts_fts_dirty` flag, `bm25_transcript_candidates` repo method, and lazy FTS index over `embed_eligible=true` rows only. Service layer assembles three candidate sources (HNSW + BM25 + optional anchor injection) into a single scored set, then hydrates and merges windows. HTTP response shape changes from `{hits}` to `{windows}` (breaking; only two in-tree callers).

**Tech Stack:** Rust 2021, DuckDB FTS (`pragma create_fts_index`, `match_bm25`), `usearch` HNSW (already wired), axum HTTP, tokio, integration tests in `tests/` against ephemeral DuckDB.

**Spec:** `docs/superpowers/specs/2026-05-01-transcript-recall-design.md` (commit `fed626e`).

---

## Conventions referenced throughout

- **Append-only schema files**: no new schema in this plan — FTS is lazy-built at runtime by code, not by a migration file. The FTS extension is already loaded by `005_fts.sql`.
- **Single-writer DB**: all writes serialize through `Arc<Mutex<Connection>>`. Same lock contract as conversation-archive plan.
- **Verbatim**: `conversation_messages.content` never trimmed/summarized. Hydration filters block_type but keeps content full.
- **Zero shared state across pipelines**: only pure functions in `pipeline/ranking.rs` are shared. No shared DB state, no shared trait objects.
- **Commit scope tags**: `feat(transcripts)`, `feat(ranking)`, `refactor(retrieve)`, `test(transcripts)`, etc.

---

## File Structure (locked decisions)

**Created:**
- `src/pipeline/ranking.rs` — `rrf_contribution`, `freshness_score`, `timestamp_score`, `RRF_K`, `RRF_SCALE` constants. Pure functions, no domain types.
- `src/pipeline/transcript_recall.rs` — transcript scorer (`score_candidates`, `ScoringOpts`, `ScoredBlock`), window assembly (`merge_windows`, `PrimaryWithContext`, `MergedWindow`), magnitude constants.
- `tests/transcript_recall.rs` — 10 integration tests covering BM25/HNSW/anchor/co-occurrence/hydration/window-merge.

**Modified:**
- `src/pipeline/mod.rs` — register `pub mod ranking; pub mod transcript_recall;`.
- `src/pipeline/retrieve.rs` — remove local `RRF_K`, `RRF_SCALE`, `freshness_score`, `timestamp_score` definitions; import from `super::ranking`. Zero behavior change.
- `src/storage/duckdb.rs` — add `transcripts_fts_dirty: Arc<AtomicBool>` field; init `true`; expose `pub(crate) fn set_transcripts_fts_dirty(&self)`.
- `src/storage/transcript_repo.rs` — `create_conversation_message` flips dirty flag on actual insert. Add `bm25_transcript_candidates`, `ensure_transcript_fts_index_fresh`, `context_window_for_block`.
- `src/service/transcript_service.rs::search` — three-channel candidate pool + scorer + hydration + window merge.
- `src/http/transcripts.rs` — `SearchRequest` adds three optional fields; `SearchResponse` reshaped to `{windows}` with `TranscriptWindow` / `TranscriptWindowBlock` DTOs.
- `tests/conversation_archive.rs::post_transcripts_search_filters_by_role_and_block_type` — assertions migrate from `hits[]` to `windows[].primary_ids` / `windows[].blocks`.
- `tests/integration_claude_code.rs::end_to_end_mine_then_search_then_get` — same migration.

**Untouched (verify in self-review):**
- `src/domain/*` (no new types)
- `src/cli/mine.rs` (mine doesn't call /transcripts/search)
- `src/mcp/server.rs` (MCP surface unchanged per spec Non-Goals)
- `src/service/embedding_worker.rs`, `src/service/transcript_embedding_worker.rs`
- `src/service/memory_service.rs`
- `src/storage/vector_index.rs`
- `db/schema/*` (no schema changes)

---

## Task 1: Extract `pipeline/ranking.rs` shared helpers (zero-behavior-change refactor)

**Files:**
- Create: `src/pipeline/ranking.rs`
- Modify: `src/pipeline/mod.rs`
- Modify: `src/pipeline/retrieve.rs`

- [ ] **Step 1: Create `src/pipeline/ranking.rs` with the three pure helpers**

```rust
//! Pure ranking helpers shared by the memories and transcripts retrieval
//! pipelines. No pipeline-specific types — only rank/timestamp arithmetic.
//! Adding code here is a deliberate decision to share math; do NOT add types
//! that name memory or transcript domain concepts.

/// Reciprocal Rank Fusion constant. Tuned for the "rank 1 hit ≈ 16" baseline
/// the memories pipeline already encodes; keep memories and transcripts in
/// lock-step so RRF magnitudes are comparable across pipelines if anyone
/// later builds a unified analytics view.
pub const RRF_K: usize = 60;

/// Multiplier applied to the raw `1 / (K + rank)` value before rounding to
/// `i64`. Combined with `RRF_K = 60`, a rank-1 hit yields:
///   `(1.0 / 61.0) * 1000.0 ≈ 16.39 → 16`.
pub const RRF_SCALE: f64 = 1000.0;

/// RRF contribution for a single appearance at the given rank in one channel
/// (lexical or semantic). Returns the same `i64` value the memories pipeline
/// has been using since the BM25 hybrid retrieval landed; transcripts share
/// the formula so RRF magnitudes are directly comparable.
pub fn rrf_contribution(rank: usize) -> i64 {
    ((RRF_SCALE / (RRF_K as f64 + rank as f64)).round()) as i64
}

/// Freshness curve: how much to credit a candidate based on how close its
/// timestamp is to the newest in the candidate pool. Range `[-14, 6]`:
/// returns `6` when `current >= newest` (i.e., this candidate is the newest
/// or in the future), then decays linearly in 10_000-ms buckets, saturating
/// at `-14` after 200 s of staleness.
///
/// The curve is intentionally tight (not a long-tail decay) — this signal
/// only acts as a tiebreaker behind RRF and lifecycle bonuses, not as a
/// dominant ranking factor.
pub fn freshness_score(newest: u128, current: u128) -> i64 {
    if newest <= current {
        return 6;
    }

    let delta = newest - current;
    let bucket = (delta / 10_000).min(20);
    6 - bucket as i64
}

/// Parse a timestamp string into a `u128` of milliseconds since epoch.
/// The codebase encodes timestamps as zero-padded 20-digit decimal strings
/// produced by `crate::storage::time::current_timestamp`. This helper
/// strips non-digit characters defensively (handles RFC-3339 sloppiness)
/// and returns `0` on parse failure (caller treats as "very old").
pub fn timestamp_score(value: &str) -> u128 {
    let digits = value
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse::<u128>().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_contribution_rank_1_is_16() {
        // Magnitude guard: (1.0 / 61.0) * 1000.0 ≈ 16.39 → round → 16.
        // Changing RRF_K or RRF_SCALE will fail this test, alerting the
        // engineer to update both pipelines' magnitude tables.
        assert_eq!(rrf_contribution(1), 16);
    }

    #[test]
    fn rrf_contribution_decreases_with_rank() {
        let r1 = rrf_contribution(1);
        let r10 = rrf_contribution(10);
        let r100 = rrf_contribution(100);
        assert!(r1 > r10);
        assert!(r10 > r100);
        assert!(r100 >= 0);
    }

    #[test]
    fn freshness_at_newest_is_max() {
        assert_eq!(freshness_score(1_000, 1_000), 6);
        assert_eq!(freshness_score(1_000, 999), 6); // newest <= current
        assert_eq!(freshness_score(1_000, 5_000), 6); // current in future
    }

    #[test]
    fn freshness_decays_in_10000ms_buckets() {
        // newest = 200_000ms, current = 195_000ms → delta = 5000 → bucket 0 → 6.
        assert_eq!(freshness_score(200_000, 195_000), 6);
        // delta = 15_000 → bucket 1 → 5.
        assert_eq!(freshness_score(200_000, 185_000), 5);
        // delta = 200_000 → bucket 20 (capped) → -14.
        assert_eq!(freshness_score(200_000, 0), -14);
        // delta = 1_000_000 → still capped at bucket 20 → -14.
        assert_eq!(freshness_score(1_000_000, 0), -14);
    }

    #[test]
    fn timestamp_score_extracts_digits() {
        assert_eq!(timestamp_score("00000000001234567890"), 1234567890);
        assert_eq!(
            timestamp_score("2026-04-30T00:00:00Z"),
            2026043000000000 // digits concatenated
        );
        assert_eq!(timestamp_score("not a number"), 0);
    }
}
```

- [ ] **Step 2: Register the module**

In `src/pipeline/mod.rs`, add:

```rust
pub mod ranking;
```

Place adjacent to other `pub mod` declarations (alphabetically near `compress`, `ingest`, `retrieve`, `session`, `workflow`).

- [ ] **Step 3: Run the new module's tests to verify they pass**

```bash
cargo test --lib pipeline::ranking -q
```
Expected: 5 tests pass.

- [ ] **Step 4: Refactor `pipeline/retrieve.rs` to use the shared helpers**

In `src/pipeline/retrieve.rs`:

(a) Add the import near the top of the file:

```rust
use crate::pipeline::ranking::{freshness_score, rrf_contribution, timestamp_score, RRF_K, RRF_SCALE};
```

Note: keep `RRF_K` and `RRF_SCALE` imported even if only the function is used directly — the file's tests reference the constants at lines 740, 875, 925.

(b) Delete the local definitions:
- Delete `const RRF_K: usize = 60;` and `const RRF_SCALE: f64 = 1000.0;` (lines 327-328).
- Delete `fn freshness_score(newest: u128, current: u128) -> i64 { ... }` (lines 635-643).
- Delete `fn timestamp_score(value: &str) -> u128 { ... }` (lines 672-678).

(c) Replace the inline RRF math at line 360. The existing code is:

```rust
let rrf_lex = lexical_ranks
    .get(&memory.memory_id)
    .map(|&r| 1.0_f64 / (RRF_K as f64 + r as f64))
    .unwrap_or(0.0);
let rrf_sem = semantic_ranks
    .get(&memory.memory_id)
    .map(|&r| 1.0_f64 / (RRF_K as f64 + r as f64))
    .unwrap_or(0.0);
score += ((rrf_lex + rrf_sem) * RRF_SCALE).round() as i64;
```

Replace with:

```rust
let rrf_lex = lexical_ranks
    .get(&memory.memory_id)
    .map(|&r| rrf_contribution(r))
    .unwrap_or(0);
let rrf_sem = semantic_ranks
    .get(&memory.memory_id)
    .map(|&r| rrf_contribution(r))
    .unwrap_or(0);
score += rrf_lex + rrf_sem;
```

**Verify magnitude preservation by hand**: at rank 1, the old expression evaluated `((1.0/61.0 + 1.0/61.0) * 1000.0).round() = 32.79 → 33`. The new expression evaluates `rrf_contribution(1) + rrf_contribution(1) = 16 + 16 = 32`. **One-off difference!**

This is the sum-then-round vs round-then-sum subtle change. The existing rrf-only memories tests (`rrf_recall_only_lexical`, `rrf_both_paths_top_rank`, `rrf_rank_monotonic`, `lex_only_candidate_has_nonzero_recall_after_rrf`) assert exact integer scores. We need to inspect them and adjust.

(d) Inspect the affected memories tests and decide:

```bash
cargo test --lib pipeline::retrieve::tests::rrf_ -q
```

If the tests fail with off-by-one differences: the migration is faithful in *behavior* (same RRF formula, same monotonic ordering) but **not** in exact integer scores. The spec's "zero behavior change" goal must be reframed: the *ranking order* is preserved, but specific integer scores may shift by ±1 due to the round-then-sum. Two options:

- Option A (preferred — preserves test assertions): keep the old sum-then-round in retrieve.rs by using a private helper that takes the raw float and returns i64:
  ```rust
  let rrf_lex_raw = lexical_ranks
      .get(&memory.memory_id)
      .map(|&r| 1.0 / (RRF_K as f64 + r as f64))
      .unwrap_or(0.0);
  let rrf_sem_raw = semantic_ranks
      .get(&memory.memory_id)
      .map(|&r| 1.0 / (RRF_K as f64 + r as f64))
      .unwrap_or(0.0);
  score += ((rrf_lex_raw + rrf_sem_raw) * RRF_SCALE).round() as i64;
  ```
  In this case, retrieve.rs uses `RRF_K` and `RRF_SCALE` from `ranking` but does NOT call `rrf_contribution`. Transcripts will call `rrf_contribution` for its round-then-sum. The two pipelines diverge by ±1 in mixed-rank scenarios but agree at rank 1 alone.

- Option B (preferred — single shared rounding):  update memories tests to expect the new `2 * 16 = 32` value, document the ±1 drift in the commit message and CLAUDE.md.

**Decision in this plan: Option A.** Reasons:
  - The spec promised zero behavior change for memories.
  - Memories tests are already in master and the user expects no surprise score shifts.
  - Transcripts is greenfield — `rrf_contribution` round-then-sum is fine there.
  - The shared helper still exists; transcripts uses it; memories continues using the raw formula but reads `RRF_K` and `RRF_SCALE` from the shared module.

Apply Option A: revert the inline change at line 360 to use the float math + shared constants:

```rust
let rrf_lex = lexical_ranks
    .get(&memory.memory_id)
    .map(|&r| 1.0 / (RRF_K as f64 + r as f64))
    .unwrap_or(0.0);
let rrf_sem = semantic_ranks
    .get(&memory.memory_id)
    .map(|&r| 1.0 / (RRF_K as f64 + r as f64))
    .unwrap_or(0.0);
score += ((rrf_lex + rrf_sem) * RRF_SCALE).round() as i64;
```

This is byte-equivalent to before — only the constant SOURCE moved. `rrf_contribution` is unused here but available for transcripts.

(e) Other call sites for `freshness_score` and `timestamp_score` in retrieve.rs:

- Line 246: `score += freshness_score(newest, timestamp_score(&memory.updated_at));` → no change (just imports).
- Lines 273, 317-318, 340, 386-387, 404, 432-433: `timestamp_score(&memory.updated_at)` → no change.
- Lines 740, 862, 875, 888, 925 (test code): no change.

(f) The constants `RRF_K` and `RRF_SCALE` referenced in tests (lines 740, etc.) — change references to `crate::pipeline::ranking::RRF_K` etc. or use the existing `use` statement at the top.

- [ ] **Step 5: Run all relevant test suites and confirm zero behavior change**

```bash
cargo test --lib pipeline::retrieve -q
cargo test --test search_api -q
cargo test --test bm25_search -q
cargo test --test hybrid_search -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

All must pass. Specifically: the rrf_* tests in `pipeline::retrieve::tests` must still produce the same integer scores they did before. If any score changes, you've drifted from Option A — re-inspect step 4(d).

- [ ] **Step 6: Commit**

```bash
git add src/pipeline/ranking.rs src/pipeline/mod.rs src/pipeline/retrieve.rs
git commit -m "refactor(retrieve): extract pipeline/ranking.rs shared helpers

Pure functions rrf_contribution, freshness_score, timestamp_score
extracted to pipeline/ranking.rs along with RRF_K and RRF_SCALE
constants. retrieve.rs imports them; the inline RRF formula is
preserved verbatim (sum-then-round) so memories test scores don't drift.

transcripts pipeline (forthcoming) will use rrf_contribution directly
and accept the ±1 round-then-sum semantics for its own scoring.

Zero behavior change for memories: pipeline::retrieve, search_api,
bm25_search, hybrid_search test suites all unchanged."
```

---

## Task 2: Probe DuckDB FTS `where := '...'` predicate support

**Files:**
- Test: ad-hoc probe (no commit yet)

This is a 5-minute investigation step that informs Task 3. The spec flags it as Concern #1.

- [ ] **Step 1: Write a tiny probe Rust binary OR shell snippet**

The simplest probe: open a fresh DuckDB, install fts, create a small table, and try the predicate index:

```bash
cat > /tmp/fts-probe.sql << 'EOF'
INSTALL fts;
LOAD fts;
CREATE TABLE t (id TEXT PRIMARY KEY, content TEXT, eligible BOOLEAN);
INSERT INTO t VALUES ('a', 'hello world', true), ('b', 'goodbye world', false);
PRAGMA create_fts_index('t', 'id', 'content', where := 'eligible = true');
SELECT id, fts_main_t.match_bm25(id, 'world') AS s FROM t WHERE fts_main_t.match_bm25(id, 'world') IS NOT NULL;
EOF
```

Run it with the same DuckDB version this project uses. From the project root:

```bash
cargo build --bin mem 2>/dev/null && cd /tmp && cargo run --manifest-path /root/workspace/master/mem/Cargo.toml -- --version 2>/dev/null
# (we'll actually probe via a tiny Rust test)
```

OR, easier — write a one-shot `#[ignore]`'d probe test in `tests/transcript_recall.rs` (file doesn't exist yet, that's fine):

```rust
// File: tests/transcript_recall.rs (new)
// Probe-only — confirms DuckDB FTS supports the `where := '...'` predicate.
// Run manually: cargo test --test transcript_recall fts_predicate_probe -- --ignored

#[test]
#[ignore]
fn fts_predicate_probe() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("probe.duckdb");
    let conn = duckdb::Connection::open(&db).unwrap();
    conn.execute_batch("install fts; load fts;").unwrap();
    conn.execute_batch(
        "create table t (id text primary key, content text, eligible boolean);
         insert into t values ('a', 'hello world', true), ('b', 'goodbye world', false);"
    ).unwrap();

    let result = conn.execute_batch(
        "pragma create_fts_index('t', 'id', 'content', where := 'eligible = true');"
    );

    match result {
        Ok(_) => {
            // Verify only eligible row is indexed.
            let count: i64 = conn
                .query_row(
                    "select count(*) from t where fts_main_t.match_bm25(id, 'world') is not null",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "only eligible row should be in index");
            println!("FTS predicate index SUPPORTED");
        }
        Err(e) => {
            println!("FTS predicate index NOT SUPPORTED: {}", e);
            // Don't fail the test — the probe is informational. Caller decides which path to take in Task 3.
        }
    }
}
```

- [ ] **Step 2: Run the probe**

```bash
cargo test --test transcript_recall fts_predicate_probe -- --ignored --nocapture
```

The test prints either `SUPPORTED` or `NOT SUPPORTED: <error>`.

- [ ] **Step 3: Record the outcome**

If **SUPPORTED**: Task 3 uses `pragma create_fts_index('conversation_messages', 'message_block_id', 'content', where := 'embed_eligible = true')`. The BM25 SQL has no extra `WHERE` clause for embed_eligible (the index already excludes ineligible rows).

If **NOT SUPPORTED**: Task 3 builds the index over the full table:
```sql
pragma create_fts_index('conversation_messages', 'message_block_id', 'content');
```
And the BM25 SQL adds the eligibility filter:
```sql
... where tenant = ?2 and embed_eligible = true ...
```

The probe test stays in the file with `#[ignore]` so engineers can re-verify on DuckDB upgrades. Optionally: leave a comment near the probe noting which branch (SUPPORTED / NOT SUPPORTED) the implementation took.

- [ ] **Step 4: Don't commit yet** — Task 3 will commit the probe alongside the BM25 method.

---

## Task 3: Storage — `transcripts_fts_dirty` flag + BM25 method

**Files:**
- Modify: `src/storage/duckdb.rs` (struct field + setter)
- Modify: `src/storage/transcript_repo.rs` (flip dirty + BM25 method + ensure_fresh helper)
- Modify: `tests/transcript_recall.rs` (add new failing test alongside the probe)

- [ ] **Step 1: Write the failing integration test**

In `tests/transcript_recall.rs` (the file from Task 2; the probe stays at the top), add module-level setup helpers and the first real test:

```rust
//! Integration tests for the new transcript-recall path.
//! See spec docs/superpowers/specs/2026-05-01-transcript-recall-design.md.

use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::storage::DuckDbRepository;
use tempfile::TempDir;

mod common;

fn sample_block(suffix: &str, content: &str, block_type: BlockType, embed: bool) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("mb-{suffix}"),
        session_id: Some("S1".to_string()),
        tenant: "local".to_string(),
        caller_agent: "claude-code".to_string(),
        transcript_path: "/tmp/t.jsonl".to_string(),
        line_number: 1,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type,
        content: content.to_string(),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: embed,
        created_at: "00000000020260430000".to_string(),
    }
}

#[tokio::test]
async fn bm25_finds_lexical_match_in_text_block() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // Seed with three text blocks, only one mentions "Rust".
    let mut a = sample_block("a", "the user asked about Python", BlockType::Text, true);
    a.line_number = 1;
    let mut b = sample_block("b", "we discussed the Rust project layout", BlockType::Text, true);
    b.line_number = 2;
    let mut c = sample_block("c", "JavaScript notes follow", BlockType::Text, true);
    c.line_number = 3;
    repo.create_conversation_message(&a).await.unwrap();
    repo.create_conversation_message(&b).await.unwrap();
    repo.create_conversation_message(&c).await.unwrap();

    let hits = repo
        .bm25_transcript_candidates("local", "Rust", 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "exactly one block should match 'Rust'");
    assert_eq!(hits[0].message_block_id, "mb-b");
}

#[tokio::test]
async fn bm25_excludes_tool_blocks() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // A tool_use block whose JSON content mentions "rare-keyword" — must NOT match
    // because tool blocks are not embed-eligible and therefore not in the BM25 index.
    let mut tool = sample_block(
        "tool",
        r#"{"file_path":"rare-keyword.toml"}"#,
        BlockType::ToolUse,
        false, // embed_eligible = false, matches the schema's check constraint
    );
    tool.line_number = 1;
    repo.create_conversation_message(&tool).await.unwrap();

    let hits = repo
        .bm25_transcript_candidates("local", "rare-keyword", 5)
        .await
        .unwrap();
    assert!(hits.is_empty(), "tool blocks must not surface in BM25");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --test transcript_recall bm25_ -q
```
Expected: FAIL — `bm25_transcript_candidates` does not exist on `DuckDbRepository`.

- [ ] **Step 3: Add the `transcripts_fts_dirty` field**

In `src/storage/duckdb.rs`, find the `DuckDbRepository` struct and add the field. The struct already has `fts_dirty: Arc<AtomicBool>` from the master BM25 work — add the new one immediately below:

```rust
pub struct DuckDbRepository {
    conn: Arc<Mutex<Connection>>,
    vector_index: Arc<RwLock<Option<Arc<VectorIndex>>>>,
    transcript_job_provider: Arc<RwLock<Option<String>>>,
    /// Set whenever a row is inserted into `memories` whose summary/content
    /// would change BM25 results. Cleared by `bm25_candidates` after a
    /// successful index rebuild. DuckDB FTS is non-incremental, so we
    /// rebuild lazily on the next BM25 query rather than after every write.
    fts_dirty: Arc<AtomicBool>,
    /// Same idea as `fts_dirty` but for the `conversation_messages` FTS index.
    /// Independent flag — transcripts and memories share zero state. Initial
    /// value is `true` so the first BM25 query after a process restart always
    /// rebuilds (defends against stale sidecars across restarts).
    transcripts_fts_dirty: Arc<AtomicBool>,
}
```

Initialize the field in `DuckDbRepository::open(...)`:

```rust
Ok(Self {
    conn: Arc::new(Mutex::new(conn)),
    vector_index: Arc::new(RwLock::new(None)),
    transcript_job_provider: Arc::new(RwLock::new(None)),
    fts_dirty: Arc::new(AtomicBool::new(true)),
    transcripts_fts_dirty: Arc::new(AtomicBool::new(true)),
})
```

Add the `pub(crate)` setter near the existing `set_fts_dirty` (find it via grep):

```rust
pub(crate) fn set_transcripts_fts_dirty(&self) {
    self.transcripts_fts_dirty.store(true, Ordering::Release);
}
```

- [ ] **Step 4: Flip the flag on transcript inserts**

In `src/storage/transcript_repo.rs`, find `create_conversation_message`. Inside the `tokio::task::spawn_blocking` closure, after the conditional `transcript_embedding_jobs` insert succeeds, add the flag flip — but **only when the message row was actually inserted** (not when ON CONFLICT swallowed it). The existing code likely tracks `affected_rows == 1`; flip `transcripts_fts_dirty` in the same branch.

Sketch (assuming the function uses an `inserted: bool` local):

```rust
// Existing code:
if inserted == 1 {
    if msg.embed_eligible {
        // ... insert into transcript_embedding_jobs ...
    }
    // NEW:
    self.set_transcripts_fts_dirty();
}
```

The setter is `&self`, so it can be called from inside `spawn_blocking` provided the closure captures `self` by clone (it should — that's the existing pattern; verify by reading `create_conversation_message`).

- [ ] **Step 5: Implement `bm25_transcript_candidates` and `ensure_transcript_fts_index_fresh`**

In `src/storage/transcript_repo.rs`, alongside `create_conversation_message`, add:

```rust
impl DuckDbRepository {
    /// BM25 lexical candidates over `conversation_messages.content`, restricted
    /// to `embed_eligible = true` rows (matching the HNSW pipeline's coverage).
    /// Returns up to `k` rows ordered by BM25 score descending.
    ///
    /// The FTS index is built lazily by `ensure_transcript_fts_index_fresh`.
    pub async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<crate::domain::ConversationMessage>, super::duckdb::StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(vec![]);
        }
        self.ensure_transcript_fts_index_fresh()?;

        // BRANCH: if the FTS predicate index probe (Task 2) succeeded, the
        // index already excludes ineligible rows. Otherwise we filter here.
        // Replace the `BM25_FILTER` placeholder below with the right SQL after
        // recording Task 2's outcome:
        //   SUPPORTED:    BM25_FILTER = ""    (no extra filter; index already pruned)
        //   NOT SUPPORTED: BM25_FILTER = "and embed_eligible = true"
        const BM25_FILTER: &str = "and embed_eligible = true";

        let scored: Vec<(String, f64)> = {
            let conn = self.conn()?;
            let sql = format!(
                "with scored as (
                    select message_block_id,
                           fts_main_conversation_messages.match_bm25(message_block_id, ?1, conjunctive := 0) as bm25
                    from conversation_messages
                    where tenant = ?2
                      {BM25_FILTER}
                )
                 select message_block_id, bm25
                 from scored
                 where bm25 is not null
                 order by bm25 desc
                 limit ?3"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(duckdb::params![query, tenant, k as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        if scored.is_empty() {
            return Ok(vec![]);
        }

        let id_strings: Vec<String> = scored.iter().map(|(id, _)| id.clone()).collect();
        let mut hydrated = self
            .fetch_conversation_messages_by_ids(tenant, &id_strings)
            .await?;

        // Preserve BM25 rank order — fetch_conversation_messages_by_ids already
        // does this when given an ordered id slice, but we sort defensively.
        let rank_by_id: std::collections::HashMap<&str, usize> = scored
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.as_str(), i))
            .collect();
        hydrated.sort_by_key(|m| {
            *rank_by_id
                .get(m.message_block_id.as_str())
                .unwrap_or(&usize::MAX)
        });
        Ok(hydrated)
    }

    /// Rebuild the FTS index iff the dirty flag is set. Called from
    /// `bm25_transcript_candidates`. Cheap when clean.
    fn ensure_transcript_fts_index_fresh(
        &self,
    ) -> Result<(), super::duckdb::StorageError> {
        if !self
            .transcripts_fts_dirty
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Ok(());
        }
        let conn = self.conn()?;
        // BRANCH (Task 2 outcome):
        //   SUPPORTED:    create_fts_index with `where := 'embed_eligible = true'`
        //   NOT SUPPORTED: create_fts_index without the predicate
        const CREATE_INDEX_SQL: &str =
            "pragma create_fts_index('conversation_messages', 'message_block_id', 'content', \
             where := 'embed_eligible = true');";

        let _ = conn.execute_batch("load fts;");
        let _ = conn.execute_batch("pragma drop_fts_index('conversation_messages');");
        if let Err(e) = conn.execute_batch(CREATE_INDEX_SQL) {
            self.transcripts_fts_dirty
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(super::duckdb::StorageError::DuckDb(e));
        }
        Ok(())
    }
}
```

(Replace `BM25_FILTER` and `CREATE_INDEX_SQL` per Task 2's outcome.)

- [ ] **Step 6: Update `tests/common/mod.rs` if needed**

Confirm `tests/common::test_app_state` is reachable from `tests/transcript_recall.rs`. The new test file declares `mod common;` at the top; Cargo wires `tests/common/mod.rs` automatically. No change should be needed.

- [ ] **Step 7: Run the tests and verify they pass**

```bash
cargo test --test transcript_recall bm25_ -q
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
Expected: 2 tests pass, no warnings.

- [ ] **Step 8: Commit**

```bash
git add src/storage/duckdb.rs src/storage/transcript_repo.rs tests/transcript_recall.rs
git commit -m "feat(transcripts): bm25_transcript_candidates with lazy FTS index

Mirrors the memories BM25 pattern: dedicated transcripts_fts_dirty flag,
lazy drop-then-create on first read after a write, embed_eligible-only
coverage. Probed DuckDB FTS predicate-index support (see Task 2 outcome
in tests/transcript_recall.rs::fts_predicate_probe).

Closes 2026-05-01-transcript-recall §Storage."
```

---

## Task 4: Storage — `context_window_for_block`

**Files:**
- Modify: `src/storage/transcript_repo.rs`
- Modify: `tests/transcript_recall.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/transcript_recall.rs`:

```rust
#[tokio::test]
async fn context_window_returns_neighbors_in_same_session() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // Seed 5 text blocks in the same session, increasing line numbers.
    for i in 0..5 {
        let mut m = sample_block(
            &format!("blk-{i}"),
            &format!("content {i}"),
            BlockType::Text,
            true,
        );
        m.line_number = (i + 1) as u64;
        m.created_at = format!("000000000{:011}", i + 1); // strictly increasing
        repo.create_conversation_message(&m).await.unwrap();
    }

    let win = repo
        .context_window_for_block("local", "mb-blk-2", 1, 1, false)
        .await
        .unwrap();
    assert_eq!(win.before.len(), 1);
    assert_eq!(win.before[0].message_block_id, "mb-blk-1");
    assert_eq!(win.primary.message_block_id, "mb-blk-2");
    assert_eq!(win.after.len(), 1);
    assert_eq!(win.after[0].message_block_id, "mb-blk-3");
}

#[tokio::test]
async fn context_window_excludes_tool_blocks_by_default() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // text, tool_use, text, tool_result, text — primary at index 2 (the middle text).
    let kinds = [
        (BlockType::Text, true),
        (BlockType::ToolUse, false),
        (BlockType::Text, true),
        (BlockType::ToolResult, false),
        (BlockType::Text, true),
    ];
    for (i, (bt, eligible)) in kinds.iter().enumerate() {
        let mut m = sample_block(&format!("k{i}"), &format!("c{i}"), *bt, *eligible);
        m.line_number = (i + 1) as u64;
        m.created_at = format!("000000000{:011}", i + 1);
        repo.create_conversation_message(&m).await.unwrap();
    }

    // include_tool_blocks=false → before/after should skip the tool blocks.
    let win = repo
        .context_window_for_block("local", "mb-k2", 2, 2, false)
        .await
        .unwrap();
    let before_ids: Vec<&str> = win.before.iter().map(|m| m.message_block_id.as_str()).collect();
    let after_ids: Vec<&str> = win.after.iter().map(|m| m.message_block_id.as_str()).collect();
    assert_eq!(before_ids, vec!["mb-k0"]);  // mb-k1 (tool_use) skipped
    assert_eq!(after_ids, vec!["mb-k4"]);   // mb-k3 (tool_result) skipped

    // include_tool_blocks=true → all neighbors returned.
    let win = repo
        .context_window_for_block("local", "mb-k2", 2, 2, true)
        .await
        .unwrap();
    let before_ids: Vec<&str> = win.before.iter().map(|m| m.message_block_id.as_str()).collect();
    let after_ids: Vec<&str> = win.after.iter().map(|m| m.message_block_id.as_str()).collect();
    assert_eq!(before_ids, vec!["mb-k0", "mb-k1"]);
    assert_eq!(after_ids, vec!["mb-k3", "mb-k4"]);
}

#[tokio::test]
async fn context_window_does_not_cross_session_boundary() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // Two sessions interleaved by created_at; window for session A primary
    // must not include session B blocks even if they're temporally adjacent.
    let mut a1 = sample_block("a1", "session A first", BlockType::Text, true);
    a1.session_id = Some("A".to_string());
    a1.line_number = 1;
    a1.created_at = "00000000010000000001".to_string();

    let mut b1 = sample_block("b1", "session B first", BlockType::Text, true);
    b1.session_id = Some("B".to_string());
    b1.line_number = 1;
    b1.created_at = "00000000010000000002".to_string();

    let mut a2 = sample_block("a2", "session A second", BlockType::Text, true);
    a2.session_id = Some("A".to_string());
    a2.line_number = 2;
    a2.created_at = "00000000010000000003".to_string();

    repo.create_conversation_message(&a1).await.unwrap();
    repo.create_conversation_message(&b1).await.unwrap();
    repo.create_conversation_message(&a2).await.unwrap();

    let win = repo
        .context_window_for_block("local", "mb-a1", 1, 1, false)
        .await
        .unwrap();
    assert_eq!(win.before.len(), 0);
    assert_eq!(win.after.len(), 1);
    assert_eq!(win.after[0].message_block_id, "mb-a2");
}
```

- [ ] **Step 2: Run the failing tests**

```bash
cargo test --test transcript_recall context_window_ -q
```
Expected: FAIL — `context_window_for_block` not defined.

- [ ] **Step 3: Implement `context_window_for_block`**

First add the result struct at the top of `src/storage/transcript_repo.rs`:

```rust
/// Result of `context_window_for_block`. The `primary` is the requested block;
/// `before` and `after` are temporally adjacent same-session blocks (filtered
/// per `include_tool_blocks`).
#[derive(Debug, Clone)]
pub struct ContextWindow {
    pub primary: crate::domain::ConversationMessage,
    pub before: Vec<crate::domain::ConversationMessage>,
    pub after: Vec<crate::domain::ConversationMessage>,
}
```

Then add the method:

```rust
impl DuckDbRepository {
    /// Fetch the primary block and up to `k_before` / `k_after` adjacent blocks
    /// in the same `session_id`, ordered by `created_at, line_number, block_index`.
    /// If `include_tool_blocks` is false (default), `before` and `after` only
    /// include `text` and `thinking` block types (the primary itself is always
    /// returned regardless of its type).
    ///
    /// Returns `Ok` with empty before/after if the primary has no `session_id`
    /// (NULL session). Returns `StorageError::InvalidInput` if the primary id
    /// doesn't exist.
    pub async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, super::duckdb::StorageError> {
        let conn = self.conn()?;

        // 1. Fetch the primary first to get its session and timestamp.
        let primary: crate::domain::ConversationMessage = {
            let mut stmt = conn.prepare(
                "select message_block_id, session_id, tenant, caller_agent, transcript_path, \
                        line_number, block_index, message_uuid, role, block_type, content, \
                        tool_name, tool_use_id, embed_eligible, created_at \
                 from conversation_messages \
                 where tenant = ?1 and message_block_id = ?2",
            )?;
            let mut rows = stmt
                .query_map(duckdb::params![tenant, primary_id], row_to_conversation_message)?;
            match rows.next() {
                Some(Ok(m)) => m,
                Some(Err(e)) => return Err(super::duckdb::StorageError::from(e)),
                None => {
                    return Err(super::duckdb::StorageError::InvalidInput(format!(
                        "primary block not found: {primary_id}"
                    )));
                }
            }
        };

        let session_id = match primary.session_id.as_deref() {
            Some(s) => s.to_string(),
            None => {
                // No session → no neighbors by definition.
                return Ok(ContextWindow {
                    primary,
                    before: vec![],
                    after: vec![],
                });
            }
        };

        // 2. Build the "type filter" clause once based on include_tool_blocks.
        let type_filter = if include_tool_blocks {
            ""
        } else {
            "and block_type in ('text', 'thinking')"
        };

        // 3. Fetch `k_before` blocks strictly before the primary's (created_at, line, block_idx).
        let before_sql = format!(
            "select message_block_id, session_id, tenant, caller_agent, transcript_path, \
                    line_number, block_index, message_uuid, role, block_type, content, \
                    tool_name, tool_use_id, embed_eligible, created_at \
             from conversation_messages \
             where tenant = ?1 \
               and session_id = ?2 \
               and (created_at, line_number, block_index) < (?3, ?4, ?5) \
               {type_filter} \
             order by created_at desc, line_number desc, block_index desc \
             limit ?6"
        );
        let before: Vec<crate::domain::ConversationMessage> = {
            let mut stmt = conn.prepare(&before_sql)?;
            let rows = stmt.query_map(
                duckdb::params![
                    tenant,
                    session_id,
                    primary.created_at,
                    primary.line_number as i64,
                    primary.block_index as i64,
                    k_before as i64,
                ],
                row_to_conversation_message,
            )?;
            // The query returns DESC order; reverse to ASC.
            let mut v: Vec<_> = rows.collect::<Result<Vec<_>, _>>()?;
            v.reverse();
            v
        };

        // 4. Fetch `k_after` blocks strictly after.
        let after_sql = format!(
            "select message_block_id, session_id, tenant, caller_agent, transcript_path, \
                    line_number, block_index, message_uuid, role, block_type, content, \
                    tool_name, tool_use_id, embed_eligible, created_at \
             from conversation_messages \
             where tenant = ?1 \
               and session_id = ?2 \
               and (created_at, line_number, block_index) > (?3, ?4, ?5) \
               {type_filter} \
             order by created_at asc, line_number asc, block_index asc \
             limit ?6"
        );
        let after: Vec<crate::domain::ConversationMessage> = {
            let mut stmt = conn.prepare(&after_sql)?;
            let rows = stmt.query_map(
                duckdb::params![
                    tenant,
                    session_id,
                    primary.created_at,
                    primary.line_number as i64,
                    primary.block_index as i64,
                    k_after as i64,
                ],
                row_to_conversation_message,
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        Ok(ContextWindow {
            primary,
            before,
            after,
        })
    }
}
```

`row_to_conversation_message` is the existing private fn in `transcript_repo.rs`; reuse it.

DuckDB supports tuple-comparison `(a, b, c) < (?, ?, ?)` for lexicographic ordering. If for some reason it doesn't (the implementer can verify with a small probe inside Task 4), the fallback is to compare timestamps as the primary key and accept that two blocks with identical `created_at` but different line_number will sort by line_number — write the equivalent disjunction:
```sql
((created_at < ?3) OR (created_at = ?3 AND line_number < ?4) OR (created_at = ?3 AND line_number = ?4 AND block_index < ?5))
```

- [ ] **Step 4: Re-export `ContextWindow` from `storage/mod.rs`**

In `src/storage/mod.rs`, find the existing `pub use transcript_repo::...` line and add:

```rust
pub use transcript_repo::{ClaimedTranscriptEmbeddingJob, ContextWindow};
```

(or wherever the existing transcript re-exports live; match the established pattern).

- [ ] **Step 5: Run the tests and verify they pass**

```bash
cargo test --test transcript_recall context_window_ -q
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add src/storage/transcript_repo.rs src/storage/mod.rs tests/transcript_recall.rs
git commit -m "feat(transcripts): context_window_for_block hydration helper

Returns ±k same-session neighbors of a primary block, ordered by
(created_at, line_number, block_index). Default filter excludes
tool_use/tool_result from context (the primary block itself is
returned regardless of type)."
```

---

## Task 5: Pipeline — `transcript_recall::score_candidates`

**Files:**
- Create: `src/pipeline/transcript_recall.rs`
- Modify: `src/pipeline/mod.rs`

- [ ] **Step 1: Create the file with constants, types, and unit tests written FIRST (TDD)**

```rust
//! Transcript candidate scoring and window assembly. Separate from
//! pipeline/retrieve.rs (memories scoring) — zero shared state. Shares
//! only the pure helpers in pipeline/ranking.rs.

use std::collections::HashMap;

use crate::domain::ConversationMessage;
use crate::pipeline::ranking::{freshness_score, rrf_contribution, timestamp_score};

// ── Scoring magnitude (tunable; documented next to constants).

/// Per-sibling boost when a candidate shares its `session_id` with another
/// candidate in the same scoring batch. Encourages multi-block matches from
/// a single conversation to surface together.
pub const SESSION_COOCC_PER_SIBLING: i64 = 3;

/// Cap on co-occurrence siblings counted (avoids runaway on long sessions
/// that happen to be massively over-represented in candidates).
pub const SESSION_COOCC_CAP_SIBLINGS: i64 = 4;

/// Bonus applied to candidates whose `session_id` matches the caller-supplied
/// `anchor_session_id`. Set above the rank-1 RRF satwurk (~16) so an explicit
/// anchor reliably bumps moderate matches up; *never* high enough to flood
/// irrelevant blocks above strong topical matches (rank-1 RRF×2 = ~32).
///
/// Magnitude invariant guarded by `magnitude_anchor_dominates_cooccurrence`
/// test below.
pub const ANCHOR_SESSION_BONUS: i64 = 20;

/// Optional per-call options.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScoringOpts<'a> {
    pub anchor_session_id: Option<&'a str>,
}

/// One candidate annotated with its final composite score.
#[derive(Debug, Clone)]
pub struct ScoredBlock {
    pub message: ConversationMessage,
    pub score: i64,
}

/// Score the given candidate set. Pure function: no I/O, no allocation
/// beyond the result `Vec`.
///
/// Final score = `rrf_contribution(lex_rank) + rrf_contribution(sem_rank)`
///              `+ session_co_occurrence_bonus(this, all)`
///              `+ anchor_session_bonus(this.session_id, opts.anchor)`
///              `+ freshness_score(newest_in_pool, this.created_at)`.
///
/// Returned vector is sorted by score descending.
pub fn score_candidates(
    candidates: Vec<ConversationMessage>,
    lexical_ranks: &HashMap<String, usize>,
    semantic_ranks: &HashMap<String, usize>,
    opts: ScoringOpts<'_>,
) -> Vec<ScoredBlock> {
    if candidates.is_empty() {
        return vec![];
    }

    // Pre-compute newest timestamp for the freshness curve.
    let newest = candidates
        .iter()
        .map(|m| timestamp_score(&m.created_at))
        .max()
        .unwrap_or(0);

    // Pre-compute session sibling counts. Counts include self so we subtract
    // 1 below (siblings = others in same session).
    let mut session_counts: HashMap<&str, i64> = HashMap::new();
    for m in &candidates {
        if let Some(sid) = m.session_id.as_deref() {
            *session_counts.entry(sid).or_insert(0) += 1;
        }
    }

    let mut scored: Vec<ScoredBlock> = candidates
        .into_iter()
        .map(|m| {
            let mut s: i64 = 0;

            // RRF (lex + sem).
            s += lexical_ranks
                .get(&m.message_block_id)
                .map(|&r| rrf_contribution(r))
                .unwrap_or(0);
            s += semantic_ranks
                .get(&m.message_block_id)
                .map(|&r| rrf_contribution(r))
                .unwrap_or(0);

            // Session co-occurrence.
            if let Some(sid) = m.session_id.as_deref() {
                let total = *session_counts.get(sid).unwrap_or(&0);
                let siblings = (total - 1).max(0).min(SESSION_COOCC_CAP_SIBLINGS);
                s += SESSION_COOCC_PER_SIBLING * siblings;
            }

            // Anchor session boost.
            if let (Some(anchor), Some(sid)) = (opts.anchor_session_id, m.session_id.as_deref()) {
                if anchor == sid {
                    s += ANCHOR_SESSION_BONUS;
                }
            }

            // Freshness curve.
            let ts = timestamp_score(&m.created_at);
            s += freshness_score(newest, ts);

            ScoredBlock { message: m, score: s }
        })
        .collect();

    scored.sort_by(|a, b| b.score.cmp(&a.score));
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BlockType, MessageRole};

    fn sample(suffix: &str, session: Option<&str>, created: &str) -> ConversationMessage {
        ConversationMessage {
            message_block_id: format!("mb-{suffix}"),
            session_id: session.map(String::from),
            tenant: "local".to_string(),
            caller_agent: "claude-code".to_string(),
            transcript_path: "/tmp/t.jsonl".to_string(),
            line_number: 1,
            block_index: 0,
            message_uuid: None,
            role: MessageRole::Assistant,
            block_type: BlockType::Text,
            content: format!("c-{suffix}"),
            tool_name: None,
            tool_use_id: None,
            embed_eligible: true,
            created_at: created.to_string(),
        }
    }

    #[test]
    fn rrf_only_no_session_no_anchor() {
        // Single candidate, lex rank 1 only.
        let m = sample("a", None, "00000000020260430000");
        let mut lex = HashMap::new();
        lex.insert("mb-a".to_string(), 1);
        let scored = score_candidates(vec![m], &lex, &HashMap::new(), ScoringOpts::default());
        assert_eq!(scored.len(), 1);
        // Expected: rrf(1) [16] + rrf(absent) [0] + cooccurrence [0; no session] + anchor [0] + freshness [6]
        assert_eq!(scored[0].score, 16 + 6);
    }

    #[test]
    fn session_cooccurrence_caps_at_4() {
        // 6 candidates in same session. Each should see up to 4 siblings (cap), not 5.
        let candidates: Vec<_> = (0..6)
            .map(|i| sample(&format!("s{i}"), Some("S"), "00000000020260430000"))
            .collect();
        let scored = score_candidates(
            candidates,
            &HashMap::new(),
            &HashMap::new(),
            ScoringOpts::default(),
        );
        // No RRF, but each gets cap*per_sibling = 4*3 = 12 + freshness (all equal → 6)
        for sb in &scored {
            assert_eq!(sb.score, 12 + 6, "co-occ should cap at 4 siblings");
        }
    }

    #[test]
    fn anchor_session_boost_applies_only_when_match() {
        let a1 = sample("a1", Some("A"), "00000000020260430000");
        let b1 = sample("b1", Some("B"), "00000000020260430000");
        let opts = ScoringOpts {
            anchor_session_id: Some("A"),
        };
        let scored = score_candidates(vec![a1, b1], &HashMap::new(), &HashMap::new(), opts);
        // a1 gets anchor bonus; b1 does not. Both have 0 co-occ (only 1 in their session).
        let a_score = scored.iter().find(|s| s.message.message_block_id == "mb-a1").unwrap().score;
        let b_score = scored.iter().find(|s| s.message.message_block_id == "mb-b1").unwrap().score;
        assert_eq!(a_score, 20 + 6); // anchor + freshness
        assert_eq!(b_score, 6);       // freshness only
    }

    #[test]
    fn magnitude_anchor_dominates_cooccurrence() {
        // Invariant: ANCHOR_SESSION_BONUS > SESSION_COOCC_PER_SIBLING * SESSION_COOCC_CAP_SIBLINGS
        // (i.e., a single anchor hit outweighs the maximum co-occurrence boost).
        // Changing constants without updating each other will fail this test.
        assert!(
            ANCHOR_SESSION_BONUS > SESSION_COOCC_PER_SIBLING * SESSION_COOCC_CAP_SIBLINGS,
            "ANCHOR_SESSION_BONUS ({}) must exceed max co-occ ({}*{})",
            ANCHOR_SESSION_BONUS,
            SESSION_COOCC_PER_SIBLING,
            SESSION_COOCC_CAP_SIBLINGS,
        );
    }

    #[test]
    fn freshness_decays_old_below_new_at_equal_rrf() {
        // Two candidates with identical lex rank; older one scores lower.
        let new = sample("new", None, "00000000020260430000");
        let mut old = sample("old", None, "00000000020260420000"); // 10 buckets earlier
        old.line_number = 2;
        let mut lex = HashMap::new();
        lex.insert("mb-new".to_string(), 1);
        lex.insert("mb-old".to_string(), 1);
        let scored = score_candidates(vec![new, old], &lex, &HashMap::new(), ScoringOpts::default());
        // Both have rrf 16; new gets freshness 6, old gets less.
        let new_s = scored.iter().find(|s| s.message.message_block_id == "mb-new").unwrap().score;
        let old_s = scored.iter().find(|s| s.message.message_block_id == "mb-old").unwrap().score;
        assert!(new_s > old_s, "newer candidate must outrank older at equal RRF");
    }
}
```

- [ ] **Step 2: Register the module in `src/pipeline/mod.rs`**

Add:

```rust
pub mod transcript_recall;
```

- [ ] **Step 3: Run the unit tests**

```bash
cargo test --lib pipeline::transcript_recall::tests -q
cargo clippy --all-targets -- -D warnings
```
Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/pipeline/transcript_recall.rs src/pipeline/mod.rs
git commit -m "feat(transcripts): score_candidates pure scorer

RRF (BM25 + semantic) + session co-occurrence + anchor session bonus
+ freshness curve. Pure function; reuses pipeline/ranking helpers.
Magnitude invariants pinned by unit tests."
```

---

## Task 6: Pipeline — `transcript_recall::merge_windows`

**Files:**
- Modify: `src/pipeline/transcript_recall.rs`

- [ ] **Step 1: Append types and the function with TDD-style tests**

Append to `src/pipeline/transcript_recall.rs`:

```rust
// ── Window assembly types

/// A primary hit with its hydrated context neighbors.
#[derive(Debug, Clone)]
pub struct PrimaryWithContext {
    pub primary: ScoredBlock,
    pub before: Vec<ConversationMessage>,
    pub after: Vec<ConversationMessage>,
}

/// Output of the window-merge phase: one or more primaries surrounded by
/// shared context, grouped per window. `score = max(primary_scores)`.
#[derive(Debug, Clone)]
pub struct MergedWindow {
    pub session_id: Option<String>,
    pub blocks: Vec<ConversationMessage>, // chronological, dedup'd
    pub primary_ids: Vec<String>,         // chronological by primary timestamp
    pub primary_scores: HashMap<String, i64>,
    pub score: i64,
}

/// Merge `PrimaryWithContext` items into windows. Two primaries belong in the
/// same window iff they share `session_id` AND their `(before, primary, after)`
/// timestamp ranges overlap or touch.
///
/// Output windows are sorted by `score` descending.
pub fn merge_windows(items: Vec<PrimaryWithContext>) -> Vec<MergedWindow> {
    if items.is_empty() {
        return vec![];
    }

    // Bucket by session_id. NULL session = its own bucket per primary
    // (no merging across NULL).
    let mut by_session: HashMap<Option<String>, Vec<PrimaryWithContext>> = HashMap::new();
    for item in items {
        let key = item.primary.message.session_id.clone();
        by_session.entry(key).or_default().push(item);
    }

    let mut windows: Vec<MergedWindow> = Vec::new();
    for (session, mut group) in by_session {
        if session.is_none() {
            // No merging for NULL sessions.
            for item in group {
                windows.push(single_window(None, item));
            }
            continue;
        }

        // Sort the group by primary timestamp for left-to-right merging.
        group.sort_by(|a, b| {
            timestamp_score(&a.primary.message.created_at)
                .cmp(&timestamp_score(&b.primary.message.created_at))
        });

        // Sweep: maintain a "current" merged window; if the next item's
        // before-range starts at or before the current's after-range end, merge.
        let mut current: Option<MergedWindow> = None;
        for item in group {
            let item_window = single_window(session.clone(), item);

            match current.take() {
                None => current = Some(item_window),
                Some(existing) => {
                    if windows_overlap(&existing, &item_window) {
                        current = Some(merge_two(existing, item_window));
                    } else {
                        windows.push(existing);
                        current = Some(item_window);
                    }
                }
            }
        }
        if let Some(w) = current {
            windows.push(w);
        }
    }

    windows.sort_by(|a, b| b.score.cmp(&a.score));
    windows
}

fn single_window(session: Option<String>, item: PrimaryWithContext) -> MergedWindow {
    let mut blocks = item.before;
    blocks.push(item.primary.message.clone());
    blocks.extend(item.after);
    let primary_id = item.primary.message.message_block_id.clone();
    let mut scores = HashMap::new();
    scores.insert(primary_id.clone(), item.primary.score);
    MergedWindow {
        session_id: session,
        blocks,
        primary_ids: vec![primary_id],
        primary_scores: scores,
        score: item.primary.score,
    }
}

fn windows_overlap(a: &MergedWindow, b: &MergedWindow) -> bool {
    // Both windows are time-sorted; compare the last block of `a` to the
    // first block of `b`. Overlap = `b`'s first ts <= `a`'s last ts.
    let a_last = a.blocks.last().expect("non-empty window");
    let b_first = b.blocks.first().expect("non-empty window");
    timestamp_score(&b_first.created_at) <= timestamp_score(&a_last.created_at)
}

fn merge_two(a: MergedWindow, b: MergedWindow) -> MergedWindow {
    // Merge block lists, dedup by message_block_id, sort by (created_at, line_number, block_index).
    let mut all_blocks = a.blocks;
    all_blocks.extend(b.blocks);
    all_blocks.sort_by(|x, y| {
        let tx = timestamp_score(&x.created_at);
        let ty = timestamp_score(&y.created_at);
        tx.cmp(&ty)
            .then(x.line_number.cmp(&y.line_number))
            .then(x.block_index.cmp(&y.block_index))
    });
    all_blocks.dedup_by(|x, y| x.message_block_id == y.message_block_id);

    let mut primary_ids = a.primary_ids;
    primary_ids.extend(b.primary_ids);
    let mut primary_scores = a.primary_scores;
    primary_scores.extend(b.primary_scores);
    let score = primary_scores.values().copied().max().unwrap_or(0);

    MergedWindow {
        session_id: a.session_id,
        blocks: all_blocks,
        primary_ids,
        primary_scores,
        score,
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::domain::{BlockType, MessageRole};

    fn block(suffix: &str, session: &str, created: &str, line: u64) -> ConversationMessage {
        ConversationMessage {
            message_block_id: format!("mb-{suffix}"),
            session_id: Some(session.to_string()),
            tenant: "local".to_string(),
            caller_agent: "claude-code".to_string(),
            transcript_path: "/tmp/t.jsonl".to_string(),
            line_number: line,
            block_index: 0,
            message_uuid: None,
            role: MessageRole::Assistant,
            block_type: BlockType::Text,
            content: format!("c-{suffix}"),
            tool_name: None,
            tool_use_id: None,
            embed_eligible: true,
            created_at: created.to_string(),
        }
    }

    fn pwc(primary_suffix: &str, session: &str, created: &str, line: u64, score: i64) -> PrimaryWithContext {
        PrimaryWithContext {
            primary: ScoredBlock {
                message: block(primary_suffix, session, created, line),
                score,
            },
            before: vec![],
            after: vec![],
        }
    }

    #[test]
    fn single_primary_no_overlap_one_window() {
        let item = pwc("p1", "S1", "00000000020260430000", 5, 30);
        let windows = merge_windows(vec![item]);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].primary_ids, vec!["mb-p1"]);
        assert_eq!(windows[0].score, 30);
        assert_eq!(windows[0].blocks.len(), 1);
    }

    #[test]
    fn two_primaries_same_session_overlapping_merge() {
        let mut a = pwc("a", "S1", "00000000020260430010", 5, 30);
        let mut b = pwc("b", "S1", "00000000020260430011", 6, 25);
        // Make their context ranges overlap: a's `after` includes b, b's `before` includes a.
        a.after = vec![block("b", "S1", "00000000020260430011", 6)];
        b.before = vec![block("a", "S1", "00000000020260430010", 5)];
        let windows = merge_windows(vec![a, b]);
        assert_eq!(windows.len(), 1, "overlapping primaries should merge");
        let mut ids = windows[0].primary_ids.clone();
        ids.sort();
        assert_eq!(ids, vec!["mb-a", "mb-b"]);
        assert_eq!(windows[0].score, 30, "merged score = max(primary scores)");
    }

    #[test]
    fn two_primaries_different_session_dont_merge() {
        let a = pwc("a", "S1", "00000000020260430010", 5, 30);
        let b = pwc("b", "S2", "00000000020260430011", 6, 25);
        let windows = merge_windows(vec![a, b]);
        assert_eq!(windows.len(), 2);
    }

    #[test]
    fn two_primaries_same_session_far_apart_dont_merge() {
        // Both in S1 but with no temporal overlap in their before/after ranges.
        let a = pwc("a", "S1", "00000000020260430010", 1, 30); // no after
        let b = pwc("b", "S1", "00000000020260430999", 2, 25); // no before
        let windows = merge_windows(vec![a, b]);
        assert_eq!(windows.len(), 2);
    }

    #[test]
    fn merged_window_blocks_dedup_and_time_sorted() {
        // Two primaries' contexts share one block ("shared").
        let a = PrimaryWithContext {
            primary: ScoredBlock {
                message: block("a", "S1", "00000000020260430010", 1),
                score: 30,
            },
            before: vec![],
            after: vec![block("shared", "S1", "00000000020260430011", 2)],
        };
        let b = PrimaryWithContext {
            primary: ScoredBlock {
                message: block("b", "S1", "00000000020260430012", 3),
                score: 25,
            },
            before: vec![block("shared", "S1", "00000000020260430011", 2)],
            after: vec![],
        };
        let windows = merge_windows(vec![a, b]);
        assert_eq!(windows.len(), 1);
        let ids: Vec<&str> = windows[0].blocks.iter().map(|b| b.message_block_id.as_str()).collect();
        assert_eq!(ids, vec!["mb-a", "mb-shared", "mb-b"], "dedup'd and time-sorted");
    }

    #[test]
    fn windows_sorted_by_score_descending() {
        let low = pwc("low", "S1", "00000000020260430010", 1, 10);
        let high = pwc("high", "S2", "00000000020260430011", 1, 50);
        let windows = merge_windows(vec![low, high]);
        assert_eq!(windows[0].primary_ids, vec!["mb-high"]);
        assert_eq!(windows[1].primary_ids, vec!["mb-low"]);
    }
}
```

- [ ] **Step 2: Run the tests**

```bash
cargo test --lib pipeline::transcript_recall -q
cargo clippy --all-targets -- -D warnings
```
Expected: ~11 tests pass (5 from Task 5 + 6 new).

- [ ] **Step 3: Commit**

```bash
git add src/pipeline/transcript_recall.rs
git commit -m "feat(transcripts): merge_windows window-assembly algorithm

Same-session primaries with overlapping (before, primary, after) ranges
merge into one window; primary_ids accumulate; score = max(primary_scores).
NULL-session primaries each get their own window (no cross-NULL merging).
Output sorted by score desc."
```

---

## Task 7: Service — three-channel candidate pool + scoring + window assembly

**Files:**
- Modify: `src/service/transcript_service.rs`
- (no test file modifications yet — Tasks 8+ wire the new shape into HTTP and tests)

- [ ] **Step 1: Read the current `TranscriptService::search` to understand what's being replaced**

Open `src/service/transcript_service.rs`. The current `search` returns `Vec<TranscriptSearchHit>`. We're replacing the return type (this commit ALSO breaks `http/transcripts.rs::post_search`; Task 8 fixes the HTTP layer).

Strategy: this commit will leave `http/transcripts.rs` momentarily broken — it's a single short commit followed immediately by Task 8. The branch is on a worktree; trunk doesn't see broken intermediate state.

Alternatively, do this task and Task 8 as a single commit. **Decision: combine Tasks 7 and 8 into one commit** to avoid the broken-intermediate state. The plan keeps them as separate tasks for clarity but the commit lands after Task 8.

- [ ] **Step 2: Replace `TranscriptSearchHit` with the new return shape**

Replace the existing `TranscriptSearchHit` struct (and the `search` method's return type) with the new types. Keep `TranscriptSearchFilters` as-is.

```rust
//! Transcript-archive service façade.
//! ... (existing module doc) ...

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::embedding::EmbeddingProvider;
use crate::pipeline::transcript_recall::{
    merge_windows, score_candidates, MergedWindow, PrimaryWithContext, ScoringOpts,
};
use crate::storage::{ContextWindow, DuckDbRepository, StorageError, VectorIndex};

#[derive(Debug, Clone, Default)]
pub struct TranscriptSearchFilters {
    pub session_id: Option<String>,
    pub role: Option<MessageRole>,
    pub block_type: Option<BlockType>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
}

/// Optional, request-scoped recall tuning.
#[derive(Debug, Clone, Default)]
pub struct TranscriptSearchOpts {
    pub anchor_session_id: Option<String>,
    /// ±N blocks of context around each primary. None → 2 (default).
    /// Capped at 10 by the service.
    pub context_window: Option<usize>,
    pub include_tool_blocks_in_context: bool,
}

/// Result of `TranscriptService::search` — a list of merged conversation
/// windows, each containing one or more primary hits and their context.
#[derive(Debug, Clone)]
pub struct TranscriptSearchResult {
    pub windows: Vec<MergedWindow>,
}

#[derive(Clone)]
pub struct TranscriptService {
    repo: DuckDbRepository,
    index: Arc<VectorIndex>,
    provider: Option<Arc<dyn EmbeddingProvider>>,
}

impl TranscriptService {
    pub fn new(
        repo: DuckDbRepository,
        index: Arc<VectorIndex>,
        provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        Self { repo, index, provider }
    }

    pub async fn ingest(&self, msg: ConversationMessage) -> Result<(), StorageError> {
        self.repo.create_conversation_message(&msg).await
    }

    pub async fn get_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.repo
            .get_conversation_messages_by_session(tenant, session_id)
            .await
    }

    /// Three-channel hybrid recall:
    ///   - HNSW (semantic) ranks
    ///   - BM25 (lexical) ranks
    ///   - Optional anchor-session injection (no rank; bonus only)
    /// then `transcript_recall::score_candidates` + filter + hydrate + merge.
    ///
    /// Empty query path (compatibility): `recent_conversation_messages` is
    /// the candidate pool; all candidates score 0; same hydration + merge.
    pub async fn search(
        &self,
        tenant: &str,
        query: &str,
        filters: &TranscriptSearchFilters,
        limit: usize,
        opts: &TranscriptSearchOpts,
    ) -> Result<TranscriptSearchResult, StorageError> {
        // Cap limit defensively (window merge is O(N²) in primaries; N≤100
        // is trivially fast).
        let limit = limit.max(1).min(100);
        let oversample = limit * 4;
        let context_window = opts.context_window.unwrap_or(2).min(10);

        // ─── Phase 1: gather candidate ids and per-channel ranks.
        let mut lexical_ranks: HashMap<String, usize> = HashMap::new();
        let mut semantic_ranks: HashMap<String, usize> = HashMap::new();
        let mut all_ids: HashSet<String> = HashSet::new();

        if !query.trim().is_empty() {
            // BM25 channel (always available; doesn't need a provider).
            let bm25_hits = self
                .repo
                .bm25_transcript_candidates(tenant, query, oversample)
                .await?;
            for (rank0, m) in bm25_hits.iter().enumerate() {
                lexical_ranks.insert(m.message_block_id.clone(), rank0 + 1);
                all_ids.insert(m.message_block_id.clone());
            }

            // HNSW channel (only if provider attached).
            if let Some(provider) = &self.provider {
                let q_vec = provider
                    .embed_text(query)
                    .await
                    .map_err(|e| StorageError::InvalidInput(format!("query embed failed: {e}")))?;
                let sem_hits = self
                    .index
                    .search(&q_vec, oversample)
                    .await
                    .map_err(|e| StorageError::VectorIndex(e.to_string()))?;
                for (rank0, (id, _score)) in sem_hits.iter().enumerate() {
                    semantic_ranks.insert(id.clone(), rank0 + 1);
                    all_ids.insert(id.clone());
                }
            }
        } else {
            // Empty query: fall back to recent-time browse mode.
            let recent = self
                .repo
                .recent_conversation_messages(tenant, oversample)
                .await?;
            for m in recent {
                all_ids.insert(m.message_block_id.clone());
            }
            // Both rank maps stay empty → all RRF contributions = 0.
        }

        // Anchor session injection (independent of channel).
        if let Some(anchor) = opts.anchor_session_id.as_deref() {
            let injected = self
                .repo
                .anchor_session_candidates(tenant, anchor, oversample)
                .await?;
            for id in injected {
                all_ids.insert(id);
            }
        }

        if all_ids.is_empty() {
            return Ok(TranscriptSearchResult { windows: vec![] });
        }

        // ─── Phase 2: hydrate to full ConversationMessage records.
        let id_vec: Vec<String> = all_ids.into_iter().collect();
        let candidates = self
            .repo
            .fetch_conversation_messages_by_ids(tenant, &id_vec)
            .await?;

        // ─── Phase 3: score.
        let scoring_opts = ScoringOpts {
            anchor_session_id: opts.anchor_session_id.as_deref(),
        };
        let mut scored = score_candidates(candidates, &lexical_ranks, &semantic_ranks, scoring_opts);

        // ─── Phase 4: apply filters.
        scored.retain(|sb| {
            let m = &sb.message;
            filters
                .session_id
                .as_ref()
                .is_none_or(|s| m.session_id.as_deref() == Some(s.as_str()))
                && filters.role.is_none_or(|r| m.role == r)
                && filters.block_type.is_none_or(|b| m.block_type == b)
                && filters
                    .time_from
                    .as_ref()
                    .is_none_or(|t| m.created_at.as_str() >= t.as_str())
                && filters
                    .time_to
                    .as_ref()
                    .is_none_or(|t| m.created_at.as_str() <= t.as_str())
        });

        // ─── Phase 5: take top-`limit` as primaries; hydrate context for each.
        scored.truncate(limit);

        let mut items: Vec<PrimaryWithContext> = Vec::with_capacity(scored.len());
        for sb in scored {
            let cw: ContextWindow = self
                .repo
                .context_window_for_block(
                    tenant,
                    &sb.message.message_block_id,
                    context_window,
                    context_window,
                    opts.include_tool_blocks_in_context,
                )
                .await?;
            items.push(PrimaryWithContext {
                primary: sb,
                before: cw.before,
                after: cw.after,
            });
        }

        // ─── Phase 6: merge windows.
        let windows = merge_windows(items);
        Ok(TranscriptSearchResult { windows })
    }
}
```

- [ ] **Step 3: Add the `anchor_session_candidates` repo method**

In `src/storage/transcript_repo.rs`, add:

```rust
impl DuckDbRepository {
    /// Returns up to `k` `message_block_id`s from the given anchor session
    /// (most recent first). Used by `TranscriptService::search` to ensure
    /// anchor-session blocks enter the candidate pool even if no topical match.
    pub async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, super::duckdb::StorageError> {
        if k == 0 {
            return Ok(vec![]);
        }
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select message_block_id \
             from conversation_messages \
             where tenant = ?1 and session_id = ?2 and embed_eligible = true \
             order by created_at desc \
             limit ?3",
        )?;
        let rows = stmt.query_map(duckdb::params![tenant, session_id, k as i64], |r| {
            r.get::<_, String>(0)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}
```

- [ ] **Step 4: Don't run tests yet** — `http/transcripts.rs` still references the old `TranscriptSearchHit`; the build is broken. Task 8 fixes this in the same commit.

- [ ] **Step 5: Don't commit yet** — combine with Task 8.

---

## Task 8: HTTP layer — new request fields, new response shape, update existing tests

**Files:**
- Modify: `src/http/transcripts.rs`
- Modify: `tests/conversation_archive.rs`
- Modify: `tests/integration_claude_code.rs`

- [ ] **Step 1: Rewrite request, response, and handler in `http/transcripts.rs`**

Find the existing `SearchRequest`, `SearchResponse`, `SearchHitDto`, `post_search` definitions and replace as below.

```rust
// ──────── Request ────────

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub tenant: String,
    pub session_id: Option<String>,
    pub role: Option<crate::domain::MessageRole>,
    pub block_type: Option<crate::domain::BlockType>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,

    // New recall-related fields
    pub anchor_session_id: Option<String>,
    pub context_window: Option<usize>,
    #[serde(default)]
    pub include_tool_blocks_in_context: bool,
}

fn default_limit() -> usize {
    20
}

// ──────── Response ────────

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub windows: Vec<TranscriptWindow>,
}

#[derive(Debug, Serialize)]
pub struct TranscriptWindow {
    pub session_id: Option<String>,
    pub blocks: Vec<TranscriptWindowBlock>,
    pub primary_ids: Vec<String>,
    pub score: i64,
}

#[derive(Debug, Serialize)]
pub struct TranscriptWindowBlock {
    pub message_block_id: String,
    pub session_id: Option<String>,
    pub line_number: u64,
    pub block_index: u32,
    pub role: crate::domain::MessageRole,
    pub block_type: crate::domain::BlockType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub created_at: String,
    pub is_primary: bool,
    pub primary_score: Option<i64>,
}

// ──────── Handler ────────

async fn post_search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, AppError> {
    let filters = TranscriptSearchFilters {
        session_id: req.session_id,
        role: req.role,
        block_type: req.block_type,
        time_from: req.time_from,
        time_to: req.time_to,
    };
    let opts = TranscriptSearchOpts {
        anchor_session_id: req.anchor_session_id,
        context_window: req.context_window,
        include_tool_blocks_in_context: req.include_tool_blocks_in_context,
    };

    let result = state
        .transcript_service
        .search(&req.tenant, &req.query, &filters, req.limit, &opts)
        .await?;

    let windows = result
        .windows
        .into_iter()
        .map(window_to_dto)
        .collect();
    Ok(Json(SearchResponse { windows }))
}

fn window_to_dto(w: crate::pipeline::transcript_recall::MergedWindow) -> TranscriptWindow {
    let primary_set: std::collections::HashSet<&str> =
        w.primary_ids.iter().map(String::as_str).collect();
    let blocks: Vec<TranscriptWindowBlock> = w
        .blocks
        .into_iter()
        .map(|m| {
            let id = m.message_block_id.clone();
            let is_primary = primary_set.contains(id.as_str());
            let primary_score = if is_primary {
                w.primary_scores.get(&id).copied()
            } else {
                None
            };
            TranscriptWindowBlock {
                message_block_id: id,
                session_id: m.session_id,
                line_number: m.line_number,
                block_index: m.block_index,
                role: m.role,
                block_type: m.block_type,
                content: m.content,
                tool_name: m.tool_name,
                tool_use_id: m.tool_use_id,
                created_at: m.created_at,
                is_primary,
                primary_score,
            }
        })
        .collect();
    TranscriptWindow {
        session_id: w.session_id,
        blocks,
        primary_ids: w.primary_ids,
        score: w.score,
    }
}
```

Add the necessary `use` statements at the top of the file:
```rust
use crate::service::transcript_service::{TranscriptSearchFilters, TranscriptSearchOpts};
```

- [ ] **Step 2: Update existing test `post_transcripts_search_filters_by_role_and_block_type` in `tests/conversation_archive.rs`**

Find the test. Its current assertions inspect `hits` array. Migrate to:

```rust
#[tokio::test]
async fn post_transcripts_search_filters_by_role_and_block_type() {
    let dir = TempDir::new().unwrap();
    let app = build_test_app(&dir).await;

    // Seed mixed-role/block_type messages.
    for (i, (role, block_type)) in [
        ("user", "text"),
        ("assistant", "text"),
        ("assistant", "tool_use"),
    ]
    .iter()
    .enumerate()
    {
        let body = serde_json::json!({
            "session_id": "S",
            "tenant": "local",
            "caller_agent": "claude-code",
            "transcript_path": "/tmp/t.jsonl",
            "line_number": i + 1,
            "block_index": 0,
            "role": role,
            "block_type": block_type,
            "content": format!("c-{i}"),
            "embed_eligible": *block_type == "text",
            "created_at": format!("2026-04-30T00:00:0{i}Z")
        });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/transcripts/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Filter by role=user.
    let body = serde_json::json!({
        "query": "",
        "tenant": "local",
        "role": "user",
        "limit": 5
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcripts/search")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let windows = v["windows"].as_array().expect("windows array");
    assert_eq!(windows.len(), 1, "exactly one user-role primary");
    let primaries: Vec<&str> = windows[0]["primary_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(primaries.len(), 1);
    let primary_block = windows[0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["is_primary"].as_bool() == Some(true))
        .unwrap();
    assert_eq!(primary_block["role"].as_str(), Some("user"));

    // Filter by block_type=tool_use.
    let body = serde_json::json!({
        "query": "",
        "tenant": "local",
        "block_type": "tool_use",
        "limit": 5
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcripts/search")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let windows = v["windows"].as_array().expect("windows array");
    assert_eq!(windows.len(), 1);
    let primary_block = windows[0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["is_primary"].as_bool() == Some(true))
        .unwrap();
    assert_eq!(primary_block["block_type"].as_str(), Some("tool_use"));
}
```

- [ ] **Step 3: Update `tests/integration_claude_code.rs::end_to_end_mine_then_search_then_get`**

Find the assertions on `v["hits"]` and migrate:

```rust
// Replace:
let hits = v["hits"].as_array().unwrap();
assert!(!hits.is_empty(), "expected at least one semantic hit");

// With:
let windows = v["windows"].as_array().unwrap();
assert!(!windows.is_empty(), "expected at least one window");
// Verify primary_ids non-empty in the top window.
let primaries = windows[0]["primary_ids"].as_array().unwrap();
assert!(!primaries.is_empty(), "top window must have at least one primary");
```

- [ ] **Step 4: Build and run the affected suites**

```bash
cargo build --tests
cargo test --test conversation_archive --test integration_claude_code --test transcript_recall -q
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Expected: all green. The previously passing tests still pass under the new shape; the new transcript_recall integration tests still pass.

- [ ] **Step 5: Commit (combined with Task 7)**

```bash
git add src/service/transcript_service.rs src/storage/transcript_repo.rs src/http/transcripts.rs tests/conversation_archive.rs tests/integration_claude_code.rs
git commit -m "feat(transcripts): three-channel search + window response shape

TranscriptService::search now combines BM25 + HNSW + optional anchor-session
injection through pipeline::transcript_recall::score_candidates, then hydrates
each primary's ±k context and merges overlapping same-session windows.

POST /transcripts/search response shape changes: { hits: [...] } →
{ windows: [...] } with TranscriptWindow / TranscriptWindowBlock DTOs.

Two in-tree callers updated:
- tests/conversation_archive.rs::post_transcripts_search_filters_by_role_and_block_type
- tests/integration_claude_code.rs::end_to_end_mine_then_search_then_get

Closes 2026-05-01-transcript-recall §HTTP."
```

---

## Task 9: Integration tests covering the full recall path

**Files:**
- Modify: `tests/transcript_recall.rs`

- [ ] **Step 1: Append the integration tests**

Append to `tests/transcript_recall.rs`. These tests exercise the wired-up service via the in-process router (using `tests/common::test_app_state`), like `tests/conversation_archive.rs::http_routes::*` already does.

Add at the file top (alongside the existing `mod common;`):

```rust
use mem::http;

async fn build_recall_app(db_dir: &TempDir) -> axum::Router {
    use mem::config::Config;
    use mem::service::MemoryService;
    use mem::storage::DuckDbRepository;
    let mut cfg = Config::local();
    cfg.db_path = db_dir.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&cfg.db_path).await.unwrap();
    repo.set_transcript_job_provider("embedanything");
    let memory_service = MemoryService::new(repo.clone());
    let state = common::test_app_state(repo, memory_service);
    http::router().with_state(state)
}
```

(If `common::test_app_state` has a different signature, follow what `conversation_archive.rs::http_routes` already does; the function exists and is the canonical way to assemble an in-test `AppState`.)

Then the tests:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

async fn ingest_block(
    app: &axum::Router,
    session: &str,
    line: u64,
    role: &str,
    block_type: &str,
    content: &str,
    embed: bool,
    created: &str,
) {
    let body = json!({
        "session_id": session,
        "tenant": "local",
        "caller_agent": "claude-code",
        "transcript_path": "/tmp/t.jsonl",
        "line_number": line,
        "block_index": 0,
        "role": role,
        "block_type": block_type,
        "content": content,
        "embed_eligible": embed,
        "created_at": created,
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcripts/messages")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn search(app: &axum::Router, body: serde_json::Value) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcripts/search")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn bm25_only_candidate_appears_in_results() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    ingest_block(&app, "S", 1, "assistant", "text", "rust project layout", true, "2026-04-30T00:00:00Z").await;
    ingest_block(&app, "S", 2, "assistant", "text", "unrelated material", true, "2026-04-30T00:00:01Z").await;

    let v = search(&app, json!({ "query": "rust", "tenant": "local", "limit": 5 })).await;
    let windows = v["windows"].as_array().unwrap();
    assert!(!windows.is_empty(), "BM25 alone should surface the rust block");
    let primary_block_id = windows[0]["primary_ids"][0].as_str().unwrap();
    assert!(primary_block_id.contains("mb-"), "primary should reference a real id");
}

#[tokio::test]
async fn anchor_session_boost_lifts_matching_session_to_top() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    // Two sessions, each with one weakly relevant block.
    ingest_block(&app, "A", 1, "assistant", "text", "scattered keyword once", true, "2026-04-30T00:00:00Z").await;
    ingest_block(&app, "B", 2, "assistant", "text", "scattered keyword once", true, "2026-04-30T00:00:01Z").await;

    let v = search(&app, json!({
        "query": "scattered",
        "tenant": "local",
        "limit": 5,
        "anchor_session_id": "A"
    })).await;
    let windows = v["windows"].as_array().unwrap();
    assert!(!windows.is_empty());
    assert_eq!(windows[0]["session_id"].as_str(), Some("A"), "anchor session must rank first");
}

#[tokio::test]
async fn context_window_includes_neighboring_text_blocks() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    for i in 0..5 {
        ingest_block(
            &app, "S", (i + 1) as u64, "assistant", "text",
            &format!("content-{i}"), true,
            &format!("2026-04-30T00:00:0{i}Z"),
        ).await;
    }

    // Search for the middle block's content; expect a window of size 5.
    let v = search(&app, json!({
        "query": "content-2",
        "tenant": "local",
        "limit": 1,
        "context_window": 2
    })).await;
    let windows = v["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    let blocks = windows[0]["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 5, "primary + 2 before + 2 after = 5");
    let primary_count = blocks.iter().filter(|b| b["is_primary"] == json!(true)).count();
    assert_eq!(primary_count, 1);
}

#[tokio::test]
async fn context_window_excludes_tool_blocks_by_default() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    let kinds = [
        ("text", true),
        ("tool_use", false),
        ("text", true),
        ("tool_result", false),
        ("text", true),
    ];
    for (i, (bt, eligible)) in kinds.iter().enumerate() {
        ingest_block(
            &app, "S", (i + 1) as u64, "assistant", bt,
            &format!("middle-distinctive-token-{i}"), *eligible,
            &format!("2026-04-30T00:00:0{i}Z"),
        ).await;
    }

    // Search hits the middle text block (index 2). Default context_window=2 + tool exclusion.
    let v = search(&app, json!({
        "query": "middle-distinctive-token-2",
        "tenant": "local",
        "limit": 1
    })).await;
    let windows = v["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    let block_types: Vec<&str> = windows[0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["block_type"].as_str().unwrap())
        .collect();
    assert!(block_types.iter().all(|bt| *bt == "text"), "context excludes tool blocks");
}

#[tokio::test]
async fn context_window_includes_tool_blocks_when_opted_in() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    let kinds = [
        ("text", true),
        ("tool_use", false),
        ("text", true),
        ("tool_result", false),
        ("text", true),
    ];
    for (i, (bt, eligible)) in kinds.iter().enumerate() {
        ingest_block(
            &app, "S", (i + 1) as u64, "assistant", bt,
            &format!("optedin-token-{i}"), *eligible,
            &format!("2026-04-30T00:00:0{i}Z"),
        ).await;
    }

    let v = search(&app, json!({
        "query": "optedin-token-2",
        "tenant": "local",
        "limit": 1,
        "context_window": 2,
        "include_tool_blocks_in_context": true
    })).await;
    let windows = v["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    let block_types: std::collections::HashSet<&str> = windows[0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["block_type"].as_str().unwrap())
        .collect();
    assert!(block_types.contains("tool_use"), "tool_use must appear when opted in");
    assert!(block_types.contains("tool_result"));
}

#[tokio::test]
async fn windows_merge_when_primaries_share_session_and_overlap() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    // Three adjacent text blocks, all with the same query keyword.
    // With context_window=1, primary 1's after overlaps primary 3's before.
    for i in 0..3 {
        ingest_block(
            &app, "S", (i + 1) as u64, "assistant", "text",
            "shared-merge-keyword", true,
            &format!("2026-04-30T00:00:0{i}Z"),
        ).await;
    }

    let v = search(&app, json!({
        "query": "shared-merge-keyword",
        "tenant": "local",
        "limit": 5,
        "context_window": 1
    })).await;
    let windows = v["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1, "all three primaries should merge into one window");
    let primary_ids = windows[0]["primary_ids"].as_array().unwrap();
    assert_eq!(primary_ids.len(), 3);
}

#[tokio::test]
async fn empty_query_returns_recent_time_windows() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    for i in 0..3 {
        ingest_block(
            &app, "S", (i + 1) as u64, "assistant", "text",
            &format!("recent-c{i}"), true,
            &format!("2026-04-30T00:00:0{i}Z"),
        ).await;
    }

    let v = search(&app, json!({ "query": "", "tenant": "local", "limit": 5 })).await;
    let windows = v["windows"].as_array().unwrap();
    assert!(!windows.is_empty(), "empty query should still return windows from recent_*");
}
```

(The HNSW-only test from the spec testing matrix is intentionally omitted from this plan — without an embedding provider attached the HNSW channel is silent, and Task 12 / spec Concerns confirms HNSW-only coverage runs through `tests/integration_claude_code.rs::end_to_end_mine_then_search_then_get` which does attach a fake provider. If the implementer wants to add HNSW-only coverage explicitly, do so via a `FakeEmbeddingProvider` injected into a dedicated test app — see `tests/integration_claude_code.rs` for the pattern.)

(The "tool_blocks_excluded_from_bm25_index" test from the spec is already in Task 3 — `bm25_excludes_tool_blocks` — so it's not duplicated here.)

(The "mem_repair_unaffected_by_fts_dirty" test from the spec is intentionally deferred to a follow-up plan — it's an isolation test and the current plan doesn't actually change `mem repair` behavior. If the implementer wants to pin the isolation, add a one-shot test that calls `repo.set_transcripts_fts_dirty()` and runs `mem::cli::repair::run_check_for_test(...)` to confirm it returns 0.)

- [ ] **Step 2: Run all tests**

```bash
cargo test --test transcript_recall -q
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
Expected: all 7 new integration tests pass, plus the 5 from earlier (probe[ignored], bm25_finds, bm25_excludes, context_window×3) — about 12 tests total in the file (including the ignored probe).

- [ ] **Step 3: Commit**

```bash
git add tests/transcript_recall.rs
git commit -m "test(transcripts): integration coverage for BM25 + anchor + context windows"
```

---

## Task 10: End-to-end smoke + verification checklist

**Files:**
- (no new tests; full-suite verification only)
- Optionally update `README.md` for the new `POST /transcripts/search` request fields and response shape.

- [ ] **Step 1: Run the full test suite**

```bash
cargo test -q --no-fail-fast 2>&1 | tail -50
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Expected: same set of pre-acknowledged failures as the conversation-archive merge (config test env-leak, EmbedAnything-runtime tests, etc.). The new tests pass. **No new red.**

- [ ] **Step 2: Manual smoke (operator pre-merge checklist; documented but not executed in CI)**

```bash
# 1. Start serve
cargo run -- serve &
SERVE_PID=$!
sleep 2

# 2. Mine a real transcript
cargo run -- mine ~/.claude/projects/<some-project>/<some-session>.jsonl --agent claude-code

# 3. Hybrid search with default options
curl -s -X POST localhost:3000/transcripts/search \
  -H 'content-type: application/json' \
  -d '{"query":"vector index", "tenant":"local", "limit":3, "context_window":2}' | jq '.windows | length'

# 4. Anchor session boost
SESSION=$(duckdb $MEM_DB_PATH "select session_id from conversation_messages where session_id is not null limit 1" -csv -noheader)
curl -s -X POST localhost:3000/transcripts/search \
  -H 'content-type: application/json' \
  -d "$(printf '{"query":"vector index", "tenant":"local", "limit":3, "anchor_session_id":"%s"}' "$SESSION")" \
  | jq ".windows[0].session_id"

# 5. Tool blocks in context
curl -s -X POST localhost:3000/transcripts/search \
  -H 'content-type: application/json' \
  -d '{"query":"vector index", "tenant":"local", "limit":3, "include_tool_blocks_in_context":true}' \
  | jq '.windows[0].blocks | map(.block_type) | unique'

# 6. Cleanup
kill $SERVE_PID
```

- [ ] **Step 3: Update `README.md` (optional but recommended)**

Find the existing `Transcript Archive (conversation_messages)` section and update the search example block:

```markdown
**Search** (BM25 + HNSW hybrid; returns merged conversation windows):
\`\`\`bash
curl -X POST localhost:3000/transcripts/search \
  -H 'content-type: application/json' \
  -d '{
    "query": "vector index",
    "tenant": "local",
    "limit": 5,
    "context_window": 2,
    "anchor_session_id": null,
    "include_tool_blocks_in_context": false
  }' | jq
\`\`\`

Response shape: \`{ "windows": [{ "session_id": "...", "blocks": [...], "primary_ids": [...], "score": 47 }] }\`. Each window is a conversation snippet around one or more primary hits; `is_primary: true` flags the actual matches inside the `blocks` array.
```

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs(transcripts): document new search request fields and window response"
```

---

## Self-Review

After writing, walk through the spec and confirm coverage:

**Spec coverage:**
- §Architecture three-channel candidates → Task 7 ✓
- §Schema (no new file; lazy build) → Task 3 ✓
- §Storage transcripts_fts_dirty + bm25 + ensure_fresh → Task 3 ✓
- §Storage context_window_for_block → Task 4 ✓
- §Ranking pipeline/ranking.rs extraction → Task 1 ✓
- §Ranking pipeline/transcript_recall.rs scorer → Task 5 ✓
- §Ranking magnitude table + invariants → Task 5 unit tests ✓
- §Ranking window assembly → Task 6 ✓
- §Service three-channel + filter + hydrate + merge → Task 7 ✓
- §HTTP request fields + response shape change → Task 8 ✓
- §HTTP existing tests migration → Task 8 ✓
- §Testing unit (ranking) → Task 1 ✓
- §Testing unit (transcript_recall scoring) → Task 5 ✓
- §Testing unit (window merge) → Task 6 ✓
- §Testing integration → Task 9 ✓
- §Testing memories regression → Task 1 step 5 ✓
- §Concerns DuckDB FTS where := probe → Task 2 ✓
- §Concerns drop_fts_index on conversation_messages → covered by Task 3 ensure_fresh ✓
- §Concerns memories zero-behavior-change fixture validation → Task 1 step 5 ✓
- §Concerns window merge N² cap → Task 7 (limit cap 100) ✓
- §Concerns anchor_session_id with no rows → Task 7 anchor_session_candidates returns empty Vec; downstream just no-ops ✓

**Placeholder scan:** No `TBD`, no `TODO`, no `implement later`. The "BRANCH per Task 2 outcome" SQL placeholders in Task 3 are explicitly marked and have both fallback strings included. No naked `Similar to Task N` references — every code block is complete.

**Type consistency:**
- `MergedWindow`, `PrimaryWithContext`, `ScoredBlock`, `ScoringOpts` defined in Task 5/6, used in Task 7 ✓
- `ContextWindow` defined in Task 4, used in Task 7 ✓
- `TranscriptSearchOpts` defined in Task 7, used in Task 8 ✓
- `TranscriptWindow`, `TranscriptWindowBlock` defined in Task 8, no other consumers ✓
- `bm25_transcript_candidates`, `anchor_session_candidates`, `recent_conversation_messages` (existing), `fetch_conversation_messages_by_ids` (existing), `context_window_for_block` — signatures all consistent across tasks ✓
- `rrf_contribution`, `freshness_score`, `timestamp_score` — defined in Task 1 ranking.rs, used in Tasks 1 (retrieve), 5 (scorer), 6 (merge) — names and signatures match ✓

---

## Execution Handoff

**Plan complete and saved to `docs/superpowers/plans/2026-05-01-transcript-recall.md`. Two execution options:**

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration. Same model used for the conversation-archive plan.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints. Better if you want to see every detail and intervene mid-task.

**Which approach?**
