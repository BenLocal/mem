# Lifecycle Score Extraction (Stage 2 Split) — Design

> Closes ROADMAP #11 (mempalace-diff §12) — but as the **reduced "Stage 2 split only"** scope that §12 line 513 explicitly contemplates: "如果 #3 落地后召回质量已经够好，#12 可降级为只做 Stage 2 拆分（半天）".

## Summary

Today's `pipeline/retrieve.rs` has two parallel scorer functions, `score_candidates_hybrid_rrf` and `score_candidates_hybrid_legacy`, each ~70 lines. Inside each, a recall calculation (RRF or `semantic_sim × 64`) and ten additive lifecycle/relevance signals (text match, scope, memory_type×intent, confidence, validation, freshness, staleness, graph_boost, provisional penalty, evidence bonus) are interleaved.

The lifecycle layer is duplicated verbatim across the two scorers and across `score_candidates` (the non-hybrid path). This spec extracts it into one shared `apply_lifecycle_score` helper. Behavior is **identical** — the math, the order of operations, and the resulting i64 score are unchanged. The win is a single source of truth, three unit tests on the lifecycle layer alone, and a foundation for future Stage 3 work (exposing `score_breakdown` to callers).

## Goals

- One private helper `apply_lifecycle_score(memory, query, query_terms, scope_filters, newest, related_memory_ids, graph_boost) -> i64` that computes every additive non-recall component the existing scorers compute today.
- `score_candidates_hybrid_rrf` collapses to: `score = recall_rrf(...) + apply_lifecycle_score(...)`.
- `score_candidates_hybrid_legacy` collapses to: `score = recall_legacy(...) + apply_lifecycle_score(...)`.
- `score_candidates` (the non-hybrid path) calls `apply_lifecycle_score` instead of inlining the same arithmetic.
- 3 new unit tests target the helper directly.
- All existing tests pass without assertion updates — proof of behavioral equivalence.

## Non-Goals

