# `compress_text` Token Counting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `chars × 3` heuristic in `pipeline/compress.rs::compress_text` with real BPE token counting via `tiktoken-rs::o200k_base`, fixing the CJK over-allocation bug while keeping the public API unchanged.

**Architecture:** Add `tiktoken-rs` as a direct dependency. Inside `compress.rs`, lazily initialize a `OnceLock<CoreBPE>` for the o200k_base encoder and rewrite `compress_text` to: encode the input, take the first `N` token IDs (where `N = budget.max(8)`), decode back to a string, and optionally back up to the last whitespace if there's one in the back half (English readability polish). Six new unit tests cover ASCII / CJK / mixed / empty / zero / exact-budget cases, each asserting that the re-encoded result respects the budget.

**Tech Stack:** Rust 2021, `tiktoken-rs = "0.6"` (latest minor at implementation time), `std::sync::OnceLock`.

**Spec:** `docs/superpowers/specs/2026-04-29-compress-text-token-budget-design.md`

---

## File Structure

**Modify only:**
- `Cargo.toml` — add tiktoken-rs dep
- `Cargo.lock` — auto-updated
- `src/pipeline/compress.rs` — rewrite `compress_text`, add tokenizer helper, add tests module

No new files. No schema changes. No CLI changes.

---

## Task 1: Add `tiktoken-rs` dependency

Pure Cargo addition. Establishes the new crate baseline before code changes.

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Find the latest tiktoken-rs version**

```bash
cargo search tiktoken-rs --limit 3
```

Expected: a line like `tiktoken-rs = "0.6.0"` or similar. Note the version. If the major has bumped (e.g., 0.7+ now), use that instead of `0.6`.

- [ ] **Step 2: Add the dependency**

In `Cargo.toml`, under `[dependencies]`, add (alphabetical placement is nice but not required):

```toml
tiktoken-rs = "0.6"
```

(Or whatever current minor was found in Step 1.)

- [ ] **Step 3: Build to update Cargo.lock**

```bash
cargo build 2>&1 | tail -20
```

Expected: clean build. The new crate downloads + compiles; the rest of the project still compiles.

- [ ] **Step 4: Verify the API we'll use is available**

```bash
cargo doc --package tiktoken-rs --no-deps --open 2>&1 | tail -5
```

Or check via a one-line script:

```bash
cat > /tmp/tiktoken_smoke.rs <<'EOF'
fn main() {
    let bpe = tiktoken_rs::o200k_base().unwrap();
    let tokens = bpe.encode_with_special_tokens("Hello world");
    println!("{} tokens", tokens.len());
}
EOF
```

