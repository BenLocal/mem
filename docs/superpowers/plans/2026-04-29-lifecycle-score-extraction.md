# Lifecycle Score Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the duplicated lifecycle additive layer (10 signals) from `score_candidates_hybrid_rrf`, `score_candidates_hybrid_legacy`, and `score_candidates` into one shared `apply_lifecycle_score` helper. **Pure refactor — zero behavior change.**

**Architecture:** A new private function in `src/pipeline/retrieve.rs` that takes a `MemoryRecord`, the parsed query bits (terms, scope filters, newest timestamp), and graph context. Returns `i64`. Each scorer keeps its own recall computation and replaces ~22 lines of inlined lifecycle scoring with a single helper call. The function is unit-tested in 3 cases targeting the helper directly; the existing 5 RRF / legacy tests + integration tests continue to pass with zero assertion changes (proof of behavioral equivalence).

**Tech Stack:** Rust 2021. No new dependencies, no schema changes, no public API changes.

**Spec:** `docs/superpowers/specs/2026-04-29-lifecycle-score-extraction-design.md`

---

## File Structure

**Modify only:** `src/pipeline/retrieve.rs`. Existing private helpers (`text_match_score`, `scope_score`, `memory_type_score`, `confidence_score`, `validation_score`, `freshness_score`, `staleness_penalty`) are untouched.

**No new files.** No new deps. No schema migrations. No CLI / HTTP / MCP surface change.

---

## Task 1: Audit `score_candidates` for divergent signals

**Why this is a separate task:** the spec assumes `score_candidates` (non-hybrid path) has the same 10 signals as the two hybrid scorers. If it diverges, the helper either widens or stays narrow. We confirm before refactoring.

**Files:**
- Read-only inspection of: `src/pipeline/retrieve.rs::score_candidates`

- [ ] **Step 1: Read the function**

```bash
grep -n "^fn score_candidates" src/pipeline/retrieve.rs
```

Note line numbers for each scorer:
- `score_candidates_hybrid_legacy` (~line 178)
- `score_candidates_hybrid_rrf` (~line 252)
- `score_candidates` (~line 326)

Read all three function bodies in `src/pipeline/retrieve.rs` to compare.

- [ ] **Step 2: Build a divergence checklist**

For each of the 10 signals the spec lists, confirm presence in each scorer:

| Signal | `_rrf` | `_legacy` | `score_candidates` |
|---|---|---|---|
| evidence bonus (`+2`) | ? | ? | ? |
| `text_match_score` | ? | ? | ? |
| `scope_score` | ? | ? | ? |
| `memory_type_score` | ? | ? | ? |
| `confidence_score` | ? | ? | ? |
| `validation_score` | ? | ? | ? |
| `freshness_score` | ? | ? | ? |
| `staleness_penalty` (`-`) | ? | ? | ? |
| graph_boost (when in `related_memory_ids`) | ? | ? | ? |
| Provisional/PendingConfirmation penalty (`-4`) | ? | ? | ? |

Fill the table by reading the code. Don't guess.

- [ ] **Step 3: Decide the helper scope**

If all three scorers have all 10 signals → helper covers all 10, all three callers use it identically. **Spec stays intact.**

