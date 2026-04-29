# Verbatim Guard for Ingest — Design

> Closes ROADMAP #9 (mempalace-diff §7 建议 #1): make the verbatim discipline machine-enforced. Caller-supplied `summary` (when provided) must differ from `content`. Replaces a dead `validate_verbatim` whose `summary.len() > 80` branch was unreachable.

## Summary

`pipeline/ingest.rs::validate_verbatim` exists and is wired into `service/memory_service.rs::ingest`, but its current condition (`!summary.is_empty() && summary.len() > 80 && summary == request.content`) is unreachable: the server-side `summarize()` always clamps the derived summary to ≤ 80 chars, so `summary.len() > 80` is impossible.

`IngestMemoryRequest` doesn't carry a caller-supplied `summary` field today — `summary` is auto-derived from `content[:80]`. This makes the verbatim rule ("agents must not copy refined/summarized text into the `content` field") un-enforceable because the system has no signal to compare against.

This spec adds an **optional** `summary: Option<String>` field on `IngestMemoryRequest`. When the caller provides it, the system validates `caller_summary != content` before persisting. When the caller doesn't provide it (or sends empty string), the server falls back to `summarize(content)` exactly as today — no behavior change for existing callers.

## Goals

- Add `summary: Option<String>` to `IngestMemoryRequest`. Default `None` for backwards compatibility.
- Rewrite `validate_verbatim` with a real, reachable check: reject ingest when the caller supplied `summary` exactly equals `content`.
- Wire the new validation into `memory_service::ingest` before record construction. Validation happens BEFORE summary fallback so a caller-supplied violation is rejected unambiguously.
- When caller supplies a non-empty summary, store *that* as `MemoryRecord.summary`. When caller doesn't (or sends empty), fall back to `summarize(content)` (preserves existing behavior).
- Add 4 unit tests in `pipeline/ingest.rs::tests` covering: no caller summary, empty caller summary, caller summary differs from content, caller summary equals content.
- Add 2 integration tests covering the HTTP path: 400 InvalidInput for matching summary/content; 200 + correct stored summary for non-matching.
- Touch up the verbatim rule wording in AGENTS.md and README.md to clarify that the rule applies specifically to caller-provided summary.

## Non-Goals