- Changing recall math (RRF formula stays, kill switch stays).
- Changing the `ScoredMemory.score: i64` type or the sort tiebreak chain.
- Exposing `recall_score` / `lifecycle_score` separately in the search response (that's the §12 "Stage 3" work; this spec stays internal).
- Refactoring `compress.rs` (also Stage 3 work).
- Re-tuning any individual lifecycle weight.
- Removing the `MEM_RANKER=legacy` kill switch.
- Splitting `retrieve.rs` into multiple files.

## Decisions (resolved during brainstorming)

- **Helper scope**: include **all** additive non-recall signals — text_match, scope, memory_type×intent, confidence, validation, freshness, staleness, graph_boost, provisional penalty, evidence bonus. The "lifecycle" name covers them as a group; this is internal naming, not a contract.
- **Helper visibility**: `fn` (private). It's an internal pipeline helper, not a public API.
- **Order of operations**: keep the exact order of the current scorers (`evidence → text_match → scope → memory_type → confidence → validation → freshness → -staleness → graph_boost → provisional`). This matters only for i64 overflow, which isn't a real risk at current magnitudes — but preserving order makes side-by-side diffing trivial.
- **Test pattern**: same as the existing `lifecycle_baseline_for` helper in retrieve.rs::tests. The new helper is the production version of what that test helper has been computing for the RRF tests.

## Algorithm

`apply_lifecycle_score`:

```rust
fn apply_lifecycle_score(
    memory: &MemoryRecord,
    query: &SearchMemoryRequest,
    query_terms: &[String],
    scope_filters: &HashMap<String, Vec<String>>,
    newest: u128,
    related_memory_ids: &HashSet<String>,
    graph_boost: i64,
) -> i64 {
    let mut score = 0i64;

    if !memory.evidence.is_empty() {
        score += 2;
    }
    score += text_match_score(memory, query_terms);
    score += scope_score(memory, scope_filters);
    score += memory_type_score(&memory.memory_type, &query.intent);
    score += confidence_score(memory.confidence);
    score += validation_score(memory.last_validated_at.is_some());
    score += freshness_score(newest, timestamp_score(&memory.updated_at));
    score -= staleness_penalty(memory.decay_score);

    if related_memory_ids.contains(&memory.memory_id) {
        score += graph_boost;
    }

    if matches!(
        memory.status,
        MemoryStatus::Provisional | MemoryStatus::PendingConfirmation
    ) {
        score -= 4;
    }

    score
}
```

This is a **literal extraction** from the existing inline code. Every line is moved, not rewritten.

## Caller Adjustments

### `score_candidates_hybrid_rrf` (lines ~252–324)

Inside the `.map(|memory| { ... })` closure, the section starting `if !memory.evidence.is_empty()` through the closing `if matches!(... Provisional ...)` block is replaced by a single line:

```rust
score += apply_lifecycle_score(
    &memory,
    query,
    &query_terms,
    &scope_filters,
    newest,
    related_memory_ids,
    graph_boost,
);
```

The recall computation (RRF lex + sem × 1000) and the trailing sort logic remain unchanged.

### `score_candidates_hybrid_legacy` (lines ~178–250)

Same pattern: replace the duplicated lifecycle block with the same `apply_lifecycle_score` call. The recall computation (`semantic_sim × 64 + 26 (lex∩sem)`) and the sort logic are untouched.

### `score_candidates` (lines ~326–381, the non-hybrid path)

Read the function. The same lifecycle signals appear; replace each inline calculation with the helper call. The function previously had no recall component (it only ranks pure-lexical candidates), so:

```rust
let score = apply_lifecycle_score(
    &memory,
    query,
    &query_terms,
    &scope_filters,
    newest,
    related_memory_ids,
    graph_boost,
);
```

If `score_candidates` did NOT have all of the same signals (e.g., lacks the evidence bonus), audit and adjust the helper invocation accordingly. The implementer must verify the behavioral parity by running the full test suite and comparing assertion expectations — none should change.

**If `score_candidates` has any signal NOT in the helper**, that's a divergence the spec missed. Surface it as `DONE_WITH_CONCERNS` in the implementation report and propose: (a) add the missing signal to the helper if it should apply universally; (b) keep `score_candidates` inlining only the divergent signal alongside the helper call.

## Testing Strategy

### New unit tests in `pipeline/retrieve.rs::tests`

1. **`apply_lifecycle_score_neutral_input`** — Build a fixture with `evidence=[]`, `confidence=0.0`, `decay_score=0.0`, `status=Active`, `last_validated_at=None`, no scope filters, no graph membership, neutral query. Expected: returns the sum of `memory_type_score(Implementation, "")` (= 5 per the existing test helper) + `freshness_score(newest, newest)` (= 6 per the existing test helper) = 11. Or whatever the helpers actually return today; either way, assert via direct re-computation:
   ```rust
   let expected = memory_type_score(&memory.memory_type, &query.intent)
       + freshness_score(newest, newest);
   let actual = apply_lifecycle_score(&memory, &query, &[], &HashMap::new(), newest, &HashSet::new(), 0);
   assert_eq!(actual, expected);
   ```
   This makes the test robust to future weight retuning.

2. **`apply_lifecycle_score_provisional_status_penalty`** — Same fixture but `status=Provisional`. Assert that the result equals `apply_lifecycle_score(neutral_fixture) - 4`.

3. **`apply_lifecycle_score_graph_neighbor_boost`** — Same neutral fixture. Insert `memory.memory_id` into the `related_memory_ids` set; pass `graph_boost = 12`. Assert that the result equals `apply_lifecycle_score(neutral_fixture, related=empty, graph_boost=0) + 12`.

### Existing tests (must continue to pass without assertion updates)

- `rrf_recall_only_lexical`
- `rrf_both_paths_top_rank`
- `rrf_rank_monotonic`
- `lex_only_candidate_has_nonzero_recall_after_rrf`
- `legacy_kill_switch_replicates_old_scoring`
- `tests/search_api.rs`
- `tests/hybrid_search.rs`

The behavioral-equivalence guarantee: refactoring is faithful only if every existing test passes with **zero assertion changes**.

If any existing test breaks during the refactor, that's a real divergence — investigate before adapting the test. Do not silently update assertions.

## File Changes

**Modify only**: `src/pipeline/retrieve.rs`. The existing helpers (`text_match_score`, `scope_score`, `memory_type_score`, `confidence_score`, `validation_score`, `freshness_score`, `staleness_penalty`, `tokenize`, `parse_scope_filters`, `timestamp_score`, `normalized_haystack`) are untouched.

**No new files**. No new dependencies. No schema changes. No CLI / HTTP / MCP changes.

## Configuration

No new env vars. The existing `MEM_RANKER=legacy` kill switch continues to work — both code paths now call `apply_lifecycle_score`, so the legacy formula remains: `(2 × ((sim+1)/2) × 64 + 26-if-intersect) + apply_lifecycle_score(...)`.

## Error Handling

No new error paths. The helper is pure arithmetic on already-validated inputs.

## Crash / Recovery

Not applicable — `apply_lifecycle_score` is in-memory-only and stateless.

## Out of Scope (this PR)

- Exposing `recall_score` / `lifecycle_score` in the HTTP search response
- Adding a `RankedCandidate { memory, recall, lifecycle, breakdown }` type
- Modifying `compress.rs`
- Removing the `_legacy` scorer (still inside its one-minor sunset window from ROADMAP #5)
- Adjusting the `RRF_K = 60` or `RRF_SCALE = 1000.0` constants
- Re-tuning any lifecycle weight
- Splitting `retrieve.rs` into multiple files (still tractable at 828 LOC)

## Verification Checklist (pre-merge)

- `cargo test -q` — all suites pass without any assertion updates
- `cargo fmt --check` — clean
- `cargo clippy --all-targets -- -D warnings` — clean
- `cargo build --release` — clean
- Manual diff inspection: confirm that the inlined lifecycle blocks in `score_candidates_hybrid_rrf` / `_legacy` / `score_candidates` are deleted and replaced verbatim by the helper call. No "while we're here" tweaks.

## References

- ROADMAP.MD row #11
- mempalace-diff §12 (the original three-stage design — this spec implements only the Stage 2 split)
- mempalace-diff §12 line 513 ("可降级为只做 Stage 2 拆分（半天）") — the scope-reduction trigger this spec activates
- `src/pipeline/retrieve.rs::score_candidates_hybrid_rrf` (lines ~252–324 — caller)
- `src/pipeline/retrieve.rs::score_candidates_hybrid_legacy` (lines ~178–250 — caller)
- `src/pipeline/retrieve.rs::score_candidates` (lines ~326–381 — caller)
- `docs/superpowers/specs/2026-04-29-rrf-rank-fusion-design.md` — pattern reference for "behavioral-equivalence refactor inside retrieve.rs"
