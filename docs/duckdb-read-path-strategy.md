# DuckDB Read-Path Strategy & Decision Record

> ⚠️ **SUPERSEDED (2026-06-24).** The central decision of this doc — "keep DuckDB
> as the read engine, it isn't worth replacing" — was **reversed**. In route-B the
> DuckDB read engine was **removed entirely** (the `duckdb` crate, `src/storage/duckdb_query/`,
> the `refresh`/`mark_dirty`/`ensure_fresh` machinery, the r2d2 read pool, `MEM_DUCKDB_THREADS`,
> and the dual-writer commit race are all gone); reads are now lancedb-native + a Tantivy
> FTS subsystem. See **`docs/remove-duckdb-keep-lance.md`** for the new decision and the
> migration record. **The body below is kept verbatim as a historical record** of why
> DuckDB was originally retained — do not treat its present-tense statements as current.

> Status: **decided** (2026-06-15). Supersedes the abandoned `duckdb-r2d2-pool.md`
> connection-pool plan. Scope of the round that produced this doc: ship the
> fire-fight (blocking-pool cap + DuckDB thread cap), capture the probe
> findings as a regression test, and record the architecture decisions
> below. The warm-connection refactor (§4) is **design-only** — no large
> implementation change landed.

This doc records WHY the read path is shaped the way it is, backed by an
empirical probe, so a future reader (or agent tempted to "add a connection
pool") doesn't re-derive it from scratch.

---

## 0. Background — the incident that triggered this

A long-lived `mem serve` ballooned to ~11 GB RSS holding 500–800
`tokio-rt-worker` threads with periodic CPU spikes (root-caused via
`kernel_clone` tracing). Two independent causes:

1. **tokio blocking-pool balloon.** Every DuckDB read runs inside
   `spawn_blocking` and serializes on the single `Arc<Mutex<Connection>>`.
   Under a burst of concurrent reads, N reads → N blocking tasks, all but
   one *parked on the mutex*, each pinning a real OS thread. tokio's default
   `max_blocking_threads = 512` let the pool grow toward that ceiling.
2. **DuckDB intra-query CPU spike.** DuckDB defaults to one thread per core.
   On a many-core box a single read fanned out across all cores. `mem`
   never set `SET threads`, so reads spiked CPU.

Neither is a *throughput* problem; both are *resource* problems.

---

## 1. The linchpin probe — can a read connection cheaply see fresh writes?

The read path's whole refresh machinery (`refresh()` / `ensure_fresh()` /
the `dirty` flag) exists because the lance DuckDB extension **pins the
dataset version per connection at first query**. The open question for any
optimization (warm-connection cheap refresh, or an r2d2 pool with cheap
staleness invalidation) was: *is there a cheaper way than rebuilding the
whole connection to make a read-only connection see a Rust-API write?*

We measured it directly with a **clean** probe — a read-only connection that
does ZERO DuckDB-side DML (the committed `lance_duckdb_poc` contaminates its
connection with DuckDB-side INSERT/UPDATE/DELETE before the Rust write,
which itself refreshes the snapshot, so it cannot answer this). The probe is
now a CI-enforced regression test: **`tests/lance_snapshot_visibility.rs`**.

Ground truth (current lance/duckdb versions), for both a Rust-API **append**
and the harder Rust-API **update**:

| Refresh mechanism (same read-only conn)        | Append visible? | Update visible? |
|------------------------------------------------|:---------------:|:---------------:|
| (a) no refresh                                 | ❌              | ❌              |
| (b) `DETACH ns;` + `ATTACH …`                  | ❌              | ❌              |
| (b2) `ATTACH OR REPLACE …`                     | ❌              | ❌              |
| **(c) brand-new `Connection` (INSTALL/LOAD/ATTACH)** | ✅          | ✅              |

**Conclusion: no same-connection re-attach primitive clears the snapshot
cache. Only a fresh `Connection` sees post-attach writes.** The ~100ms
connection rebuild is therefore *intrinsic* to the "DuckDB attaches Lance"
route — it cannot be optimized into a cheap in-place re-attach.

---

## 2. Decision: connection pool — **REJECTED**

A bounded r2d2 read pool was planned (generation-stamped connections, dropped
+ rebuilt on staleness via `is_valid`). It is **rejected** for now:

- **It does not fix the incident.** The thread balloon is fixed by capping
  `max_blocking_threads` (§3); a pool doesn't lower the thread count, it only
  trades parking for concurrent work. The CPU spike is fixed by `SET threads`
  (§3). A pool addresses neither.
- **A pool's only benefit is read concurrency**, and that benefit is
  **unmeasured** — `mem` is local-first / few concurrent agents, so the
  single-mutex serialization is very likely not a throughput bottleneck.
- **A pool carries an N× write-churn cost** that follows directly from §1:
  every write bumps the staleness generation, invalidating all idle
  connections, so the pool rebuilds *N* connections (~100ms each) on the next
  reads after a write — vs the current single connection's *1* rebuild.
  Write-heavy bursts (e.g. `mem mine` ingesting hundreds of capsules) make
  this strictly worse than today.

**Revisit only if** profiling shows read concurrency is a real bottleneck.
Even then, prefer a small pool (2–3) with eyes open to the write-churn
trade-off, or — better — eliminate the attach entirely (§5).

---

## 3. What landed this round (the fire-fight)

