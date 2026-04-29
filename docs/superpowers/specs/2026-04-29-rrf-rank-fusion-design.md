# RRF Rank Fusion for Hybrid Retrieval — Design

> Closes ROADMAP #5 (mempalace-diff §3 启示 #3): replace the additive integer scoring of lexical/semantic recall with two-list Reciprocal Rank Fusion, keeping lifecycle signals as additive boost.

## Summary

`pipeline/retrieve.rs::score_candidates_hybrid` currently combines lexical and semantic recall via `semantic_sim × 64 + 26 (if both)` plus 7+ additive lifecycle signals (scope, intent, confidence, freshness, decay, graph, provisional, validated). This produces two real problems:

1. **Scale imbalance**: semantic top contribution (64) dwarfs lifecycle signals individually (scope max 18, confidence max 10) and even sums of them. Tuning is fragile.
2. **Lex-only candidates get zero recall score**: a memory matched only by lexical search has no recall contribution to its score; it relies entirely on lifecycle signals to rank. This is a latent correctness gap, not just a quality issue.

This spec replaces the recall combination with two-list RRF (`1/(k+rank)`) scaled to integer space (× 1000), preserving the lifecycle additive layer verbatim. The change is contained to one function plus its caller's input shape; no public API changes.

## Goals

- Replace `semantic_sim × 64` and `+26 (lex ∩ sem)` with `(rrf_lex + rrf_sem) × 1000` rounded to i64
- Give lex-only candidates a non-zero recall contribution
- Keep `ScoredMemory.score: i64` and the existing sort tie-break chain unchanged
- Preserve every lifecycle scoring helper (`text_match_score`, `scope_score`, `memory_type_score`, `confidence_score`, `validation_score`, `freshness_score`, `staleness_penalty`, graph_boost, provisional penalty) verbatim
- Provide a `MEM_RANKER=legacy` env-var kill switch (one-minor sunset, mirroring §3 `MEM_VECTOR_INDEX_USE_LEGACY`)
- Add unit tests guarding RRF math + the kill switch + the "lex-only now has recall" invariant

## Non-Goals

- Changing `score_candidates` (the non-hybrid lexical-only path used when no semantic results exist)
- Re-tuning lifecycle scoring weights (`confidence_score`, `freshness_score`, etc.) — that is a separate effort if needed after this lands
- Switching `ScoredMemory.score` from `i64` to `f64` — out of scope for S sizing
- Multi-list RRF over lifecycle signals (treating scope, freshness, etc. as ranked lists)
- Changing the public HTTP search response shape

## Decisions (resolved during brainstorming)

- **Q1 — Combination model**: two-list RRF for lex+sem, lifecycle remains additive. Other models (full RRF, normalize-and-weight, light rescale) rejected.
- **Q2 — Score type**: keep `i64` by scaling RRF × 1000. f64 throughout would touch every helper signature; out of S budget.
- **K constant**: `RRF_K = 60` (industry standard from the original RRF paper; not exposed as a knob in this PR).
- **Kill switch**: include `MEM_RANKER=legacy` for one-release safety net.

## Algorithm

```
const RRF_K: usize = 60;
const RRF_SCALE: i64 = 1000;

for each candidate:
    rrf_lex = lexical_ranks.get(memory_id).map(|r| 1.0 / (RRF_K + r) as f64).unwrap_or(0.0)
    rrf_sem = semantic_ranks.get(memory_id).map(|r| 1.0 / (RRF_K + r) as f64).unwrap_or(0.0)

    score = ((rrf_lex + rrf_sem) * RRF_SCALE as f64).round() as i64

    # lifecycle additive layer — verbatim from current code
    if memory.evidence.non_empty() { score += 2 }
    score += text_match_score(...)
    score += scope_score(...)
    score += memory_type_score(...)
    score += confidence_score(...)
    score += validation_score(...)
    score += freshness_score(...)
    score -= staleness_penalty(...)
    if memory_id in related_memory_ids { score += graph_boost }
    if status in {Provisional, PendingConfirmation} { score -= 4 }
```

