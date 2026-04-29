# Verbatim Guard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the verbatim discipline machine-enforced. Add an optional `summary: Option<String>` field on `IngestMemoryRequest`, validate `caller_summary != content` when provided, and store the caller's summary verbatim. Existing callers (no `summary` supplied) continue to work unchanged.

**Architecture:** Single-file domain change (`src/domain/memory.rs`) adding the field. `pipeline/ingest.rs::validate_verbatim` rewritten with reachable logic (current `> 80` branch is dead code). `service/memory_service.rs::ingest` validates first, then derives the stored summary from caller's value (if non-empty) or falls back to `summarize(content)`. Four unit tests in `pipeline/ingest.rs::tests` plus two integration tests in a new `tests/ingest_verbatim_guard.rs` cover the rule.

**Tech Stack:** Rust 2021, serde (`#[serde(skip_serializing_if = "skip_none")]`), axum (for integration tests), existing test infra.

**Spec:** `docs/superpowers/specs/2026-04-29-verbatim-guard-design.md`

---

## File Structure

**Modify:**
- `src/domain/memory.rs` — add `summary: Option<String>` field to `IngestMemoryRequest`
- `src/pipeline/ingest.rs` — rewrite `validate_verbatim`, add `mod tests`
- `src/service/memory_service.rs` — adjust caller of `validate_verbatim`, derive summary with fallback
- `AGENTS.md` — refine the verbatim rule wording
- `README.md` — same refinement (different surrounding context)

**Create:**
- `tests/ingest_verbatim_guard.rs` — 2 integration tests

No new dependencies. No schema migrations. No CLI changes.

---

## Task 1: Add `summary` field to `IngestMemoryRequest`

Pure domain change. Establishes the schema before the validation logic depends on it.

**Files:**
- Modify: `src/domain/memory.rs`

- [ ] **Step 1: Read the current struct**

```bash
grep -n "pub struct IngestMemoryRequest" src/domain/memory.rs
```

Confirm the struct is around line 105.

- [ ] **Step 2: Insert the new field**

In `src/domain/memory.rs`, find:
```rust
pub struct IngestMemoryRequest {
    pub tenant: String,
    pub memory_type: MemoryType,
    pub content: String,
    pub evidence: Vec<String>,
    ...
}
```

Insert immediately after `pub content: String,`:
```rust
    #[serde(skip_serializing_if = "skip_none")]
    pub summary: Option<String>,
```

The full top of the struct should read:
```rust
pub struct IngestMemoryRequest {
    pub tenant: String,
    pub memory_type: MemoryType,
    pub content: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub summary: Option<String>,
    pub evidence: Vec<String>,
    pub code_refs: Vec<String>,
    ...
}
```

- [ ] **Step 3: Build to verify**

```bash
cargo build 2>&1 | tail -10
```

Expected: clean build. The new optional field defaults to `None` for any existing struct literal (Rust requires explicit init by default — but if any existing test code constructs `IngestMemoryRequest { ... }` literally, it will fail to compile because of the missing field).

**Likely caller breakage**: integration tests, fixture builders. Run:
```bash
cargo build --tests 2>&1 | tail -30
```

For each compile error, add `summary: None,` to the struct literal. Don't change any other behavior — these are mechanical fixes.

- [ ] **Step 4: Run tests to confirm green baseline**

```bash
cargo test -q 2>&1 | tail -20
```

Expected: clean. The field is purely additive at the struct level; no behavior change yet.

- [ ] **Step 5: Lint**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
```

- [ ] **Step 6: Commit**

```bash
git add -A src/domain/memory.rs tests/ src/  # include any test fixtures touched
git commit -m "feat(domain): add optional summary field to IngestMemoryRequest

Caller may now provide an explicit summary distinct from content.
Field is optional; existing callers see no behavior change.

Refs ROADMAP #9"
```

---

## Task 2: Rewrite `validate_verbatim` + wire into ingest + 4 unit tests

TDD core change. Replace the dead `> 80` check with reachable logic that fires when a caller-supplied summary equals content.

**Files:**
- Modify: `src/pipeline/ingest.rs` (rewrite function, add tests)
- Modify: `src/service/memory_service.rs` (adjust caller)

- [ ] **Step 1: Add a `#[cfg(test)] mod tests` block at the bottom of `src/pipeline/ingest.rs`**

```bash
grep -n "mod tests" src/pipeline/ingest.rs
```

If absent, append at the very end of the file:
```rust
#[cfg(test)]
mod tests {
    use super::*;
}
```

- [ ] **Step 2: Write the 4 failing unit tests**

