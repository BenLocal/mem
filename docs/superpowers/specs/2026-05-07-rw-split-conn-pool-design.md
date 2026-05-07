# Read/Write-Split DuckDB Connection Pool — Design

## Summary

`Arc<Mutex<Connection>>` in `src/storage/duckdb.rs:85` is the single concurrency boundary for the entire service. Every HTTP handler, every embedding worker tick, every transcript ingest serializes through the same lock. For SELECT-heavy paths (`memory_search`, `memory_search_contextual`, `GET /transcripts`) this is the throughput ceiling.

This spec proposes a **two-part narrow change**:

1. **Read pool** — an `r2d2` pool of read-only connections that backs the two search hot paths (`semantic_search_memories`, `lexical_search_memories`). Behind `MEM_RW_POOL_ENABLED=1` for first ship; everything else still routes through the existing write Mutex.
2. **Worker isolation** — `embedding_worker` (and `transcript_embedding_worker`) get their own dedicated `Connection`, separate from the HTTP write Mutex. Worker tick no longer blocks HTTP write traffic and vice versa. Unconditional in v1 (no flag).

The HTTP write Mutex stays serialized — no transaction-conflict surface or FK retry-loop class of issues introduced on the HTTP write path. Worker writes target a non-overlapping set of tables (`embedding_jobs` UPDATEs vs HTTP-side INSERTs), so DuckDB's MVCC handles them without conflicts in steady state.

This spec deliberately stops short of a full multi-writer pool. That comes later (or never).

## Goals

