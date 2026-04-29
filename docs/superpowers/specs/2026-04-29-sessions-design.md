# Sessions: Auto-bucketed Time Containers — Design

> Closes ROADMAP #10 (mempalace-diff §11). Adds a `sessions` table and per-`(tenant, caller_agent)` auto-bucket on ingest. Each memory carries a `session_id` linking it to the time-window it was captured in.

## Summary

mem currently has no first-class concept of a "work session" or "conversation window." Time queries lean on `created_at` / `updated_at` only, which makes "what did I work on this morning?" or "delete the broken session I just had" impossible to answer cleanly.

This spec adds:

1. A `sessions` table keyed on `(tenant, caller_agent)` with `started_at` / `last_seen_at` / `ended_at` lifecycle.
2. An `alter table memories add column session_id` migration.
3. Server-side **auto-bucket** logic on ingest: continue the existing active session if its last activity was < `idle_minutes` ago; otherwise close it and open a new one.
4. A small repository surface (`latest_active_session`, `open_session`, `close_session`, `touch_session`) the service layer composes into `resolve_session`.
5. Configurable idle threshold via `MEM_SESSION_IDLE_MINUTES` (default 30).

Out of scope for this PR: HTTP endpoints (`GET /sessions`, `GET /sessions/{id}`, `DELETE /sessions/{id}`), `episodes.session_id` extension, soft-delete via `supersedes_memory_id`. These are deferred to follow-up work and tracked in mempalace-diff §11 once this lands.

## Goals

- New table `sessions` per the schema below.
- New column `memories.session_id text references sessions(session_id)` (NULL allowed for historical rows).
- New domain type `Session` with the fields needed by the resolver and (future) HTTP layer.
- New optional field `MemoryRecord.session_id: Option<String>`.
- New repository methods: `latest_active_session`, `open_session`, `close_session`, `touch_session`.
- New `pipeline::session::resolve_session` that returns the `session_id` to use for the current ingest, opening or continuing as appropriate.
- Wired into `memory_service::ingest`: resolve session before writing the record, then `touch_session` afterward to update `last_seen_at` and increment `memory_count`.
- Configuration: `MEM_SESSION_IDLE_MINUTES` (default 30); read fresh on each ingest call.
- Unit tests covering the pure decision logic + `minutes_since` helper.
- Integration tests covering the full ingest-with-auto-bucket flow against an ephemeral DuckDB.

## Non-Goals

- HTTP / MCP surface for sessions (deferred — Q2 chose B during brainstorming).
- `episodes.session_id` (deferred — Q4 chose B; episodes stay untouched).
- `DELETE /sessions/{id}` soft-delete via supersedes (deferred — Q1 chose C; the ended_at column is enough for now).
- Cross-`caller_agent` session merging (each agent gets its own buckets — explicit in §11 "不做什么").
- Goal as a required field (it stays nullable, caller-supplied; no auto-derivation).
- Backfilling `session_id` on historical rows. They stay NULL — interpreted as "independent pseudo-session" by future read paths.
- Including `session_id` in `compute_content_hash`. Session is a container/index, not part of memory identity (same treatment as `summary` from §9).

## Decisions (resolved during brainstorming)

- **Q1 (DELETE endpoint)**: C — only ship the `ended_at` column. No DELETE route, no supersedes-based soft delete. Future PR can add it without schema changes.
- **Q2 (HTTP surface)**: B — schema + auto-bucket + repo helpers only. The HTTP routes (`GET /sessions`, `GET /sessions/{id}`) ship in a follow-up.
- **Q3 (session_id format)**: A — UUIDv7 via `uuid::Uuid::now_v7()`, matching the project convention for `memory_id` / `episode_id`.
- **Q4 (episodes.session_id)**: B — only `memories` gets the FK column. Episodes stay as-is; their session linkage is a separate PR.
- **§11 design tweak**: add `last_seen_at` column. The original §11 resolver compared `ended_at.unwrap_or(started_at)` against `now`, which fails for long sessions (an 8-hour active session would always look "idle" relative to its own start). `last_seen_at` is updated on every ingest within the session and is the authoritative "last activity time."