Both are pure resource caps, orthogonal to the pool question, and verified by
the three CI gates (`cargo fmt --check`, `cargo clippy --all-targets -D
warnings`, `cargo test`).

- **Blocking-pool cap** (`src/main.rs`): build the tokio runtime explicitly
  with `max_blocking_threads(32)` instead of `#[tokio::main]`'s default 512.
  32 is ample for the embedding inference + mutex-bound reads; excess work
  queues in tokio's internal queue rather than as idle thread stacks.
- **DuckDB thread cap** (`src/storage/duckdb_query/mod.rs`): every read
  connection now runs `SET threads = N` (default **6**, env
  `MEM_DUCKDB_THREADS`, invalid / `0` → default). Applied via the shared
  `build_lance_connection` helper so `open` and `refresh` cannot drift.
  Unit-tested in `duckdb_query::tests`.

Deployment note (the live daemon still carries the leaked process): rebuild
the binary, replace the installed `mem`, restart the daemon to clear the
~11 GB / thread backlog. Restart resets the evolution-worker idle clock.

---

## 4. Architecture conclusions (verified, recorded for posterity)

### (a) Keep DuckDB — it is the SQL read engine over Lance. **Verified.**

`src/storage/duckdb_query/` is **~240 KB** of query logic (measured
242,741 bytes; **69 `SELECT`s**) that leans hard on SQL the LanceDB native
query API does not expose: **21 `GROUP BY`**, window functions (**4
`ROW_NUMBER`**, **4 `OVER (…)`**), **6 `JOIN`s**, plus ANN via
`lance_vector_search` (**11**) with `_distance` ranking (**10**). Dropping
DuckDB means re-implementing a query engine (joins, grouping, window/rank,
RRF fusion) over raw Arrow — not worth it. DuckDB stays; the only cost we
care about is the per-refresh attach (§4b, §5).

### (b) Warm-connection refresh — **the target form, but capped by §1**

The shape the read path should converge to (and the form `refresh()` /
`ensure_fresh()` already largely *are*):

> One **warm, long-lived** connection for reads, **`mark_dirty`** on every
> write, and on a read **refresh only when dirty** — reusing the connection
> and already-loaded extension, serialized by a mutex.

Reality check against the probe: the current design **already implements
most of this** — the connection is long-lived and reused across reads;
`mark_dirty` sets the flag on write; `ensure_fresh` refreshes lazily only on
the first read after a write; multiple writes coalesce into one refresh.

The **one remaining wish** — make the refresh itself cheap by *reusing the
connection + loaded extension* (a re-attach) instead of a full `Connection`
rebuild — is **blocked at the lance-extension layer**: §1 proves every
same-connection re-attach primitive (`DETACH`+`ATTACH`, `ATTACH OR REPLACE`)
fails to surface writes. So `refresh()` must keep doing a full rebuild
(`open_in_memory` + `INSTALL/LOAD lance` + `ATTACH`), and the ~100ms is
intrinsic until the extension grows a cache-invalidation primitive (the
standing `// TODO: lance_refresh()` in `refresh()`'s docs) or we move off
attach (§5).

**Net:** the converged form = *what exists today* (single warm connection +
`dirty` + lazy full-rebuild-on-read) **plus the §3 caps**. No large rewrite
is warranted by this round; if/when a cheap re-attach primitive appears,
`tests/lance_snapshot_visibility.rs` will flip and signal that `refresh()`
can be cheapened in place. **Design-only this round — do not implement a
rewrite without sign-off.**

### (c) Mid-term root-cause fix — DataFusion-over-Lance. **To evaluate.**

The only way to eliminate the attach (and thus the ~100ms rebuild *and* the
per-connection version pin) is to stop going through the DuckDB lance
extension and instead query Lance with **DataFusion in-process**: Lance
exposes a DataFusion `TableProvider` that reads the **latest** committed
version natively from Arrow, no ATTACH, no snapshot pin. That would make
reads see writes immediately with no refresh machinery at all.

Cost/unknowns to evaluate before committing: DataFusion's SQL dialect vs the
69 existing DuckDB queries (window/rank/`lance_vector_search`/`lance_fts`
have no 1:1 DataFusion equivalent and would need rework), build-graph weight,
and whether FTS/ANN table functions are reachable. **Recorded as a candidate,
not scheduled.**

---

## 5. Summary table

| Item | Decision |
|---|---|
| tokio `max_blocking_threads` | **Capped to 32** (was default 512). Fixes thread balloon. ✅ landed |
| DuckDB `SET threads` | **Capped to 6** (`MEM_DUCKDB_THREADS`). Fixes CPU spike. ✅ landed |
| r2d2 connection pool | **Rejected** — doesn't fix the incident, unmeasured benefit, N× write churn. Revisit only on proven read contention. |
| Keep DuckDB | **Yes** — ~240 KB / 69-`SELECT` SQL engine over Lance; replacing it isn't worth it. |
| Warm-connection cheap refresh | **Target form, blocked by §1** — current design is already 90% there; cheap re-attach is impossible (probe-proven), so full rebuild stays. Design-only. |
| DataFusion-over-Lance | **To evaluate** — the only route that removes attach entirely. |
| Linchpin probe | **Promoted to `tests/lance_snapshot_visibility.rs`** — CI guard on the snapshot-pin assumption. |