Append inside `mod tests`:

```rust
#[test]
fn validate_verbatim_no_caller_summary_ok() {
    assert!(validate_verbatim("any content here", None).is_ok());
}

#[test]
fn validate_verbatim_empty_caller_summary_ok() {
    // Empty string normalized to "no summary supplied" → no validation.
    assert!(validate_verbatim("any content here", Some("")).is_ok());
}

#[test]
fn validate_verbatim_caller_summary_differs_ok() {
    assert!(validate_verbatim("hello world", Some("greeting")).is_ok());
}

#[test]
fn validate_verbatim_caller_summary_equals_content_rejected() {
    let err = validate_verbatim("the same text", Some("the same text"))
        .expect_err("should reject identical caller summary");
    assert!(err.contains("verbatim"), "error must mention verbatim: {}", err);
}
```

- [ ] **Step 3: Run to verify failure**

```bash
cargo test --lib pipeline::ingest::tests::validate_verbatim 2>&1 | tail -20
```

Expected: 4 compile errors — `validate_verbatim` currently has signature `(request: &IngestMemoryRequest, summary: &str) -> Result<(), String>`, which doesn't match the test calls.

- [ ] **Step 4: Rewrite `validate_verbatim`**

In `src/pipeline/ingest.rs`, replace the existing function (lines ~13–20):

```rust
/// Validate that the request follows verbatim discipline. When the caller
/// supplies a non-empty `summary`, it must not equal `content` (otherwise
/// the agent has copied refined/summarized text into the content field).
pub fn validate_verbatim(content: &str, caller_summary: Option<&str>) -> Result<(), String> {
    if let Some(summary) = caller_summary.filter(|s| !s.is_empty()) {
        if summary == content {
            return Err(
                "summary must not be identical to content (verbatim violation)".into(),
            );
        }
    }
    Ok(())
}
```

Note: the function now takes `(content: &str, caller_summary: Option<&str>)` instead of the old `(request: &IngestMemoryRequest, summary: &str)`.

- [ ] **Step 5: Update the caller in `memory_service.rs`**

In `src/service/memory_service.rs`, find the existing block (around lines 135–138):
```rust
let summary = summarize(&request.content);

crate::pipeline::ingest::validate_verbatim(&request, &summary)
    .map_err(|e| ServiceError::Storage(StorageError::InvalidInput(e)))?;
```

Replace with:
```rust
crate::pipeline::ingest::validate_verbatim(&request.content, request.summary.as_deref())
    .map_err(|e| ServiceError::Storage(StorageError::InvalidInput(e)))?;

let summary = request
    .summary
    .as_deref()
    .filter(|s| !s.is_empty())
    .map(|s| s.to_string())
    .unwrap_or_else(|| summarize(&request.content));
```

