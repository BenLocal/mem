# `compress_text` Token Counting via tiktoken — Design

> Closes ROADMAP #6 (mempalace-diff §8 #6): replace the `chars × 3` heuristic in `pipeline/compress.rs::compress_text` with real BPE token counting via `tiktoken-rs::o200k_base`, fixing the CJK over-allocation bug.

## Summary

`pipeline/compress.rs::compress_text` (lines 199–212) currently approximates a token budget by `let char_limit = limit * 3`, then truncates by character count and tries to back up to the last whitespace. This is wrong for CJK text:

1. **3× over-allocation for CJK**: BPE tokenizers emit roughly 1 token per CJK character (often less for common bigrams). `limit * 3` gives ~3× the requested token budget when the text is pure CJK.
2. **No whitespace fallback for CJK**: `rfind(char::is_whitespace)` returns `None` because CJK has no inter-character whitespace, so the function falls through to a raw char-aligned cut at the inflated limit.

Net effect: a `budget = 100` call on a Chinese paragraph returns ~300 tokens worth of content, severely overrunning the caller's budget. This violates the §8 verbatim-discipline track ("budgeted output must mean the budget it claims").

This spec replaces the body of `compress_text` with a tiktoken-rs-based implementation: encode → truncate token IDs → decode → whitespace-clean for English readability. Other functions in the file are untouched. Public API (function signature) is unchanged.

## Goals

- Replace the `chars × 3` heuristic with real BPE token counting using `tiktoken-rs::o200k_base`.
- Keep the `compress_text(text: &str, budget: usize) -> String` signature.
- Guard against CJK over-allocation: a token budget of N must produce output with ≤ N tokens (verified by re-encoding the result in tests).
- Preserve readability for English: when token truncation lands inside a word, back up to the last whitespace if it's past the halfway mark.
- Cache the tokenizer in a `OnceLock` so cold-start cost is paid once per process.
- Add 6 unit tests covering ASCII / CJK / mixed / empty / zero-budget / exact-budget cases.

## Non-Goals

- Changing the budget allocation in `compress` (the 30/35/20/15 split for directives/facts/patterns/workflow). That's separate tuning.
- Switching the tokenizer model. `o200k_base` is a deliberate choice for GPT-4o/o1 lineage; it's also closer to Claude's tokenizer than `cl100k_base` for non-English text. Other tokenizers are out of scope.
- Adding a tokenizer-warmup hook in `mem serve`. The first request will pay ~10–50ms for tokenizer init; YAGNI for now, can be added later if it shows up in latency telemetry.
- Re-tuning `max_items` (the 1/2/3/4 buckets keyed off budget). It's a separate axis and unaffected by token-vs-char counting.
- Re-tuning the budget percentages or the `+8`/`/2` constants in the call sites of `compress_text` from `compress`. Those are in token-domain already (the field calls them `directives_budget` etc.) and the caller's contract is preserved.

## Decisions (resolved during brainstorming)

- **Crate**: `tiktoken-rs`. HF `tokenizers` is already in the dep tree transitively via `embed_anything`, but it doesn't ship OpenAI BPE encodings; using it would require shipping a ~16 MB `o200k_base.tokenizer.json` resource file. tiktoken-rs has o200k_base baked in.
- **Encoding**: `o200k_base` (GPT-4o/o1 era). Slightly tighter on non-English text than `cl100k_base`, ~3 MB embedded in the crate.
- **Truncation strategy**: encode → take first N token IDs → decode → if a whitespace exists in the back half of the decoded string, back up to it. CJK paths usually skip the backstep (no whitespace).
- **No predictive trimming on the encode side**: we don't try to strip BPE merges that span a token-boundary. We trust the decoder to give us a syntactically valid prefix.
- **No new env-var knob**: the encoding choice is hard-coded. If we ever want to tune, that's a follow-up.

## Algorithm

```rust
use std::sync::OnceLock;
use tiktoken_rs::{o200k_base, CoreBPE};

fn tokenizer() -> &'static CoreBPE {
    static T: OnceLock<CoreBPE> = OnceLock::new();
    T.get_or_init(|| o200k_base().expect("o200k_base load"))
}

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

The `encode_with_special_tokens` call is the right surface for general-purpose text (it handles `<|endoftext|>` etc. as literal text, which is what we want — we're not constructing prompts here).

## Why o200k_base over cl100k_base

| Aspect | cl100k_base | o200k_base |
|---|---|---|
| Era | GPT-3.5 / GPT-4 | GPT-4o / o1 |
| Non-English compression | Looser | Tighter (~10–15% fewer tokens) |
| Vocab size | ~100 k | ~200 k |
| Embedded size | ~1 MB | ~3 MB |
| Closeness to Claude tokenizer | Off by ~10–15% | Off by ~5–10% (better fit) |

For the CJK use case driving this work, `o200k_base` matches reality more closely. The +2 MB binary cost is acceptable for a local-first service.

## File Changes

### `Cargo.toml`

Add to `[dependencies]`:

```toml
tiktoken-rs = "0.6"
```

(Or whatever the current latest minor version is at implementation time. The crate has been at 0.5–0.6 for a while.)

### `src/pipeline/compress.rs`

- Add imports: `use std::sync::OnceLock;` and `use tiktoken_rs::{o200k_base, CoreBPE};`.
- Add a private `tokenizer()` helper returning `&'static CoreBPE`, init via `OnceLock`.
- Replace the body of `compress_text` (lines 199–212) per the algorithm above. Function signature unchanged.
- Append a `#[cfg(test)] mod tests { ... }` block with the 6 unit tests below.

No other functions in this file change.