**RRF magnitude reference**:

| Scenario | Raw RRF | × 1000 → i64 |
|---|---|---|
| Single-path, rank 1 | 0.01639 | **16** |
| Both-paths, both rank 1 | 0.03279 | **33** |
| Single-path, rank 100 | 0.00625 | **6** |
| Single-path, rank 200 | 0.00385 | **4** |
| Single-path, rank 500 | 0.00179 | **2** |

At the search recall depth this codebase uses (default `MEM_VECTOR_INDEX_OVERSAMPLE × limit ≈ 192`), RRF integer values stay distinguishable down to the bottom of the list.

## Function Signature

`score_candidates_hybrid` gains two new parameters and drops two existing ones:

**Before**:
```rust
fn score_candidates_hybrid(
    candidates: Vec<MemoryRecord>,
    query: &SearchMemoryRequest,
    related_memory_ids: &HashSet<String>,
    graph_boost: i64,
    lexical_ids: &HashSet<String>,
    semantic_sims: &HashMap<String, f32>,
) -> Vec<ScoredMemory>
```

**After**:
```rust
fn score_candidates_hybrid(
    candidates: Vec<MemoryRecord>,
    query: &SearchMemoryRequest,
    related_memory_ids: &HashSet<String>,
    graph_boost: i64,
    lexical_ranks: &HashMap<String, usize>,    // 1-based rank in lexical recall list
    semantic_ranks: &HashMap<String, usize>,   // 1-based rank in semantic recall list
) -> Vec<ScoredMemory>
```

`semantic_sims: HashMap<String, f32>` is no longer needed (raw cosine value is gone — RRF only uses rank). `lexical_ids: HashSet<String>` is replaced by the keyset of `lexical_ranks`.

## Caller Adjustments

`merge_and_rank_hybrid` (line 25) currently builds `lexical_ids` and `semantic_sims`. Replace with rank maps:

```rust
let lexical_ranks: HashMap<String, usize> = lexical
    .iter()
    .enumerate()
    .map(|(i, m)| (m.memory_id.clone(), i + 1))
    .collect();
let semantic_ranks: HashMap<String, usize> = semantic
    .iter()
    .enumerate()
    .map(|(i, (m, _sim))| (m.memory_id.clone(), i + 1))
    .collect();
```

The existing dedup/merge step that builds `candidates: Vec<MemoryRecord>` (combining lex + sem unique) stays — only the auxiliary maps change.

`rank_with_graph_hybrid` (line 59) and `rank_with_graph` (line 97) call into `merge_and_rank_hybrid` with the same shapes; nothing else needs touching.

## Kill Switch

```rust
fn use_legacy_ranker() -> bool {
    std::env::var("MEM_RANKER")
        .ok()
        .map(|v| v == "legacy")
        .unwrap_or(false)
}
```

Read once at the top of `score_candidates_hybrid`; if true, dispatch to a private `score_candidates_hybrid_legacy` containing the verbatim pre-PR implementation. Both functions share the same signature so the switch is internal.

The legacy function is preserved for one minor release; remove in the PR after the next tag.

## Testing Strategy

### New unit tests in `src/pipeline/retrieve.rs::tests`

1. **`rrf_recall_only_lexical`** — candidate appears in `lexical_ranks` at rank 1, absent from `semantic_ranks`. Score (after stripping known lifecycle contributions for a clean fixture) ≈ `1000/(60+1) = 16`.
2. **`rrf_both_paths_top_rank`** — candidate at rank 1 in both maps. RRF contribution = `(2/61 × 1000).round() = 33` (32.787 rounds up).
3. **`rrf_rank_monotonic`** — three candidates with ranks 1, 50, 100 in semantic only → recall scores strictly decreasing.
4. **`lex_only_candidate_has_nonzero_recall_after_rrf`** — explicit guard for the bug fix: candidate in `lexical_ranks` only, not in `semantic_ranks`, with otherwise neutral fixture → recall portion of score is positive (was zero pre-PR).
5. **`legacy_kill_switch_replicates_old_scoring`** — set `MEM_RANKER=legacy`, build a small fixture with deterministic scores, assert exact i64 values match the old `semantic_sim × 64 + 26 (intersect)` formula.