If `score_candidates` is missing some signals (e.g., no graph_boost — likely, since it's the non-hybrid path that may not pass through graph expansion) → either:
- (a) Add the missing signal to `score_candidates`'s helper invocation by passing default values (`&HashSet::new()`, `0` for graph_boost). The helper's behavior in those cases is correct (no boost applied if memory not in empty set).
- (b) Keep the helper covering all 10 but document that `score_candidates` invokes with neutral inputs for any signals it didn't have before.

**Default to (a)** — the helper signature is wide enough that passing neutral values for unused signals doesn't change behavior. This keeps the helper general.

If a more drastic divergence is found (e.g., `score_candidates` has signals NOT in the hybrid scorers), that's a real spec issue. Stop and report `DONE_WITH_CONCERNS` per the spec's instruction.

- [ ] **Step 4: Document the audit in a brief note**

Write the audit table into a comment at the top of the new helper (Task 2 will write the helper body; for now, just collect the data and decide the strategy).

If there are no divergences, no action needed beyond proceeding to Task 2.

- [ ] **Step 5: No commit yet**

This task is reconnaissance. Move to Task 2.

---

## Task 2: Add `apply_lifecycle_score` helper + 3 unit tests (TDD)

**Files:**
- Modify: `src/pipeline/retrieve.rs`

- [ ] **Step 1: Write the failing tests first**

Find the existing `#[cfg(test)] mod tests` block in `src/pipeline/retrieve.rs` (around line 600+ after the RRF tests landed). Append:

```rust
#[test]
fn apply_lifecycle_score_neutral_input() {
    let memory = fixture_memory("mem_neutral");
    let query = fixture_query();
    let newest = timestamp_score(&memory.updated_at);

    let actual = apply_lifecycle_score(
        &memory,
        &query,
        &[],
        &HashMap::new(),
        newest,
        &HashSet::new(),
        0,
    );

    // Stable expectation: re-compute via the same helpers the function uses.
    let expected = memory_type_score(&memory.memory_type, &query.intent)
        + freshness_score(newest, newest)
        - staleness_penalty(memory.decay_score);
    assert_eq!(actual, expected, "neutral fixture should produce only memory_type + freshness contributions");
}

#[test]
fn apply_lifecycle_score_provisional_status_penalty() {
    let mut memory = fixture_memory("mem_provisional");
    memory.status = MemoryStatus::Provisional;
    let query = fixture_query();
    let newest = timestamp_score(&memory.updated_at);

    let baseline = {
        let mut neutral = memory.clone();
        neutral.status = MemoryStatus::Active;
        apply_lifecycle_score(
            &neutral,
            &query,
            &[],
            &HashMap::new(),
            newest,
            &HashSet::new(),
            0,
        )
    };

    let actual = apply_lifecycle_score(
        &memory,
        &query,
        &[],
        &HashMap::new(),
        newest,
        &HashSet::new(),
        0,
    );

    assert_eq!(
        actual,
        baseline - 4,
        "Provisional status must subtract 4 from the baseline"
    );
}

#[test]
fn apply_lifecycle_score_graph_neighbor_boost() {
    let memory = fixture_memory("mem_with_neighbor");
    let query = fixture_query();
    let newest = timestamp_score(&memory.updated_at);

    let baseline = apply_lifecycle_score(
        &memory,
        &query,
        &[],
        &HashMap::new(),
        newest,
        &HashSet::new(),
        0,
    );

    let mut related = HashSet::new();
    related.insert("mem_with_neighbor".to_string());

    let actual = apply_lifecycle_score(
        &memory,
        &query,
        &[],
        &HashMap::new(),
        newest,
        &related,
        12,
    );

    assert_eq!(
        actual,
        baseline + 12,
        "memory in related set must add graph_boost"
    );
}
```

These reference `apply_lifecycle_score`, which doesn't exist yet — that's the failing TDD state.

- [ ] **Step 2: Verify tests fail to compile**

```bash
cargo test --lib retrieve::tests::apply_lifecycle_score 2>&1 | tail -10
```

Expected: compile error — `apply_lifecycle_score` not in scope.

- [ ] **Step 3: Implement the helper**

Add to `src/pipeline/retrieve.rs`, just above `score_candidates_hybrid_legacy` (around line 175):

```rust
/// Computes the additive non-recall portion of a memory's score.
///
/// Used by all three scorers (`score_candidates_hybrid_rrf`,
/// `score_candidates_hybrid_legacy`, `score_candidates`) so the lifecycle
/// math has a single source of truth. The recall computation (RRF, legacy
/// `semantic_sim × 64 + 26`, or none for pure-lexical) is the only thing
/// that differs between scorers.
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

This is a **literal extraction** from the inlined code in `_rrf` / `_legacy` — every line is moved, not rewritten. Order of operations matches the existing scorers exactly.

- [ ] **Step 4: Run the new tests → expect pass**

```bash
cargo build 2>&1 | tail -10
cargo test --lib retrieve::tests::apply_lifecycle_score 2>&1 | tail -10
```

Expected: 3/3 pass.

- [ ] **Step 5: Run full lib + integration test suite**

```bash
cargo test -q 2>&1 | tail -30
```

Expected: clean. The helper exists but is not yet called from any scorer — no behavior change.

- [ ] **Step 6: Lint**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
```

- [ ] **Step 7: Commit**

```bash
git add src/pipeline/retrieve.rs
git commit -m "refactor(retrieve): introduce apply_lifecycle_score helper

Pure addition. Helper computes the additive non-recall layer that
all three scorers currently inline verbatim. 3 unit tests cover
neutral input, provisional penalty, and graph neighbor boost.

Refs ROADMAP #11"
```

---

## Task 3: Migrate `score_candidates_hybrid_rrf` to the helper

**Files:**
- Modify: `src/pipeline/retrieve.rs`

- [ ] **Step 1: Read the current `score_candidates_hybrid_rrf`**

```bash
grep -n "^fn score_candidates_hybrid_rrf" src/pipeline/retrieve.rs
```

Read the full function. Identify the inlined block (currently around lines ~285–321 — the section starting `if !memory.evidence.is_empty()` through the `Provisional` penalty).

- [ ] **Step 2: Replace the inline block with the helper call**

Inside the closure passed to `.map`, replace the entire inlined lifecycle block with:

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

The lines to remove are (literal text from the existing function):

```rust
if !memory.evidence.is_empty() {
    score += 2;
}
score += text_match_score(&memory, &query_terms);
score += scope_score(&memory, &scope_filters);
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
```

The recall computation above this block (the RRF formula) and the sort logic below it stay exactly as they were.

Update the `// Lifecycle additive layer — same scoring axes as _legacy; duplication is intentional (legacy will be removed).` comment to reflect that this is now a helper call:

```rust
// Lifecycle additive layer — extracted to apply_lifecycle_score for shared math.
```

(or remove the comment entirely if the helper call is self-explanatory).

- [ ] **Step 3: Build + run all retrieve tests**

```bash
cargo build 2>&1 | tail -10
cargo test --lib retrieve -q 2>&1 | tail -20
```

Expected: ALL existing retrieve tests pass with zero assertion changes. Specifically, these RRF tests must still pass:
- `rrf_recall_only_lexical`
- `rrf_both_paths_top_rank`
- `rrf_rank_monotonic`
- `lex_only_candidate_has_nonzero_recall_after_rrf`

If any test fails, the helper's behavior diverged from the inlined code. Investigate:
- Order of operations changed?
- A signal accidentally dropped or added?
- The neutral default values (empty HashSet, graph_boost=0) trigger something they shouldn't have?

Don't paper over a failure. The whole point of this task is "refactor proven by zero test changes."

- [ ] **Step 4: Run full integration suite**

```bash
cargo test -q 2>&1 | tail -30
```

Expected: clean. `tests/search_api.rs` and `tests/hybrid_search.rs` continue to pass.

- [ ] **Step 5: Lint**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
```

- [ ] **Step 6: Commit**

```bash
git add src/pipeline/retrieve.rs
git commit -m "refactor(retrieve): score_candidates_hybrid_rrf uses lifecycle helper

Drops ~22 lines of inlined lifecycle scoring; replaces with a
single apply_lifecycle_score call. Behavior unchanged — all
existing RRF and integration tests pass without assertion edits.

Refs ROADMAP #11"
```

---

## Task 4: Migrate `score_candidates_hybrid_legacy` to the helper

Same surgery, on the legacy scorer.

**Files:**
- Modify: `src/pipeline/retrieve.rs`

- [ ] **Step 1: Read the current `score_candidates_hybrid_legacy`**

```bash
grep -n "^fn score_candidates_hybrid_legacy" src/pipeline/retrieve.rs
```

Read the full function. The inlined lifecycle block in `_legacy` is **identical** to the one in `_rrf` (per the spec's invariant from ROADMAP #5).

- [ ] **Step 2: Replace the inline block with the helper call**

Inside the `.map` closure, replace the lifecycle block exactly as in Task 3:

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

The recall computation above (the legacy `((sim+1)/2) × 64` + intersect bonus) and the sort logic below stay unchanged.

Update or remove the `// Lifecycle additive layer ...` comment to reflect the new helper call.

- [ ] **Step 3: Build + run tests**

```bash
cargo build 2>&1 | tail -10
cargo test --lib retrieve -q 2>&1 | tail -20
```

Expected: ALL tests pass, including:
- `legacy_kill_switch_replicates_old_scoring` — the legacy formula must still produce the same output as before.

- [ ] **Step 4: Run full integration suite**

```bash
cargo test -q 2>&1 | tail -30
```

Expected: clean.

- [ ] **Step 5: Lint**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
```

- [ ] **Step 6: Commit**

```bash
git add src/pipeline/retrieve.rs
git commit -m "refactor(retrieve): score_candidates_hybrid_legacy uses lifecycle helper

Same extraction as the RRF scorer. Both scorers now share one
source of truth for the additive non-recall layer. Behavior
unchanged — all existing tests including legacy kill-switch pass.

Refs ROADMAP #11"
```

---

## Task 5: Migrate `score_candidates` (non-hybrid path) to the helper

**Files:**
- Modify: `src/pipeline/retrieve.rs`

- [ ] **Step 1: Read `score_candidates`**

```bash
grep -n "^fn score_candidates" src/pipeline/retrieve.rs
```

Read the full function (around line 326).

- [ ] **Step 2: Apply the audit decision from Task 1**

Recall the divergence checklist from Task 1. Apply the strategy decided there:

- If `score_candidates` had all 10 signals: replace inline block with the helper, passing the actual `related_memory_ids` and `graph_boost` arguments the function already has.
- If `score_candidates` was missing some signals (e.g., never used graph_boost): pass neutral defaults (`&HashSet::new()`, `0`) for the missing arguments. The helper's contribution for those signals will be zero — matching the previous inlined behavior.
- If `score_candidates` had EXTRA signals not in the helper: that's the spec divergence Task 1 should have flagged. Stop and report.

Practical replacement template:

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

Adapt the argument names if `score_candidates` uses different variable names (e.g., if it doesn't compute `newest` and instead uses something else, compute `newest` here using the same `candidates.iter().map(|m| timestamp_score(&m.updated_at)).max().unwrap_or(0)` formula the helper expects).

- [ ] **Step 3: Build + run all tests**

```bash
cargo build 2>&1 | tail -10
cargo test -q 2>&1 | tail -30
```

Expected: clean. All tests pass.

If the integration tests `tests/search_api.rs` show ordering changes that map to `score_candidates` (the non-hybrid path is exercised when `semantic` is empty — `rank_with_graph_hybrid` line 95 falls through to `rank_with_graph` which uses `score_candidates`), investigate. The behavior must match exactly.

- [ ] **Step 4: Lint**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
```

- [ ] **Step 5: Commit**

```bash
git add src/pipeline/retrieve.rs
git commit -m "refactor(retrieve): score_candidates uses lifecycle helper

Last of the three scorers migrated. retrieve.rs now has a single
source of truth for the additive non-recall layer.

Refs ROADMAP #11"
```

---

## Task 6: Final verification + close ROADMAP #11

**Files:**
- Modify: `docs/ROADMAP.MD`
- Modify: `docs/mempalace-diff.md`

- [ ] **Step 1: Full verification**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test -q
cargo build --release
```

All clean.

- [ ] **Step 2: Inspect file size impact**

```bash
wc -l src/pipeline/retrieve.rs
```

Note the line count. Before this PR, `retrieve.rs` was 828 lines. After three lifecycle-block extractions (~22 lines each removed, replaced by ~9-line helper calls), the file should be ~30-40 lines shorter — somewhere around 790 lines. If it grew, investigate (extra duplication you missed?).

- [ ] **Step 3: Mark ROADMAP #11 complete**

In `docs/ROADMAP.MD`, find row 11:
```markdown
| 11 | 🔍 | **检索流水线三段式重构**（召回 → 结构化过滤 → caller 端 LLM 精排）| 🟠 架构升级 | M（1 天） | 中（依赖 #2 + #5） | `pipeline/retrieve.rs` 重构、`pipeline/compress.rs` 输出层调整，详见 [§12](./mempalace-diff.md#12-检索流水线三段式重构取-mem-之结构化借-mempalace-之分层) |
```

Replace with:
```markdown
| 11 | 🔍 | ✅ **检索流水线 Stage 2 拆分**（lifecycle 加性层抽出共享 helper；Stage 3 / 响应形状变更延后） | 🟠 架构升级 | M（1 天 → 实际 ~2h） | 中（依赖 #2 + #5） | `pipeline/retrieve.rs` 重构、`pipeline/compress.rs` 输出层调整，详见 [§12](./mempalace-diff.md#12-检索流水线三段式重构取-mem-之结构化借-mempalace-之分层) |
```

- [ ] **Step 4: Update mempalace-diff §12**

In `docs/mempalace-diff.md`, find the §12 section header (around line 401). Right after the header line, insert:

```markdown
> **2026-04-29 落地**：✅ 按 line 513 的 "可降级为只做 Stage 2 拆分（半天）" 路径执行。`pipeline/retrieve.rs::apply_lifecycle_score` 抽出共享 helper，三个 scorer 调它复用 lifecycle 加性层（10 个信号），行为零改动；3 个新单测覆盖 neutral / Provisional / graph 邻居场景。**Stage 3 仍未做**：响应形状变更（暴露 `score_breakdown`）、`compress.rs` 解构、A/B `MEM_RANKER=three_stage` 切换均延后到后续 PR。详见 `docs/superpowers/specs/2026-04-29-lifecycle-score-extraction-design.md`。
>
> **触发完整 §12 重构的条件**：当 `tests/search_api.rs` 出现 P95 延迟报警 OR caller 反馈 token 截断失真 OR caller-side LLM rerank 真的接进来。前两项是性能/质量信号，第三项是产品决策。
```

- [ ] **Step 5: Commit doc updates**

```bash
git add docs/ROADMAP.MD docs/mempalace-diff.md
git commit -m "docs: mark ROADMAP #11 / mempalace-diff §12 (Stage 2 split) ✅

Stage 2 lifecycle helper extracted; Stage 3 response-shape work
deferred per §12 line 513. Re-evaluate on P95 latency or caller
fidelity complaints."
```

---

## Self-Review Notes

- **Spec coverage**: audit (Task 1), helper + tests (Task 2), three scorer migrations (Tasks 3-5), close-out (Task 6).
- **TDD pattern**: Task 2 writes the helper tests before the helper. Tasks 3-5 don't write new tests because behavioral equivalence is the contract — existing tests are the proof. If they pass with zero assertion changes, the refactor is correct.
- **Type consistency**: `apply_lifecycle_score` returns `i64`. All three callers add this to a recall `i64`. The sort tiebreak chain in each scorer is unchanged.
- **No placeholders**: every step has concrete code or commands.
- **Commit cadence**: 5 commits (Task 1 doesn't commit). Each migration task ends with a green-tested commit.
- **Behavioral equivalence**: the strict invariant. If at any point an existing test fails, that's a bug in the refactor, not a permission to update assertions.