## Testing Strategy

### New unit tests in `src/pipeline/compress.rs::tests`

Each test asserts `tokenizer().encode_with_special_tokens(&result).len() <= budget` (the contract guard) plus a behavior-specific assertion. Avoid hard-coded character lengths so the tests are tokenizer-version-stable.

1. **`compress_text_ascii_within_budget`** — Input "Hello world" with `budget = 100`. Expected: original string returned (small input, no truncation).

2. **`compress_text_ascii_exceeds_budget_breaks_at_whitespace`** — Input is a long English passage (e.g., Lorem-ipsum-style, 500+ tokens worth) with `budget = 50`. Assertions:
   - `tokenizer().encode(&result).len() <= 50`
   - `result` does not end mid-word (i.e., last char is whitespace boundary OR the tail is a clean word)
   - This is verified by checking `result.ends_with(char::is_alphanumeric) → result` is suffix of an original word boundary.

3. **`compress_text_cjk_respects_token_budget`** — **The bug-fix guard.** Input is a long CJK passage (e.g., 500 Chinese characters, expected ~500 tokens with o200k_base) with `budget = 50`. Assertions:
   - `tokenizer().encode(&result).len() <= 50` (must hold; this fails on the pre-fix `chars × 3` implementation by ~3×).
   - `result` is a non-empty char-aligned prefix of the input.

4. **`compress_text_mixed_cjk_ascii`** — Input mixes CJK and ASCII (e.g., "项目 X uses HNSW for ANN queries 实现细节 ..."). `budget = 30`. Assertion: token count of result ≤ 30.

5. **`compress_text_zero_or_empty`** — Two cases:
   - `compress_text("", 100) == ""` (no panic, returns empty)
   - `compress_text("hello", 0) == ?` — current behavior is to clamp to `budget.max(8) == 8`. Preserve that. Token count of result ≤ 8.

6. **`compress_text_exact_budget`** — Pick a short input, encode it via `tokenizer()` to get its actual token count `n`. Call `compress_text(input, n)`. Assertion: result equals input verbatim (the `if tokens.len() <= limit` early-return branch — no truncation when token count is exactly at budget).

### Existing integration tests

Run unchanged. `tests/search_api.rs` and `tests/hybrid_search.rs` may have assertions sensitive to exact output text lengths from `compress`. Task 5 of the plan runs them and either:
- Confirms they still pass (most likely — they typically check field presence and ordering, not character counts).
- Updates assertions if a behavioral change shows up, with a comment referencing this spec.

## Risk Assessment

- **Cold start**: `o200k_base()` parses BPE merges at first call. Empirically ~10–50ms on modern hardware. After that, `OnceLock` returns instantly. The first `mem search` request after process start will see this overhead. Acceptable. If it becomes a problem, add a warmup call in `serve.rs::run_serve`.
- **Per-call cost**: `encode + decode` for a typical 100-token output runs in single-digit microseconds. `compress` is called once per search request (with a few internal sub-calls for steps/signals). Net cost is negligible vs. DB and embedding stages.
- **Memory**: tokenizer singleton holds ~3–5 MB resident memory for the o200k_base BPE table. Single-process residency is fine.
- **Build time**: tiktoken-rs adds `fancy-regex`, `base64`, `bstr`, `rustc-hash` to the dep tree (already present transitively in most cases). Cross-compile (release Linux x86-64) must remain green — verify before merge.
- **Cargo lock churn**: adding tiktoken-rs will rev `Cargo.lock`. Expected.
- **Reproducibility**: o200k_base is a frozen artifact in the crate; output is deterministic across runs and machines.

## Configuration

No new env vars. The choice of o200k_base is hard-coded.

## Error Handling

- `o200k_base()` returns `Result<CoreBPE>`; we `.expect("o200k_base load")` because failure here is unrecoverable (the binary shipped wrong) and would surface immediately on first call.
- `bpe.decode(...)` returns `Result<String>`; we `.unwrap_or_default()` to fall back to empty string on the (theoretical) case where token IDs don't map back. Tokens we just emitted from `encode_with_special_tokens` are by construction valid, so this is defensive only.

## Crash / Recovery

Not applicable. `compress_text` is in-memory-only and stateless.

## Out of Scope (this PR)

- Tokenizer warmup at server startup
- Switching to a different encoding (cl100k_base, p50k_base, or a HF tokenizer)
- Re-tuning budget percentages in the surrounding `compress` function
- Exposing the tokenizer to other modules (it stays private inside compress.rs)
- Counting tokens for the input *query* (separate concern; query lives elsewhere and isn't budget-bounded today)

## Verification Checklist (pre-merge)

- `cargo test -q` — all suites pass; integration test ordering changes documented in commits if any
- `cargo fmt --check` — clean
- `cargo clippy --all-targets -- -D warnings` — clean
- `cargo build --release` — clean (verifies tiktoken-rs builds in release mode)
- `cross build --release --target x86_64-unknown-linux-gnu` — clean (matches CI)
- Manual smoke: ingest a memory with long Chinese content, hit `/memories/search`, verify the output is reasonably sized (not 3× over budget)

## References

- ROADMAP.MD row #6
- mempalace-diff §8 #6 (the line being closed)
- `src/pipeline/compress.rs::compress_text` (lines 199–212 — function being rewritten)
- `tiktoken-rs` crate docs (https://docs.rs/tiktoken-rs)
- OpenAI tiktoken o200k_base spec (used by GPT-4o)
- §3 RRF design (`docs/superpowers/specs/2026-04-29-rrf-rank-fusion-design.md`) — pattern reference for tokenizer-stable test assertions