## Schema

`db/schema/004_sessions.sql` (append-only convention; never edit historical files):

```sql
-- Sessions: time-window containers for memories captured by a single
-- caller_agent. Auto-bucketed on ingest by resolve_session(); see §11
-- of mempalace-diff and the 2026-04-29 sessions design doc.

create table if not exists sessions (
    session_id text primary key,
    tenant text not null,
    caller_agent text not null,
    started_at text not null,
    last_seen_at text not null,
    ended_at text,
    goal text,
    memory_count integer not null default 0
);

create index if not exists idx_sessions_agent_active
    on sessions(tenant, caller_agent, ended_at);

alter table memories add column session_id text references sessions(session_id);

create index if not exists idx_memories_session on memories(session_id);
```

### DuckDB caveats

- DuckDB's `alter table ... add column` rewrites the table fully on first run. Acceptable: this codebase has no production-scale data yet.
- DuckDB does **not** support `if not exists` for `alter table add column`. The migration runner (TODO: confirm exact behavior in `storage/duckdb.rs`) must be idempotent — typically by tracking applied migrations in a meta table or by tolerating "duplicate column" errors. Implementer must verify this works under both fresh-init and re-run scenarios. If the runner errors on re-applying, it needs a `pragma` check or a try/catch.

## Domain Types

### `domain/session.rs` (new file)

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub session_id: String,
    pub tenant: String,
    pub caller_agent: String,
    pub started_at: String,
    pub last_seen_at: String,
    pub ended_at: Option<String>,
    pub goal: Option<String>,
    pub memory_count: u32,
}
```

### `domain/memory.rs` (modify)

Add to `MemoryRecord`:

```rust
#[serde(skip_serializing_if = "skip_none")]
pub session_id: Option<String>,
```

Place after `idempotency_key`. `IngestMemoryRequest` is **not** modified — session is server-derived.

## Repository Surface

### Trait additions (`storage/repository.rs` or wherever `Repository` lives)

```rust
async fn latest_active_session(
    &self,
    tenant: &str,
    caller_agent: &str,
) -> Result<Option<Session>, StorageError>;

async fn open_session(
    &self,
    session_id: &str,
    tenant: &str,
    caller_agent: &str,
    now: &str,
) -> Result<Session, StorageError>;

async fn close_session(
    &self,
    session_id: &str,
    ended_at: &str,
) -> Result<(), StorageError>;

async fn touch_session(
    &self,
    session_id: &str,
    last_seen_at: &str,
) -> Result<(), StorageError>;
```

### `latest_active_session` SQL

```sql
SELECT session_id, tenant, caller_agent, started_at, last_seen_at, ended_at, goal, memory_count
FROM sessions
WHERE tenant = ? AND caller_agent = ? AND ended_at IS NULL
ORDER BY last_seen_at DESC
LIMIT 1
```

### `open_session` SQL

```sql
INSERT INTO sessions (session_id, tenant, caller_agent, started_at, last_seen_at, memory_count)
VALUES (?, ?, ?, ?, ?, 0)
```

Returns the freshly-built `Session`. The `session_id` is generated by the caller (UUIDv7).

### `close_session` SQL

```sql
UPDATE sessions SET ended_at = ? WHERE session_id = ? AND ended_at IS NULL
```

The `AND ended_at IS NULL` guard is defensive — prevents accidentally moving an already-closed session's ended_at.

### `touch_session` SQL

```sql
UPDATE sessions
SET last_seen_at = ?, memory_count = memory_count + 1
WHERE session_id = ?
```

No `ended_at IS NULL` guard here — if the resolver decided to continue a session, we're committed.

## Resolver

### `pipeline/session.rs` (new file)

```rust
use crate::storage::Repository;
use crate::storage::error::StorageError;
use crate::domain::session::Session;