- Re-tuning the `SUMMARY_LIMIT = 80` clamp (still applies to server-derived summaries when caller doesn't supply one).
- Validating server-derived summaries (they're trivially `content[:80]`, comparing to content is meaningless when content < 80 chars).
- Changing `compute_content_hash` to include summary (summary is index/hint only — keep it out of the canonical hash so the same content with different summaries dedupes correctly).
- Forbidding short content. Legitimate short verbatim memories ("Always use UUIDv7 for IDs.") must still ingest cleanly when no summary is supplied.
- Updating MCP tool descriptions beyond what `schemars` derives automatically. Tool schema gains a `summary` field passively.
- Changing the API return shape or HTTP status codes for non-violation paths.

## Decisions (resolved during brainstorming)

- **API surface**: extend the existing request type rather than introducing a new endpoint. `summary` is optional and skipped from JSON when `None`.
- **Validation site**: in `memory_service::ingest` before constructing `MemoryRecord`, mapping a `validate_verbatim` `Err` to `ServiceError::Storage(StorageError::InvalidInput(...))` (matches the existing pattern at line 138).
- **Validation signature**: `fn validate_verbatim(content: &str, caller_summary: Option<&str>) -> Result<(), String>`. Drops the request reference (we only need content) and makes the optional nature of summary explicit.
- **Empty string treatment**: `caller_summary = Some("")` is normalized to "no summary supplied" — no validation, fallback to derive. Matches HTTP/JSON convention where empty string and absent field often have the same intent.
- **Hash stability**: `compute_content_hash` remains untouched; summary is not part of the canonical request hash.
- **Docs**: AGENTS.md and README.md already document the verbatim rule. They mention "agents must not copy refined/summarized text into the `content` field"; we'll add one short clause clarifying that providing an explicit `summary` field that equals `content` is the machine-enforced violation.

## Algorithm

### `pipeline/ingest.rs::validate_verbatim` (new body)

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

### `service/memory_service.rs::ingest` integration

Replace the current block (around lines 135–138):
```rust
let summary = summarize(&request.content);

crate::pipeline::ingest::validate_verbatim(&request, &summary)
    .map_err(|e| ServiceError::Storage(StorageError::InvalidInput(e)))?;
```
with:
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

This validates first (using the caller's summary if any), then derives the stored summary — caller's value if provided & non-empty, otherwise fallback.

### `domain/memory.rs::IngestMemoryRequest` field addition

Insert between `content` and `evidence`:
```rust
pub content: String,
#[serde(skip_serializing_if = "skip_none")]
pub summary: Option<String>,
pub evidence: Vec<String>,
```

## Field Layout (after change)

```rust
pub struct IngestMemoryRequest {
    pub tenant: String,
    pub memory_type: MemoryType,
    pub content: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub summary: Option<String>,
    pub evidence: Vec<String>,
    pub code_refs: Vec<String>,
    pub scope: Scope,
    pub visibility: Visibility,
    #[serde(skip_serializing_if = "skip_none")]
    pub project: Option<String>,
    // ... rest unchanged ...
}
```

## Testing Strategy

### Unit tests (`src/pipeline/ingest.rs::tests`)

Add a `#[cfg(test)] mod tests { use super::*; ... }` block at the bottom (one doesn't exist yet in this file).

1. **`validate_verbatim_no_caller_summary_ok`** — `validate_verbatim("any content", None)` returns `Ok(())`.
2. **`validate_verbatim_empty_caller_summary_ok`** — `validate_verbatim("any content", Some(""))` returns `Ok(())` (normalized to None).
3. **`validate_verbatim_caller_summary_differs_ok`** — `validate_verbatim("hello world", Some("greeting"))` returns `Ok(())`.
4. **`validate_verbatim_caller_summary_equals_content_rejected`** — `validate_verbatim("the same text", Some("the same text"))` returns `Err(_)`. Assert the error message mentions "verbatim".

### Integration tests (`tests/ingest_verbatim_guard.rs` — new file)

A small new integration test file. Pattern follows existing tests like `tests/search_api.rs` (axum + ephemeral DuckDB). Two cases:

1. **`ingest_rejects_summary_equals_content`** — POST `/memories` with body that includes both `content` and `summary` set to identical strings. Expect HTTP 400 (or whatever `InvalidInput` maps to in the axum layer; verify by reading existing 400 handling). Assert response body contains the verbatim error.

2. **`ingest_accepts_caller_summary_and_stores_it`** — POST `/memories` with `content = "long verbatim text..."`, `summary = "shorter caller hint"`. Expect 200 with memory_id. GET `/memories/{id}` and assert `summary` field equals `"shorter caller hint"` (NOT `summarize(content) = content[:80]`).

If the existing test infra makes a new file too heavy, append the cases to `tests/search_api.rs` instead — that's a known acceptable pattern (it's already 514 LOC mixing concerns).

### Existing tests

Run unchanged. All existing ingest paths don't supply `summary`, so the `summary: Option<String>` defaults to `None` (via `serde::Deserialize`) and the new code branch matches the old `summarize(content)` behavior exactly.

## Documentation Updates

### `AGENTS.md` (line 51)

Current: "The ingest pipeline enforces that `summary` must not be identical to `content` (agents must not copy refined/summarized text into the `content` field)."

Update to: "The ingest pipeline enforces that, when a caller provides an explicit `summary` field, it must not be identical to `content` — agents must not copy refined/summarized text into the `content` field. When no summary is supplied, the server derives one from `content[:80]` for indexing purposes only."

### `README.md` (line 143)

Same wording update applied to the README's Verbatim discipline bullet, adapted for the slightly different surrounding tone.

## Risk Assessment

- **Schema impact**: `IngestMemoryRequest` gains a field. JSON callers that don't send `summary` continue to work (serde defaults `Option<String>` to `None`). No DB migration needed (the persistence path uses `MemoryRecord.summary: String`, populated either from caller or from `summarize`).
- **MCP tool schema**: `schemars` auto-derives the schema. Tools like `mempalace_*` (if any forward to mem) will see the new optional field appear. Existing JSON tool calls with no `summary` keep working.
- **Idempotency**: `compute_content_hash` does NOT include summary (verified by reading `canonical_request_json`). Two requests with the same content but different summaries hash to the same key and dedupe — that's correct behavior (summary is index/hint, not identity).
- **Test fragility**: existing integration tests don't pass `summary`, so they take the fallback path with no behavior change. New tests added in their own file (or appended to an existing one) without disturbing other assertions.
- **Backwards compatibility**: full. Old clients work; new clients gain enforcement.

## Configuration

No new env vars. The validation is unconditional (no kill switch — the rule is documented as part of the design discipline; if a caller wants to bypass, they simply don't supply `summary`).

## Error Handling

- `validate_verbatim` returns `Result<(), String>`; the caller (`memory_service::ingest`) maps the error to `ServiceError::Storage(StorageError::InvalidInput(...))` matching the existing pattern.
- The HTTP layer turns `InvalidInput` into a 400-class response (verify by reading `service/error.rs` or wherever the mapping lives — needed for the integration test's status assertion).

## Crash / Recovery

Not applicable. `validate_verbatim` is pure and stateless.

## Out of Scope (this PR)

- Validating short verbatim content (no length floor)
- Including summary in `compute_content_hash`
- A summary-required mode (e.g., env var `MEM_REQUIRE_CALLER_SUMMARY=1`)
- Changes to MCP tool descriptions beyond auto-derived schema

## Verification Checklist (pre-merge)

- `cargo test -q` — all suites pass; new unit + integration tests included
- `cargo fmt --check` — clean
- `cargo clippy --all-targets -- -D warnings` — clean
- `cargo build --release` — clean
- Manual smoke (optional): `cargo run -- serve` then POST `/memories` with `summary == content` and confirm 400 response

## References

- ROADMAP.MD row #9
- mempalace-diff §7 建议 #1 (the line being closed)
- `src/pipeline/ingest.rs::validate_verbatim` (lines 13–20 — function being rewritten)
- `src/service/memory_service.rs::ingest` (lines ~135–138 — caller adjustment)
- `src/domain/memory.rs::IngestMemoryRequest` (lines 105–126 — schema change)
- `AGENTS.md` line 51, `README.md` line 143 — doc touchpoints