- Unlock parallel SELECT execution for the two confirmed hot paths (`semantic_search_memories`, `lexical_search_memories`) via the read pool
- Stop the embedding worker from blocking HTTP writes (and vice versa) by giving the worker its own dedicated `Connection`
- Zero changes to HTTP write-path call sites — `MemoryService::ingest`, `supersede_memory`, `transcript_repo::insert_*` keep their Mutex semantics
- Read pool behind `MEM_RW_POOL_ENABLED=1` for first ship; worker isolation is unconditional (no flag — it's a Mutex split with no behavior surface to A/B)
- No new public surface area on `MemoryService` / `TranscriptService` — the split is hidden inside `DuckDbRepository`

## Non-Goals

- Multi-writer concurrency on the same table (would require conflict-retry around every UPDATE — see "Risks" below). The two write connections write to *different* tables in steady state.
- Cross-process pool / external Postgres-style coordinator
- Async DB driver (DuckDB Rust binding is sync; async wouldn't change throughput)
- Refactoring `vector_index.rs` (already `Arc<RwLock<Index>>`, already supports concurrent reads — no change needed)
- Routing every SELECT to the read pool. v1 is just the two search hot paths; everything else stays on `http_write_conn` until per-method audit.
- Replacing `r2d2` with `deadpool` / a custom async pool
- Profiling-driven gating (see "Decisions resolved 2026-05-07" — gate is bench result, not pre-merge profile data)

## Decisions

- **`r2d2` over `deadpool`**: DuckDB's Rust binding is sync. All current call sites already run inside `tokio::task::spawn_blocking` (or the equivalent axum/blocking pattern). Adding async-pool semantics buys nothing and adds a dep.
- **Three connection buckets**: (1) `http_write_conn: Arc<Mutex<Connection>>` — the existing connection, used by all HTTP write paths; (2) `worker_write_conn: Arc<Mutex<Connection>>` — a dedicated connection for the embedding worker(s), so worker ticks no longer block HTTP writes (and vice versa); (3) `read_pool: r2d2::Pool<…>` of N read-only connections for opt-in SELECT paths.
- **Pool size: hardcoded 8**. Search is the dominant read workload; 8 concurrent searches comfortably saturate ANN + post-fetch on commodity hardware. No env-var knob in v1 — trivially adjustable later if a workload demands it.
- **Pool checkout timeout 5 s** — fail fast rather than queue indefinitely. Pool exhaustion is a signal of either overload or accidentally-blocking-write-from-read-path.
- **Each pool / worker connection runs `SET threads = 1` at init** so DuckDB doesn't try to consume the host's whole CPU on every checkout. Eight read conns × all CPU each would be a thundering herd.
- **Read methods are pure-SELECT**: no DDL, no INSERT/UPDATE/DELETE, no `CREATE TEMP TABLE`, no `PRAGMA`. Marked at type level via a new `ReadOnlyConn<'a>` newtype wrapper that exposes only `prepare(&str)` and `query_*` — not `execute(&str)`. Misuse is a compile error, not a runtime check.
- **Read pool is opt-in per call site, not opt-out**. Default = every SELECT keeps using the HTTP write Mutex (current behavior). A method moves to the read pool only when its author has confirmed — and left a comment at the routing point — that no caller depends on read-own-write within the same handler. **v1 opt-ins: `semantic_search_memories` and `lexical_search_memories` only.** Everything else in the "candidates" table below is *eligible* but unrouted until a follow-up audit.
- **Schema migrations run only on the write connection at startup** (existing behavior). Read connections open against the already-migrated file; no schema state is per-connection.
- **HNSW sidecar untouched**: `Arc<RwLock<Option<Arc<VectorIndex>>>>` already permits concurrent ANN reads via `lock_index_read`. The DuckDB read pool unblocks the SQL-side post-fetch (the `SELECT memories WHERE memory_id IN (...)` step that currently sits behind the write lock).

## Architecture

`src/storage/duckdb.rs` shape change:

```rust
pub struct DuckDbRepository {
    /// HTTP-side write connection. Same Mutex semantics as today; all
    /// HTTP-driven writes (memory ingest, supersede, transcript ingest,
    /// graph edges, schema migrations) go through this.
    http_write_conn: Arc<Mutex<Connection>>,

    /// Worker-side write connection. embedding_worker and
    /// transcript_embedding_worker hold this exclusively for status
    /// updates on `embedding_jobs` / `transcript_embedding_jobs` and
    /// vector upserts on `memory_embeddings`. Worker ticks are
    /// single-threaded so a bare Mutex is sufficient — no pool needed.
    worker_write_conn: Arc<Mutex<Connection>>,

    /// Read pool. Opt-in SELECT paths (`semantic_search_memories`,
    /// `lexical_search_memories`) check out a connection here.
    /// `None` when MEM_RW_POOL_ENABLED is unset — those paths fall back
    /// to http_write_conn (current behavior).
    read_pool: Option<r2d2::Pool<DuckDbReadManager>>,

    vector_index: Arc<RwLock<Option<Arc<VectorIndex>>>>,
    // ... existing fields
}

struct DuckDbReadManager {
    db_path: PathBuf,
}

impl r2d2::ManageConnection for DuckDbReadManager {
    type Connection = Connection;
    type Error = duckdb::Error;

    fn connect(&self) -> Result<Connection, duckdb::Error> {
        let conn = Connection::open(&self.db_path)?;
        conn.execute_batch("SET threads = 1; SET memory_limit = '512MB'")?;
        Ok(conn)
        // NOTE: read-only enforcement is type-level via ReadOnlyConn — we
        // do *not* use SET access_mode='READ_ONLY' because that's
        // database-wide in DuckDB and would break the writer in the same
        // process if connections share a Database handle.
    }
    fn is_valid(&self, conn: &mut Connection) -> Result<(), duckdb::Error> {
        conn.execute_batch("SELECT 1")
    }
    fn has_broken(&self, _conn: &mut Connection) -> bool { false }
}
```

Internal routing helper:

```rust
impl DuckDbRepository {
    /// Run a pure-SELECT closure. If pool is enabled, use a checked-out read
    /// conn; otherwise fall back to the write Mutex (current behavior).
    fn with_read<F, R>(&self, f: F) -> Result<R, RepoError>
    where
        F: FnOnce(ReadOnlyConn<'_>) -> Result<R, RepoError>,
    {
        match &self.read_pool {
            Some(pool) => {
                let mut conn = pool.get()?;
                f(ReadOnlyConn::wrap(&mut conn))
            }
            None => {
                let conn = self.write_conn.lock();
                f(ReadOnlyConn::wrap(&conn))
            }
        }
    }
}
```

`ReadOnlyConn<'_>` is a thin newtype around `&Connection` that re-exports only:
- `prepare(&self, sql: &str) -> Result<Statement<'_>>`
- `query_row(...)` / `query_map(...)` shortcuts

It does **not** expose `execute(&str)` or `execute_batch(&str)`. Compile-time guarantee that nobody accidentally writes from the read pool.

## Migration paths (routing per method)

**Three buckets**: `[R]` = read pool, `[H]` = http_write_conn, `[W]` = worker_write_conn.

### Read pool — v1 opt-ins (only these two route to the pool initially)

| Method | File | Bucket |
|---|---|---|
| `semantic_search_memories` | `storage/duckdb.rs` | **[R]** v1 |
| `lexical_search_memories` | `storage/duckdb.rs` | **[R]** v1 |

### Read pool — eligible candidates (stay on `[H]` until per-method audit)

| Method | File | Status |
|---|---|---|
| `get_memory` | `storage/duckdb.rs` | candidate — audit RoW (used post-ingest?) |
| `list_memories` | `storage/duckdb.rs` | candidate |
| `list_pending_review` | `storage/duckdb.rs` | candidate |
| `transcript_repo::list_messages` | `storage/transcript_repo.rs` | candidate |
| `transcript_repo::search_*` | `storage/transcript_repo.rs` | candidate |
| `entity_repo::lookup_by_alias` | `storage/entity_repo.rs` | candidate — but used inside ingest, may need RoW; default `[H]` |
| `graph_store::neighbors` / `neighbors_at` | `storage/graph_store.rs` | candidate |

### Worker write connection `[W]`

| Method | File |
|---|---|
| `claim_next_n_embedding_jobs` | `storage/duckdb.rs` |
| `mark_job_completed` / `mark_job_failed` / `mark_job_stale` | `storage/duckdb.rs`, `storage/transcript_repo.rs` |
| `upsert_memory_embedding` | `storage/duckdb.rs` |
| `upsert_transcript_embedding` | `storage/transcript_repo.rs` |
| `delete_embedding_jobs_by_memory_id` (orphan cleanup, called from worker) | `storage/duckdb.rs` |

### HTTP write connection `[H]` (unchanged from today)

Everything else: ingest paths, supersede, schema migrations, graph edge sync, transcript message inserts, feedback events, memory archival.

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Read-after-write linearizability: a read started right after a write may not see the write (DuckDB MVCC, separate connection sees its own snapshot) | Acceptable for search — top-k results need not include literally-just-written rows. Document explicitly. For paths that *must* read-own-write (e.g., `memory_get` immediately after `memory_ingest`), either route through write conn or accept eventual visibility. Audit table: in current code, `memory_get` is mostly used for "fetch the row I just wrote in this same handler" — those handlers can keep using write conn. |
| Pool checkout starvation under load | 5 s timeout + structured error → 503. Surfaces a real signal rather than hanging. |
| Connection-init cost on cold pool | r2d2 `min_idle = 2`, lazy fill up to `max_size`. Pre-warm on startup. |
| DuckDB extensions / settings divergence between write conn and read conns | Read connections run identical `SET` / `LOAD` at `connect()`. Codify this in `DuckDbReadManager::connect`. |
| WAL / checkpoint contention | Single-writer model is unchanged; checkpoints still happen on the write connection. Read connections see snapshot consistency. |
| FK retry-loop class of incidents (cf. 2026-05-06) | The orphan-detection logic in `embedding_worker` (FK error → mark stale → continue) is preserved. Adding `worker_write_conn` does not change *whether* orphans appear — it changes *who serializes the FK-violating UPDATE*. Worker stays single-tick, so within the worker conn there's still only one in-flight UPDATE on `embedding_jobs` at a time. The HTTP supersede path that creates orphans runs on `http_write_conn` in a separate transaction; this matches today's behavior since the existing single Mutex was released between supersede commit and worker tick anyway. **No regression expected. **|
| Worker conn vs HTTP conn writing same table (`embedding_jobs`) | Conflict surface is INSERT (HTTP, new pending row) vs UPDATE (worker, existing row). Different MVCC tuples, no row conflict in DuckDB's optimistic concurrency model. Verified via stress test in `tests/`. |
| `mem repair` / CLI tools | Out of scope. Repair tools open their own `Connection` directly today and continue to do so. |

## Rollout

1. Land behind `MEM_RW_POOL_ENABLED=1` (default unset → existing single-connection behavior, only the worker isolation kicks in unconditionally since it has no failure mode beyond the Mutex split).
2. **Worker isolation is unconditional in v1** (no flag) — it's a localized split with a clear "two single-writer tracks against non-overlapping tables" model. No flag because there's no behavior change to A/B against; either it works or we revert.
3. **Read pool is flagged**. Run a `tests/bench/concurrent_search.rs` before/after with `MEM_RW_POOL_ENABLED=1` set. Sanity threshold: ≥1.5× P99 search throughput improvement at 4 concurrent searches. Below that → leave the flag default-off until the gain is proven.
4. Flip the read-pool default to on in a follow-up commit only after the bench result + at least one week of soak in dev tenant.

## Testing

- **Existing integration tests pass unchanged** — they use whichever code path the env var dictates. Run the full suite once with `MEM_RW_POOL_ENABLED=1`, once unset.
- **`tests/conn_pool.rs`** (new):
  - Spawn 8 concurrent `memory_search` calls with pool on; assert wall-clock < 1.5× single-call latency (pool actually parallelizes)
  - Interleave 1 HTTP writer + 4 readers; assert all readers complete and writer commits within bounded time
  - Pool exhaustion: configure pool size = 1, fire 4 concurrent searches with timeout 1 s, assert 3 fail-fast with timeout error
- **`tests/worker_isolation.rs`** (new):
  - Drive a long HTTP write path (large supersede chain) and concurrently drive worker tick; assert worker progress is not blocked by the HTTP write lock and vice versa
  - Stress test: 100 HTTP INSERTs on `embedding_jobs` interleaved with 100 worker UPDATEs on the same table; assert zero transaction conflicts (sanity-check the MVCC INSERT-vs-UPDATE non-overlap claim)
- **Type-level test** (compile_fail doctest): `ReadOnlyConn::execute("INSERT …")` — must not compile.
- **Schema-migration safety**: open DB with stale schema, ensure `http_write_conn` migrates before read pool / worker conn open; add ordering test.

## Decisions resolved 2026-05-07

| Question | Resolution |
|---|---|
| Profile-data gate | **Skip** — proceed on architectural judgment. Bench in CI provides the confirmation signal post-implementation. |
| RoW policy | **Default to write conn, opt-in to read pool per method.** v1 routes only `semantic_search_memories` + `lexical_search_memories`. |
| Worker connection scope | **Phase 1** — `worker_write_conn` is part of this spec, not deferred. |
| Pool size | **Hardcoded 8.** No env-var tuning knob in v1. |

## What this spec does *not* commit to

- Phase 3 (full multi-writer pool with retry-on-conflict on every UPDATE path). Separate decision, separate spec.
- Cross-process pool coordination. Single-process model is unchanged.
- A guarantee that the read pool flag flips to default-on. The `tests/bench/concurrent_search` result decides.