pub fn idle_minutes_from_env() -> u64 {
    std::env::var("MEM_SESSION_IDLE_MINUTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n: &u64| n > 0)
        .unwrap_or(30)
}

/// Returns minutes elapsed from `from` to `to`. Returns `None` on parse error.
/// Both inputs use the project's `current_timestamp` ISO format.
pub fn minutes_since(from: &str, to: &str) -> Option<i64> {
    // Implementation: parse both timestamps using whatever the project uses
    // (likely chrono::DateTime<Utc> from current_timestamp's ISO output).
    // Read existing parsing in service/memory_service.rs::current_timestamp
    // to match exactly. Return signed minutes; callers compare to idle_minutes
    // as i64 and treat negative as "in the past, very far" (clamped to >= idle).
    todo!()
}

pub enum SessionDecision {
    Continue(String),
    OpenNew { previous: Option<String> },
}

pub fn decide_session(
    latest: Option<(&str /* session_id */, &str /* last_seen_at */)>,
    now: &str,
    idle_minutes: u64,
) -> SessionDecision {
    match latest {
        Some((id, last_seen)) => {
            let elapsed = minutes_since(last_seen, now).unwrap_or(i64::MAX);
            if elapsed >= 0 && (elapsed as u64) < idle_minutes {
                SessionDecision::Continue(id.to_string())
            } else {
                SessionDecision::OpenNew { previous: Some(id.to_string()) }
            }
        }
        None => SessionDecision::OpenNew { previous: None },
    }
}

pub async fn resolve_session(
    repo: &impl Repository,
    tenant: &str,
    caller_agent: &str,
    now: &str,
    idle_minutes: u64,
) -> Result<String, StorageError> {
    let latest = repo.latest_active_session(tenant, caller_agent).await?;

    match decide_session(
        latest.as_ref().map(|s| (s.session_id.as_str(), s.last_seen_at.as_str())),
        now,
        idle_minutes,
    ) {
        SessionDecision::Continue(id) => Ok(id),
        SessionDecision::OpenNew { previous } => {
            if let Some(prev) = previous {
                repo.close_session(&prev, now).await?;
            }
            let new_id = uuid::Uuid::now_v7().to_string();
            repo.open_session(&new_id, tenant, caller_agent, now).await?;
            Ok(new_id)
        }
    }
}
```

The split between `decide_session` (pure) and `resolve_session` (DB-bound) gives us cheap unit tests for the decision logic without mocking.

## `memory_service::ingest` Wiring

Inside `MemoryService::ingest`, after the existing dedup short-circuit (`find_by_idempotency_or_hash` early return) but before constructing the `MemoryRecord`:

```rust
let session_id = pipeline::session::resolve_session(
    &*self.repository,
    &request.tenant,
    &request.source_agent,
    &now,
    pipeline::session::idle_minutes_from_env(),
).await?;
```

Then inside the `MemoryRecord { ... }` literal, set:

```rust
session_id: Some(session_id.clone()),
```

After `self.repository.create_memory(&memory).await?` completes successfully:

```rust
self.repository.touch_session(&session_id, &now).await?;
```

Order rationale:
- Resolution before write: we need `session_id` on the row.
- Touch after write: don't bump `memory_count` if the write fails. If `touch_session` itself fails, the memory is already persisted — the session row is stale by 1 count but functional. Acceptable degraded state for a non-critical counter.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `MEM_SESSION_IDLE_MINUTES` | 30 | Idle threshold for closing a session. Set to e.g. 5 for CI/batch jobs that run quickly and want tighter buckets. Read fresh on each ingest call (no caching) — ingest is not a hot path. |

If the env var is unset, malformed, or `0`, falls back to 30.

## Testing Strategy

### Unit tests in `pipeline/session.rs::tests`

1. **`decide_session_continue_when_within_idle`** — `latest = Some(("sid_a", "2026-04-29T08:00:00Z")), now = "2026-04-29T08:05:00Z", idle = 30` → `Continue("sid_a")`.
2. **`decide_session_open_new_when_idle_exceeded`** — same fixture but `now = "2026-04-29T09:00:00Z"` (60 min later) → `OpenNew { previous: Some("sid_a") }`.
3. **`decide_session_open_new_when_no_existing`** — `latest = None` → `OpenNew { previous: None }`.
4. **`minutes_since_basic_math`** — assert `minutes_since("2026-04-29T08:00:00Z", "2026-04-29T08:30:00Z") == Some(30)`.
5. **`minutes_since_invalid_returns_none`** — malformed timestamp → `None`. Caller defaults to "treat as idle" (consistent with `unwrap_or(i64::MAX)`).
6. **`idle_minutes_default_when_unset`** — verify default 30 when env unset (use a temp scope; same pattern as RRF test).

### Integration tests in `tests/sessions_integration.rs` (new file)

Use the existing ephemeral-DuckDB pattern from other integration tests (look at `tests/ingest_api.rs` or `tests/embedding_jobs.rs` for the test-app builder).

1. **`ingest_creates_first_session`**
   - Empty DB. POST `/memories` with `tenant=t, source_agent=a`.
   - Assert: 1 row in `sessions` table; `last_seen_at = started_at`; `memory_count = 1`. The new memory has `session_id` set.

2. **`ingest_continues_active_session_within_idle`**
   - First ingest creates session S1.
   - Second ingest within 30 min (default). Assert both memories share S1's `session_id`; sessions table still has 1 row; `memory_count = 2`; `last_seen_at` of the second ingest > `started_at`; `ended_at` is NULL.

3. **`ingest_opens_new_session_after_idle`**
   - First ingest creates session S1.
   - Manually update `sessions.last_seen_at` for S1 to a timestamp > 30 min in the past (direct DB write — test-only convenience).
   - Second ingest. Assert: new session S2 in sessions table; S1.`ended_at` is set (close_session fired); the second memory has S2's `session_id`.

4. **`ingest_independent_session_per_caller_agent`**
   - Two ingests, same tenant, different `source_agent` ("codex" vs "cursor"). Assert: 2 sessions in the table, both with `ended_at` NULL, each linked to one memory.

### Existing tests

Run unchanged. Many will now produce memories with non-null `session_id` (and a new session row per test app instance). Assertions that check the full memory JSON shape may need to ignore the new field:

- Locate by grepping for `assert_eq!(memory.summary, ...)` style or full-record assertions.
- If assertion is "field equals X" — fine.
- If assertion is "memory has exactly these N fields" or "JSON output matches verbatim" — needs to acknowledge the new optional field.

Plan task 5 explicitly walks the test suite and updates anything that breaks.

## Risk Assessment

- **Schema migration**: DuckDB `alter table add column` rewrites. Time cost on local DBs is negligible (the dataset isn't large yet). Production DBs: do during a quiet window. Documented in §11 risks.
- **NULL `session_id` rows post-migration**: read paths default to "independent pseudo-session" (a NULL value, no FK violation, no constraint trigger). Future filter queries on `session_id` need to handle NULL explicitly.
- **Phantom sessions on partial failure**: `resolve_session` may open a session that the subsequent memory insert then fails. Resulting state: empty session row with `memory_count = 0`, `ended_at IS NULL`. Acceptable — next ingest will continue or close it like any other. A garbage-collection sweep is out of scope.
- **Migration idempotency**: implementer must verify the migration runner can re-apply `004_sessions.sql` without erroring. If DuckDB rejects "duplicate column" on `alter table add column`, the runner needs to handle it (catch the error, or check column existence first).
- **`current_timestamp` format consistency**: `minutes_since` parsing depends on the exact format produced by `service::memory_service::current_timestamp`. The implementer reads that function first and matches the parser.
- **FK constraint at insert time**: if DuckDB enforces FKs (it does, for declared FKs), inserting a memory with a `session_id` not yet committed in `sessions` errors. The order in `ingest` (open_session BEFORE create_memory) handles this. Tests must verify.
- **Concurrency**: `Arc<Mutex<Connection>>` serializes everything. No race between resolve and insert. If the storage layer ever gains a connection pool, this design needs revisiting (lock acquisition or transactional resolve).
- **`source_agent` as the agent dimension**: §11 refers to `caller_agent`. The existing `IngestMemoryRequest` field is `source_agent`. Implementer should verify consistency — likely we just use `request.source_agent` as the value to store in `sessions.caller_agent`. If `caller_agent` is meant to be a separate concept, that's a bigger discussion (see "Concerns to confirm before implementing" below).

## Concerns to Confirm Before Implementing

- **`source_agent` vs `caller_agent`**: The schema column is `caller_agent`, the request field is `source_agent`. Are these semantically the same in this codebase? If yes, the bucket key is `(tenant, source_agent)` and we just rename for the column. If they're meant to differ (e.g., `source_agent` is who created the memory but `caller_agent` is the live interactive caller), the resolution logic needs a different value source. **Implementer must confirm by reading existing usage** and either treat them as synonyms or adjust.
- **Migration runner behavior**: confirm whether re-running `004_sessions.sql` errors on the `alter table add column` line. If yes, plan task 1 includes a defensive workaround (or a meta-table check).
- **FK enforcement**: confirm DuckDB enforces the `references` clause. If it doesn't (FKs declarative-only), we lose insert-time guarantee but the design still works in practice.

## Configuration

No new HTTP endpoints. Only the env var listed above.

## Error Handling

- All new repo methods return `Result<_, StorageError>`. Wired up the existing way through `ServiceError::Storage(...)`.
- `resolve_session` propagates DB errors; the caller `ingest` returns them as `ServiceError::Storage(...)`.
- `idle_minutes_from_env` cannot fail (always returns u64 with default fallback).
- `minutes_since` returns `Option<i64>` and the resolver treats `None` as "very long ago" (i.e., open new). This is the safe default — better to over-bucket than to glue together memories from a parse error.

## Crash / Recovery

- DB writes are individual `Arc<Mutex<Connection>>`-serialized operations. No transaction wraps them today. After a crash mid-`ingest`:
  - If crash between `open_session` and `create_memory`: phantom session, no memory. Recovers naturally on next ingest (the phantom is "active" but stale enough to be idle on next attempt).
  - If crash between `create_memory` and `touch_session`: memory is in DB with the session_id; session has `last_seen_at` of the *previous* memory write (or `started_at` if none) and `memory_count` is off by one. Functional, slightly stale.
- These degraded states are tolerated; no recovery code added in this PR.

## Out of Scope (this PR)

- HTTP routes (`GET /sessions`, `GET /sessions/{id}`, `DELETE /sessions/{id}`)
- `episodes.session_id` extension
- Soft-delete of sessions via `supersedes_memory_id`
- Backfill of `session_id` on historical rows
- Cross-`caller_agent` session merging
- A garbage-collector for phantom sessions (zero `memory_count`, never written to)
- Surfacing session info in MCP tool responses
- A clock abstraction for tests (we manipulate `last_seen_at` directly via test-only DB writes for the idle-test case)

## Verification Checklist (pre-merge)

- `cargo test -q` — all suites pass
- `cargo fmt --check` — clean
- `cargo clippy --all-targets -- -D warnings` — clean
- `cargo build --release` — clean
- Manual smoke: `cargo run -- serve`; ingest two memories within 30 min; `select * from sessions` shows 1 row with `memory_count = 2`. Wait > 30 min OR manually update `last_seen_at`; ingest again; verify a second session row appears.
- Re-init smoke: delete the DB file, restart `cargo run -- serve`. Confirm migration applies cleanly on fresh init.

## References

- ROADMAP.MD row #10
- mempalace-diff §11 (the original Sessions design — this spec refines `last_seen_at` semantics)
- `db/schema/001_init.sql` (current `episodes` table — pattern reference)
- `db/schema/003_graph.sql` (most recent migration — pattern reference for DuckDB-flavored DDL)
- `src/service/memory_service.rs::ingest` (caller adjustment site)
- `src/storage/duckdb.rs` (where the new repository methods land)
- `docs/superpowers/specs/2026-04-29-verbatim-guard-design.md` — pattern reference for "field added to MemoryRecord, not part of compute_content_hash"