The legacy test sets/unsets the env var with `unsafe { std::env::set_var(...) }` per the §3 #20 pattern.

### Existing integration tests

Run unchanged; expect:
- `tests/search_api.rs` (514 LOC) — most top-1 / set-membership assertions should still pass because high-confidence candidates were ranked correctly under both models. Ordering ties may shuffle.
- `tests/hybrid_search.rs` (234 LOC) — same expectation.
- `tests/semantic_search_via_ann.rs` — only uses `repo.semantic_search_memories` (storage layer, not retrieve), unaffected.

For each integration test that breaks:
1. Inspect what changed and why.
2. If the new ordering reflects the bug fix (lex-only candidate moved up appropriately), update the assertion and add a comment referencing this spec.
3. If the new ordering is unexpected, debug the RRF wiring; do not just rubber-stamp the test update.

## Module Layout

**Modify only**: `src/pipeline/retrieve.rs` — function bodies + caller edits within the same file. Existing helper functions (`text_match_score`, `scope_score`, `memory_type_score`, `confidence_score`, `validation_score`, `freshness_score`, `staleness_penalty`, `tokenize`, `parse_scope_filters`, `timestamp_score`, `normalized_haystack`) are untouched.

**No new files**. No new dependencies. No schema changes. No CLI changes.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `MEM_RANKER` | unset | Set to `legacy` to bypass RRF and use the additive `semantic_sim × 64 + 26` formula. One-minor sunset, removal pending. |

No `RRF_K` env var: the constant is hard-coded at 60. If we ever want to tune, that's a follow-up scope expansion.

## Error Handling

No new error paths. RRF computation is pure arithmetic on `f64`s and a single rounding cast. Empty rank maps simply produce zero contribution (`unwrap_or(0.0)`).

`MEM_RANKER` parsing is permissive — any value other than the literal string `"legacy"` keeps RRF active.

## Crash / Recovery

Not applicable — `score_candidates_hybrid` is in-memory-only and stateless.

## Out of Scope (this PR)

- Tuning RRF_K, RRF_SCALE, or any lifecycle weight
- Multi-list RRF over lifecycle signals
- Splitting `retrieve.rs` (522 LOC currently — still tractable)
- Adjusting `score_candidates` (non-hybrid path)
- ROADMAP #5 / mempalace-diff §3 启示 #3 status update — happens in the final commit of the implementation, not here

## Verification Checklist (pre-merge)

- `cargo test -q` — all suites pass; integration test ordering changes documented in commits
- `cargo fmt --check` — clean
- `cargo clippy --all-targets -- -D warnings` — clean
- `MEM_RANKER=legacy` smoke: hand-craft a curl + assert the legacy formula reproduces (one ad-hoc check)
- Default smoke: `cargo run -- serve` → ingest a few memories → POST `/memories/search` → verify response shape unchanged

## References

- ROADMAP.MD row #5 (the line being closed)
- mempalace-diff §3 启示 #3 (the original write-up + 2026-04-29 review note)
- `src/pipeline/retrieve.rs::score_candidates_hybrid` (lines 149-218 — the function being rewritten)
- `src/pipeline/retrieve.rs::merge_and_rank_hybrid` (line 25 — caller building the rank maps)
- `docs/superpowers/specs/2026-04-27-vector-index-sidecar-design.md` — pattern reference for `MEM_*_USE_LEGACY` kill switch
- Cormack, Clarke, Buettcher (2009) "Reciprocal Rank Fusion outperforms Condorcet and individual Rank Learning Methods" — the RRF source paper