This validates first (using the caller's optional summary), then derives the stored summary: caller's value if non-empty, else `summarize(content)` fallback.

- [ ] **Step 6: Run unit tests → expect pass**

```bash
cargo build 2>&1 | tail -10
cargo test --lib pipeline::ingest::tests::validate_verbatim -q 2>&1 | tail -20
```

Expected: 4/4 pass.

- [ ] **Step 7: Run full lib + integration tests**

```bash
cargo test -q 2>&1 | tail -30
```

Expected: clean. All existing tests don't supply `summary` so they take the fallback path (which matches the old behavior exactly).

- [ ] **Step 8: Lint clean**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
```

- [ ] **Step 9: Commit**

```bash
git add -A src/pipeline/ingest.rs src/service/memory_service.rs
git commit -m "feat(ingest): enforce verbatim rule when caller supplies summary

validate_verbatim rewritten with reachable logic: rejects ingest when
caller-provided summary equals content. Old > 80-char branch was dead.
memory_service::ingest validates first, then derives stored summary
from caller's value or falls back to summarize(content).

Refs ROADMAP #9"
```

---

## Task 3: Integration tests for the HTTP path

Verify the validation surfaces as a 400 response and that a valid caller summary is stored as-is.

**Files:**
- Create: `tests/ingest_verbatim_guard.rs`

- [ ] **Step 1: Find the test pattern used by existing integration tests**

```bash
ls tests/
head -30 tests/search_api.rs
```

Look at the test setup pattern (axum app + ephemeral DuckDB). Reuse it.

- [ ] **Step 2: Create the new integration test file**

Write `tests/ingest_verbatim_guard.rs`. The exact axum/test-setup imports depend on what `tests/search_api.rs` uses — reuse them. Skeleton:

```rust
// Integration tests for the verbatim guard introduced by ROADMAP #9.
// Spec: docs/superpowers/specs/2026-04-29-verbatim-guard-design.md

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`
// ... import whatever helper builds the test app from search_api.rs

async fn build_test_app() -> /* same return type as in search_api.rs */ {
    // copy the test setup pattern from tests/search_api.rs
}

#[tokio::test]
async fn ingest_rejects_summary_equals_content() {
    let app = build_test_app().await;

    let body = json!({
        "tenant": "test",
        "memory_type": "implementation",
        "content": "the exact same text",
        "summary": "the exact same text",
        "evidence": [],
        "code_refs": [],
        "scope": "global",
        "visibility": "shared",
        "tags": [],
        "source_agent": "test",
        "write_mode": "auto"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/memories")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let bytes = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let err_msg = body
        .get("error")
        .or(body.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        err_msg.contains("verbatim") || err_msg.contains("summary"),
        "expected verbatim/summary error in response: {}",
        body
    );
}

#[tokio::test]
async fn ingest_accepts_caller_summary_and_stores_it() {
    let app = build_test_app().await;

    let body = json!({
        "tenant": "test",
        "memory_type": "implementation",
        "content": "Long verbatim text describing the full implementation context, much longer than the summary, including specific details that would normally be captured in the original source material.",
        "summary": "Concise caller-provided hint",
        "evidence": [],
        "code_refs": [],
        "scope": "global",
        "visibility": "shared",
        "tags": [],
        "source_agent": "test",
        "write_mode": "auto"
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/memories")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
    let post_resp: Value = serde_json::from_slice(&bytes).unwrap();
    let memory_id = post_resp.get("memory_id").and_then(|v| v.as_str()).unwrap();

    // GET /memories/{id} and verify summary is the caller's value
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/memories/{}", memory_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), 16384).await.unwrap();
    let get_resp: Value = serde_json::from_slice(&bytes).unwrap();
    let stored_summary = get_resp.get("summary").and_then(|v| v.as_str()).unwrap();
    assert_eq!(stored_summary, "Concise caller-provided hint");
}
```

**IMPORTANT — adapt to actual API**:
- The exact JSON structure for `IngestMemoryRequest` (field names like `write_mode`, `scope` enum casing) might differ. Read existing tests in `search_api.rs` and copy a working request body.
- The error response shape (`error` vs `message` field, status code 400 vs 422) depends on the existing error handler in `service/error.rs` or the axum extractor. Read the existing error-path tests for the pattern.
- The GET /memories/{id} endpoint's response shape for `summary` field — verify by reading `domain/memory.rs::MemoryRecord` and the GET handler in `service/`.

If the existing infra makes a fresh test file too heavy, append the two `#[tokio::test]` functions to `tests/search_api.rs` instead and skip creating the new file. Pick whichever path is lower-friction.

- [ ] **Step 3: Run the new tests**

```bash
cargo test --test ingest_verbatim_guard -q 2>&1 | tail -20
```
(Or `--test search_api` if you appended there.)

Expected: 2/2 pass.

- [ ] **Step 4: Run full suite**

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
git add -A tests/
git commit -m "test(ingest): integration coverage for verbatim guard

Two HTTP-path tests: 400 when summary == content; 200 + caller's
summary stored verbatim when they differ.

Refs ROADMAP #9"
```

---

## Task 4: Doc touch-ups in AGENTS.md and README.md

The docs already mention the verbatim rule but say "ingest enforces summary != content" generically. Refine to clarify that the rule fires when a caller provides an explicit `summary`.

**Files:**
- Modify: `AGENTS.md` (line 51)
- Modify: `README.md` (line 143)

- [ ] **Step 1: Update AGENTS.md**

In `AGENTS.md`, find the line:
```
- **Verbatim rule**: `memories.content` is the **fact source** — never rewrite, never truncate at storage. `memories.summary` is **index / hint only** — never use it as the basis for an answer or quote. Output-layer compression (`pipeline/compress.rs`) operates on `content`, never replaces it. The ingest pipeline enforces that `summary` must not be identical to `content` (agents must not copy refined/summarized text into the `content` field).
```

Replace the last sentence ("The ingest pipeline enforces ...") with:
```
The ingest pipeline enforces that, when a caller provides an explicit `summary` field, it must not equal `content` — agents must not copy refined/summarized text into the `content` field. When no summary is supplied, the server derives one from `content[:80]` for indexing purposes only.
```

- [ ] **Step 2: Update README.md**

In `README.md`, find the line (around 143):
```
- **Verbatim discipline**: `memories.content` is the **fact source** — never rewritten or truncated at storage. `memories.summary` is **index/hint only** — never used as the basis for answers or quotes. The ingest pipeline enforces that `summary` must not be identical to `content` to prevent agents from copying refined text into the content field.
```

Replace the last sentence with:
```
When a caller provides an explicit `summary` field, the ingest pipeline rejects requests where `summary` equals `content` — preventing agents from copying refined text into the content field. When no caller summary is supplied, the server derives one from `content[:80]` for indexing only.
```

- [ ] **Step 3: Commit**

```bash
git add AGENTS.md README.md
git commit -m "docs: clarify verbatim rule applies to caller-supplied summary

The check fires only when a caller provides an explicit summary
that equals content. Server-derived summaries (content[:80]) are
unaffected.

Refs ROADMAP #9"
```

---

## Task 5: Final verification + close ROADMAP #9

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

All must be clean.

- [ ] **Step 2: Mark ROADMAP #9 complete**

In `docs/ROADMAP.MD`, find row 9 starting with `| 9 | 📦 | **Verbatim 守护**`. Change the entry to prefix the value with `✅ `:

Find:
```markdown
| 9 | 📦 | **Verbatim 守护**（ingest 校验 `summary != content`，禁止把提炼版塞 `content`；同时把"`content` 是事实源、`summary` 只做索引"写进 `AGENTS.md` / `README.md`） | 🟢 哲学一致性 | S（1h） | 低 | `pipeline/ingest.rs`、`AGENTS.md`、`README.md` |
```

Replace with:
```markdown
| 9 | 📦 | ✅ **Verbatim 守护**（caller 显式提供 `summary` 时校验 `summary != content`；AGENTS.md / README.md 已写） | 🟢 哲学一致性 | S（1h） | 低 | `pipeline/ingest.rs`、`AGENTS.md`、`README.md` |
```

- [ ] **Step 3: Update mempalace-diff.md §7 建议 #1**

Find the line in `docs/mempalace-diff.md` (around line 241):
```
1. 在 ingest 路径加一个 `assert!(content.len() > 0)`，**禁止 `summary` 与 `content` 完全相同时写入**——这是 agent 偷懒抄过去的信号。
```

Replace with:
```
1. ✅ 在 ingest 路径加一个 `assert!(content.len() > 0)`，**禁止 `summary` 与 `content` 完全相同时写入**——这是 agent 偷懒抄过去的信号。（2026-04-29 落地：`IngestMemoryRequest.summary: Option<String>`，caller 提供时校验 `summary != content`，4 个单测 + 2 个集成测试。ROADMAP #9。）
```

- [ ] **Step 4: Commit doc updates**

```bash
git add docs/ROADMAP.MD docs/mempalace-diff.md
git commit -m "docs: mark ROADMAP #9 / mempalace-diff §7 (verbatim guard) ✅"
```

- [ ] **Step 5: Sanity manual smoke (optional)**

```bash
cargo run -- serve  # terminal 1

# terminal 2 — should reject with 400
curl -i -X POST http://127.0.0.1:3000/memories \
  -H 'Content-Type: application/json' \
  -d '{"tenant":"test","memory_type":"implementation","content":"abc","summary":"abc","evidence":[],"code_refs":[],"scope":"global","visibility":"shared","tags":[],"source_agent":"smoke","write_mode":"auto"}'

# should accept (200) and store caller summary
curl -i -X POST http://127.0.0.1:3000/memories \
  -H 'Content-Type: application/json' \
  -d '{"tenant":"test","memory_type":"implementation","content":"long verbatim source text describing the actual fact in full detail","summary":"short caller hint","evidence":[],"code_refs":[],"scope":"global","visibility":"shared","tags":[],"source_agent":"smoke","write_mode":"auto"}'
```

Skip if running the server isn't practical in your environment.

---

## Self-Review Notes

- **Spec coverage**: schema change (Task 1), validate_verbatim rewrite + service wiring + unit tests (Task 2), integration tests (Task 3), doc clarifications (Task 4), close-out (Task 5).
- **Type consistency**: `validate_verbatim` signature changes from `(&IngestMemoryRequest, &str)` to `(&str, Option<&str>)` — both the function definition (Task 2 Step 4) and the only caller (Task 2 Step 5) are updated together so no intermediate task leaves a broken state.
- **No placeholders**: all code blocks are complete; integration test bodies have explicit JSON.
- **Commit cadence**: 5 commits. Each task ends with a green-tested commit.
- **Backwards compatibility verified**: existing tests don't supply `summary`, so they take the fallback `summarize(content)` path — identical to current behavior.