(Don't run it — just confirm the symbols exist via `cargo doc`. If the function signature has changed in 0.7+, adjust later steps.)

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add tiktoken-rs for BPE token counting (ROADMAP #6)"
```

---

## Task 2: Rewrite `compress_text` + first failing test (CJK contract guard)

Core change. TDD: write the test that demonstrates the CJK bug, watch it fail under the old implementation, then rewrite the function and watch it pass.

**Files:**
- Modify: `src/pipeline/compress.rs`

- [ ] **Step 1: Add a `#[cfg(test)] mod tests` block at the bottom of `src/pipeline/compress.rs` if one doesn't exist**

Check first:
```bash
grep -n "mod tests" src/pipeline/compress.rs
```

If absent, append at end of file:
```rust
#[cfg(test)]
mod tests {
    use super::*;
}
```

- [ ] **Step 2: Write the failing CJK test**

Append inside `mod tests`:

```rust
#[test]
fn compress_text_cjk_respects_token_budget() {
    // Long Chinese paragraph; o200k_base encodes CJK at roughly 1 token/char.
    // Pre-fix `chars × 3` heuristic returns ~3× the budget for pure CJK.
    let cjk: String = "机器学习是一个广泛的研究领域涵盖了从统计学到神经网络的多种方法"
        .repeat(20);
    let budget = 50;

    let result = compress_text(&cjk, budget);

    // Re-encode the result; must respect the budget contract.
    let bpe = tokenizer();
    let result_tokens = bpe.encode_with_special_tokens(&result).len();
    assert!(
        result_tokens <= budget,
        "CJK budget violated: got {} tokens for budget {}",
        result_tokens,
        budget
    );
    assert!(!result.is_empty(), "expected nonempty truncation");
    assert!(cjk.starts_with(&result), "result must be a prefix of input");
}
```

This test calls `tokenizer()` directly, which we haven't defined yet. The test will fail to compile — that's the failure mode we want first.

- [ ] **Step 3: Run to verify failure**

```bash
cargo test --lib compress::tests::compress_text_cjk_respects_token_budget 2>&1 | tail -20
```

Expected: compile error — `tokenizer` not in scope.

- [ ] **Step 4: Add the tokenizer helper + rewrite `compress_text`**

At the top of `src/pipeline/compress.rs`, add the imports:

```rust
use std::sync::OnceLock;
use tiktoken_rs::{o200k_base, CoreBPE};
```

(Place these alongside the existing `use crate::domain::...` imports.)

Add the tokenizer helper near the bottom of the file (just above the existing `compress_text`):

```rust
fn tokenizer() -> &'static CoreBPE {
    static T: OnceLock<CoreBPE> = OnceLock::new();
    T.get_or_init(|| o200k_base().expect("o200k_base load"))
}
```

Replace the body of `compress_text` (currently lines ~199–212):

```rust
fn compress_text(text: &str, budget: usize) -> String {
    let limit = budget.max(8);
    let bpe = tokenizer();
    let tokens = bpe.encode_with_special_tokens(text);
    if tokens.len() <= limit {
        return text.to_string();
    }
    let truncated = bpe.decode(tokens[..limit].to_vec()).unwrap_or_default();
    if let Some(last_space) = truncated.rfind(char::is_whitespace) {
        if last_space > truncated.len() / 2 {
            return truncated[..last_space].trim_end().to_string();
        }
    }
    truncated
}
```

Function signature unchanged.

- [ ] **Step 5: Run the test → expect pass**

```bash
cargo test --lib compress::tests::compress_text_cjk_respects_token_budget 2>&1 | tail -20
```

Expected: PASS.

Then full lib tests:
```bash
cargo test --lib -q 2>&1 | tail -30
```

Expected: clean. (Any pre-existing tests in `compress.rs::tests` still pass since their behavior under valid budgets is unchanged.)

- [ ] **Step 6: Lint clean**

```bash
cargo fmt --all
cargo clippy --lib --tests -- -D warnings 2>&1 | tail -20
```

Fix any issues.

- [ ] **Step 7: Commit**

```bash
git add src/pipeline/compress.rs
git commit -m "feat(compress): tiktoken-based token counting for compress_text

Replaces the chars × 3 heuristic with real BPE encoding via
tiktoken-rs::o200k_base. Fixes CJK over-allocation: a budget of N
tokens now actually means ≤ N tokens in the output.

Refs ROADMAP #6"
```

---

## Task 3: Add 5 more `compress_text` unit tests

Round out coverage: ASCII within budget, ASCII exceeds budget (whitespace backstep), mixed CJK/ASCII, empty/zero, exact-budget.

**Files:**
- Modify: `src/pipeline/compress.rs`

- [ ] **Step 1: Append the tests inside `mod tests`**

```rust
#[test]
fn compress_text_ascii_within_budget() {
    let input = "Hello world";
    let result = compress_text(input, 100);
    assert_eq!(result, input);
}

#[test]
fn compress_text_ascii_exceeds_budget_breaks_at_whitespace() {
    // ~600 tokens of English; budget of 30 forces truncation.
    let input: String = "The quick brown fox jumps over the lazy dog. ".repeat(60);
    let budget = 30;

    let result = compress_text(&input, budget);

    let bpe = tokenizer();
    let result_tokens = bpe.encode_with_special_tokens(&result).len();
    assert!(
        result_tokens <= budget,
        "ASCII budget violated: got {} tokens for budget {}",
        result_tokens,
        budget
    );
    // Whitespace backstep should kick in for English: result should not
    // end mid-word. After trim_end(), the last char is a word char and the
    // input must contain "<result> " (a space immediately after) somewhere.
    let space_marker = format!("{} ", result.trim_end());
    assert!(
        input.contains(&space_marker) || input.ends_with(result.trim_end()),
        "result should end at a word boundary"
    );
}

#[test]
fn compress_text_mixed_cjk_ascii() {
    let input: String = "项目 X uses HNSW for ANN queries 实现细节: see vector_index.rs. ".repeat(20);
    let budget = 25;

    let result = compress_text(&input, budget);

    let bpe = tokenizer();
    let result_tokens = bpe.encode_with_special_tokens(&result).len();
    assert!(
        result_tokens <= budget,
        "mixed budget violated: got {} tokens for budget {}",
        result_tokens,
        budget
    );
    assert!(input.starts_with(&result), "result must be a prefix of input");
}

#[test]
fn compress_text_zero_or_empty() {
    // Empty input never panics, returns empty.
    assert_eq!(compress_text("", 100), "");

    // budget=0 clamps to budget.max(8) == 8 internally; the result must
    // still respect that effective budget.
    let result = compress_text("hello world this is a longer test sentence", 0);
    let bpe = tokenizer();
    let tokens = bpe.encode_with_special_tokens(&result).len();
    assert!(tokens <= 8, "budget=0 must clamp to 8; got {} tokens", tokens);
}

#[test]
fn compress_text_exact_budget() {
    // Pick a short input, measure its exact token count, then call with that as budget.
    let input = "Hello, world!";
    let bpe = tokenizer();
    let n = bpe.encode_with_special_tokens(input).len();

    let result = compress_text(input, n);
    assert_eq!(result, input, "no truncation when token count == budget");
}
```

- [ ] **Step 2: Run all compress tests**

```bash
cargo test --lib compress::tests -q 2>&1 | tail -20
```

Expected: 6 tests pass total (`_cjk_respects_token_budget` from Task 2 plus the 5 new ones).

- [ ] **Step 3: Lint clean**

```bash
cargo fmt --all
cargo clippy --lib --tests -- -D warnings 2>&1 | tail -20
```

- [ ] **Step 4: Commit**

```bash
git add src/pipeline/compress.rs
git commit -m "test(compress): ASCII / mixed / boundary cases for token budget"
```

---

## Task 4: Run integration suite

Verify no integration tests broke from the behavioral change. The functional contract change is "CJK output is now ~3× shorter than before" — this could shift assertions that compare exact output strings.

**Files:**
- Possibly modify: `tests/search_api.rs`, `tests/hybrid_search.rs`, or other integration tests

- [ ] **Step 1: Run search_api**

```bash
cargo test --test search_api -q 2>&1 | tail -30
```

If all pass: continue to Step 2. If some fail: jump to Step 4.

- [ ] **Step 2: Run hybrid_search**

```bash
cargo test --test hybrid_search -q 2>&1 | tail -30
```

If all pass: continue to Step 3.

- [ ] **Step 3: Run full suite**

```bash
cargo test -q 2>&1 | tail -50
```

If all pass: skip to Task 5.

- [ ] **Step 4: For each failure, investigate**

For each failing assertion:

1. Read what the test asserts and what the new value is.
2. If the failure is because output got *shorter* (the fix is working — old behavior over-allocated), and the new output is still semantically valid (correct memory IDs surfaced, correct sections populated), update the assertion. Add a short comment referencing this spec.
3. If the failure is something else (test infra broke, panic, wrong section, missing field), debug — do not just rubber-stamp.

When updating an assertion, prefer to assert structural properties (length ≤ N, contains substring) over exact string equality. Hard-coded strings are fragile; structural assertions survive future tokenizer minor-version changes.

- [ ] **Step 5: Commit any test updates**

```bash
git add tests/
git commit -m "test: update fixture lengths for tiktoken-based compress_text

The old chars × 3 heuristic over-allocated CJK by ~3×; now token
budgets are honored exactly. Updated assertions to match.

Refs ROADMAP #6"
```

If no test changes were needed, skip this commit.

---

## Task 5: Release-build smoke + final verification

Confirm tiktoken-rs builds cleanly in release mode and that no cross-build regressions occur.

**Files:**
- None (verification only)

- [ ] **Step 1: Release build**

```bash
cargo build --release 2>&1 | tail -10
```

Expected: clean. tiktoken-rs's `bstr` and `fancy-regex` deps must compile under `--release`.

- [ ] **Step 2: Cross build (if Cross.toml exists and has been the CI baseline)**

```bash
cross build --release --target x86_64-unknown-linux-gnu 2>&1 | tail -20
```

Expected: clean. If this fails with a numkong-like AVX-512 issue (similar to ROADMAP #3's experience), check whether tiktoken-rs has any feature flag we need to disable. Most likely it's just fine — tiktoken-rs has no SIMD assumptions.

If the cross-build infra isn't usable in this environment, mark this step as deferred to CI and continue.

- [ ] **Step 3: Final fmt + clippy**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: clean.

- [ ] **Step 4: Mark ROADMAP #6 complete**

In `docs/ROADMAP.MD`, change row 6 from:

```markdown
| 6 | 📦 | `compress_text` 改 token 计数（CJK 不再按词裸奔截断）| 🟡 输出 verbatim 纪律 | S（2h） | 低 | `pipeline/compress.rs` |
```

to:

```markdown
| 6 | 📦 | ✅ `compress_text` 改 token 计数（tiktoken-rs::o200k_base，CJK 不再按词裸奔截断）| 🟡 输出 verbatim 纪律 | S（2h） | 低 | `pipeline/compress.rs` |
```

In `docs/mempalace-diff.md`, find the §8 #6 entry (search for "compress_text" or "token 计数"). Append a "**2026-04-29 落地**：✅ ..." note describing what landed (one short paragraph: o200k_base, OnceLock cache, 6 unit tests, kill-switch not needed, no schema changes).

- [ ] **Step 5: Commit doc updates**

```bash
git add docs/ROADMAP.MD docs/mempalace-diff.md
git commit -m "docs: mark ROADMAP #6 / mempalace-diff §8 #6 (compress_text tokenization) ✅"
```

- [ ] **Step 6: Sanity manual smoke (optional but recommended)**

In a separate terminal, ingest a long Chinese memory and search:

```bash
# Terminal 1
cargo run -- serve

# Terminal 2
curl -X POST http://127.0.0.1:3000/memories/ingest \
  -H 'Content-Type: application/json' \
  -d '{"tenant":"test","memory_type":"implementation","summary":"测试","content":"机器学习是一个广泛的研究领域涵盖了从统计学到神经网络的多种方法。深度学习作为机器学习的子集近年来取得了显著进展，尤其是在自然语言处理和计算机视觉领域。当前主流的预训练模型包括 BERT、GPT 系列以及视觉相关的 ViT 等。","scope":"global","caller_agent":"smoke"}'

curl -X POST http://127.0.0.1:3000/memories/search \
  -H 'Content-Type: application/json' \
  -d '{"tenant":"test","query":"机器学习","token_budget":80,"caller_agent":"smoke","scope_filters":[]}' \
  | jq '.relevant_facts[].text | length'
```

Expected: response field text lengths are reasonable (in chars, single-digit hundreds for budget 80, not ~1000+ as the old heuristic would produce).

This step is optional — if you don't have a way to run the server in your environment, skip it.

---

## Self-Review Notes

- **Spec coverage**: Every spec section maps to a task. The 6 unit tests are split as 1 in Task 2 (the bug-fix guard, written first per TDD) + 5 in Task 3.
- **Type consistency**: `tokenizer()` returns `&'static CoreBPE`; the test references it via `super::tokenizer()` (importable through `use super::*;` inside `mod tests`).
- **Decisions resolved**: tiktoken-rs (not HF tokenizers), o200k_base (not cl100k), encode-decode-with-whitespace-backstep (策略 1).
- **No placeholders**: every step has concrete code or commands.
- **Each task produces a committable, testable change** suitable for review checkpoints.
