# Temporal Graph Edges — Design

> Closes mempalace-diff §8 #5 (with deviation: full migration from IndraDB to DuckDB rather than the originally-budgeted "extend IndraDB property API" approach).

## Summary

The graph layer currently stores edges as `(from_node_id, to_node_id, relation)` triples in IndraDB (in-memory `MemoryDatastore` with optional msgpack persistence). Edges have no time dimension — when a memory is superseded, its edges remain in the graph as if they were still active, contaminating retrieval and making "what was the graph state on 2026-03-15?" unanswerable.

This spec adds **`valid_from` / `valid_to` timestamps** to every graph edge and migrates the graph layer to a DuckDB `graph_edges` table (closing the IndraDB era entirely). The supersede flow is the sole trigger that closes edges (sets `valid_to`); rebuild logic is unnecessary because DuckDB row-level consistency replaces the cross-store sync problem.

## Goals

- Add `valid_from` / `valid_to` to `GraphEdge` so retrieval can filter to active edges and history can be queried by time
- Make `merge_and_rank_hybrid`'s `graph_boost` automatically ignore edges from superseded memories — without changing `pipeline/retrieve.rs`
- Close edges atomically when a memory is superseded
- Provide `neighbors_at(node_id, ts)` for point-in-time queries
- Drop IndraDB entirely (dependency, adapter, config, env vars) — DuckDB is the single source of truth

## Non-Goals

- Backfilling edges for memories that existed before the migration (intentional — IndraDB users were waved off; backfill is its own item if asked for later)
- A `mem repair --rebuild-graph` CLI (defer to a follow-up if a real user asks)
- Multi-hop traversal (`MemPalace_traverse`-style) — that is a separate feature on top of this infrastructure
- Per-tenant filtering at the graph layer — graph edges remain global; tenant filtering happens at memory retrieval (same model as §3 vector index)
- Foreign keys from `graph_edges.from_node_id` to `memories.memory_id` — node IDs are prefix-encoded strings (`memory:abc` / `project:foo`), not a single-table primary key
- Historical "rollback graph to time T" traversal — only point-in-time `neighbors_at` is provided

## Decisions (resolved during brainstorming)

- **Backend**: full migration to DuckDB. IndraDB dep, both adapters (`IndraDbGraphAdapter` and `LocalGraphAdapter`), and the `GraphStore` trait are deleted.
- **No backfill**: existing IndraDB graph state is abandoned; new ingest writes to the new table from the moment the migration ships. Old memories simply have no graph edges until they are re-ingested or superseded.
- **Supersede is the sole close trigger**: archive / status changes / feedback / decay do *not* close edges. If we ever want archive to close edges, that's a separate item.
- **`extract_graph_edges` stays a pure function**: it returns templates with `valid_from = ""`, `valid_to = None`; the storage layer fills timestamps at write time.
- **Schema migration is append-only** (per project convention): one new file, `db/schema/003_graph.sql`. Existing schema files are untouched.

## Architecture

A new module `src/storage/graph.rs` (full rewrite of the existing file) holds:

```rust
pub struct DuckDbGraphStore {
    repo: Arc<DuckDbRepository>,
}

impl DuckDbGraphStore {
    pub fn new(repo: Arc<DuckDbRepository>) -> Self;

    pub async fn sync_memory(&self, memory: &MemoryRecord) -> Result<(), GraphError>;
    pub async fn close_edges_for_memory(&self, memory_id: &str) -> Result<usize, GraphError>;

    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError>;
    pub async fn neighbors_at(&self, node_id: &str, at: &str) -> Result<Vec<GraphEdge>, GraphError>;
    pub async fn related_memory_ids(&self, node_ids: &[String]) -> Result<Vec<String>, GraphError>;
    pub async fn all_edges_for_memory(&self, memory_id: &str) -> Result<Vec<GraphEdge>, GraphError>;
}
```

No trait — there is one and only one implementation. `MemoryService` and any other consumer holds `Arc<DuckDbGraphStore>` directly. This eliminates the `dyn GraphStore` indirection and the `LocalGraphAdapter` test-only code path; integration tests use the real `DuckDbGraphStore` against a tempdir DuckDB (same pattern as §3 vector index tests).

`GraphError` shrinks to:

```rust
#[derive(Debug, Error)]
pub enum GraphError {
    #[error("graph backend error: {0}")]
    Backend(String),
}
```

`Poisoned`, `Unavailable`, `InvalidIdentifier` are deleted — they were IndraDB-specific. DuckDB lock poisoning surfaces through `DuckDbRepository::conn()` as `StorageError`, which converts to `GraphError::Backend(...)` at the boundary.

## Schema

`db/schema/003_graph.sql`:

```sql
create table if not exists graph_edges (
  from_node_id text not null,
  to_node_id   text not null,
  relation     text not null,
  valid_from   text not null,
  valid_to     text,                    -- NULL means active
  primary key (from_node_id, to_node_id, relation, valid_from)
);

-- Plain (non-partial) indexes: schema 002 documents that bundled DuckDB does not support
-- partial unique indexes; non-partial indexes here are safe and sufficient for current scale.
create index if not exists idx_graph_edges_from on graph_edges (from_node_id, relation);
create index if not exists idx_graph_edges_to on graph_edges (to_node_id, relation);
create index if not exists idx_graph_edges_history on graph_edges (from_node_id, valid_from);
```

The primary key includes `valid_from` so the same triple can have multiple historical rows (closed → reopened scenarios). The two `(node, relation)` indexes cover both directions; the history index covers `neighbors_at`. Active-edge filtering happens at query time via `WHERE valid_to IS NULL` — at the expected scale (≪100k edges) the index scan + filter is negligible. If profiling later shows a hotspot, this is the single point to revisit.

## Data Model

`domain/memory.rs::GraphEdge` becomes:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub struct GraphEdge {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
    pub valid_from: String,            // millisecond timestamp, zero-padded (matches current_timestamp())
    pub valid_to: Option<String>,      // None => still active
}
```

Both timestamps use the existing `current_timestamp()` format (`format!("{millis:020}")`), so they sort lexicographically.

`extract_graph_edges` (`pipeline/ingest.rs:94`) is unchanged in body — it still computes `(from, to, relation)` from memory fields. The constructed `GraphEdge` values get `valid_from = String::new()` and `valid_to = None` as placeholders. `DuckDbGraphStore::sync_memory` overwrites `valid_from` with `current_timestamp()` at write time. This keeps the function pure (no clock dependency) and testable without mocks.

## Data Flow

### Write: `sync_memory(memory)`

```
sync_memory(memory):
  now = current_timestamp()
  for edge in extract_graph_edges(memory):
    BEGIN
      exists = SELECT 1 FROM graph_edges
               WHERE from_node_id = ?1 AND to_node_id = ?2
                 AND relation = ?3 AND valid_to IS NULL
               LIMIT 1
      if exists:
        skip                         -- idempotent: active edge already there
      else:
        INSERT INTO graph_edges (from_node_id, to_node_id, relation, valid_from, valid_to)
        VALUES (?1, ?2, ?3, now, NULL)
    COMMIT
```

Idempotent by design:
- Active edge present → no-op (the common case for re-syncing the same memory)
- No active edge present → insert fresh row with `valid_from = now`
- A row with same triple but `valid_to IS NOT NULL` (closed) → does *not* prevent inserting a new active row; the closed row stays in history

The transaction wraps `SELECT EXISTS → INSERT` so the `Arc<Mutex<Connection>>` lock makes it atomic w.r.t. concurrent writers — same pattern as `try_enqueue_embedding_job`.

### Close: `close_edges_for_memory(memory_id)`

```sql
UPDATE graph_edges
   SET valid_to = ?1
 WHERE from_node_id = ?2 AND valid_to IS NULL;
```

`?1 = current_timestamp()`, `?2 = "memory:" + memory_id`. Returns the row count touched.

Only `from_node_id = "memory:..."` is matched — there is no inbound side because the memory-derived edges always have the memory as the outbound endpoint.

### Supersede integration

In `service/memory_service.rs` near line 493 (where `supersedes_memory_id` is set on the new memory), after the supersede SQL succeeds:

```rust
graph.close_edges_for_memory(&original.memory_id).await?;
graph.sync_memory(&superseding).await?;
```

If `close` fails the supersede operation as a whole returns Err — the memory record was already written but the graph is now inconsistent. Mitigation: on next ingest of any memory, the graph self-corrects for that memory's *new* edges; old un-closed edges from the superseded memory remain stuck. This is acceptable for a rare error path (most likely a DuckDB lock poison) and matches §3's "best-effort with eventual consistency" stance.

### Read: `neighbors(node_id)`

```sql
SELECT from_node_id, to_node_id, relation, valid_from, valid_to
  FROM graph_edges
 WHERE (from_node_id = ?1 OR to_node_id = ?1)
   AND valid_to IS NULL
 ORDER BY relation, from_node_id, to_node_id
```

Default behavior is unchanged from the current public contract: only active edges. The `valid_to IS NULL` filter is the new restriction. The two partial indexes cover this query.

### Read: `neighbors_at(node_id, at)`

```sql
SELECT from_node_id, to_node_id, relation, valid_from, valid_to
  FROM graph_edges
 WHERE (from_node_id = ?1 OR to_node_id = ?1)
   AND valid_from <= ?2
   AND (valid_to IS NULL OR valid_to > ?2)
 ORDER BY relation, from_node_id, to_node_id
```

`?2` is the timestamp string. The `idx_graph_edges_history` index covers the `from_node_id, valid_from` portion; the `valid_to` predicate is filtered after.

### Read: `related_memory_ids(node_ids)`

```sql
SELECT DISTINCT
  CASE WHEN from_node_id IN (...) THEN to_node_id ELSE from_node_id END
FROM graph_edges
WHERE (from_node_id IN (...) OR to_node_id IN (...))
  AND valid_to IS NULL
```

Then in Rust filter to `starts_with("memory:")` and strip the prefix. Default to active edges only — same as `neighbors`. Retrieve path uses this; behavior visible to `merge_and_rank_hybrid::graph_boost` is "superseded memory edges are silently excluded," which is the desired outcome.

### Read: `all_edges_for_memory(memory_id)`

```sql
SELECT from_node_id, to_node_id, relation, valid_from, valid_to
  FROM graph_edges
 WHERE from_node_id = ?1
 ORDER BY valid_from
```

`?1 = "memory:" + memory_id`. No `valid_to` filter. For debugging / future audit tooling — not consumed by any retrieval path in this PR.

## Concurrency

`DuckDbGraphStore` does not hold its own lock. Every method goes through `repo.conn()`, which serializes through the existing `Arc<Mutex<Connection>>`. Multiple async callers wait at the mutex; transactions are atomic. There is no nested lock concern (unlike `VectorIndex`, which has its own `RwLock<Index>`) because everything graph-related lives in the same connection.

The supersede flow already holds the DuckDB mutex through the original SQL operations; the additional `close_edges_for_memory` + `sync_memory(superseding)` calls re-acquire the mutex briefly. They are sequential, not nested.

## Crash / Recovery

- Crash mid-supersede (after `update_status` but before `close_edges_for_memory`): old memory is `Superseded`, but its edges still active → over-counts in `graph_boost` for that memory until something else triggers a re-close. Acceptable — superseded count is small, and re-running the supersede manually (or future `mem repair --rebuild-graph`) cleans it up.
- Crash mid-`sync_memory`: the per-edge transaction either commits or doesn't. Partial commits (some edges written, others not) are possible across multiple loop iterations; on next call the existing rows are detected and skipped, missing rows are inserted. Idempotent by construction.
- Disk full / DuckDB error: bubbles up as `GraphError::Backend`. Caller decides whether to fail the ingest or not.

## API Surface Changes

`HTTP GET /memories/{id}` returns `graph_links: Vec<GraphEdge>`. After this PR each edge gains two new JSON fields:

```json
{
  "from_node_id": "memory:abc",
  "to_node_id": "project:foo",
  "relation": "applies_to",
  "valid_from": "00000001761662918634",
  "valid_to": null
}
```

The change is **additive** — existing callers that ignore unknown fields are unaffected. No `#[serde(skip_serializing_if)]` on `valid_to`; consistent emission of `null` is clearer than conditional omission.

`HTTP GET /graph/neighbors/:node_id` likewise gains the two fields.

## Configuration Removed

- `Config.graph_backend` field
- `Config.indradb_path` field
- `GraphBackendKind` enum
- `ConfigError::InvalidGraphBackend`
- `GRAPH_BACKEND` env var
- `INDRADB_PATH` env var

`Config::from_env()` and `Config::local()` simplify accordingly. `EmbeddingSettings` is unchanged (graph config was on `Config`, not nested).

## Cargo.toml Removed

```toml
indradb-lib = "5"     # delete this line
```

No replacement dependency. DuckDB + serde + the existing `Arc<Mutex<Connection>>` give us everything.

## Error Handling

| Scenario | Behavior |
|---|---|
| `sync_memory` SQL error (lock, disk, FK) | `Err(GraphError::Backend(...))` propagates; caller decides whether to fail ingest |
| `close_edges_for_memory` SQL error during supersede | Bubbles up; supersede operation as a whole fails. Memory record was written before this point, so caller sees a partial-success-then-fail state. Document in `MemoryService::supersede` rustdoc |
| `neighbors` / `neighbors_at` / `related_memory_ids` SQL error | `Err(GraphError::Backend(...))`; retrieval path catches it and returns empty graph_boost (current behavior in `retrieve.rs` already tolerates `Err`) |
| `current_timestamp()` regression to clock skew (impossible — wall clock; we don't validate monotonicity) | Not handled. Two timestamps within the same millisecond resolve to the same string; the primary key tolerates this for distinct triples but two events on the *same* triple in the same ms would collide. Probability < 10⁻⁶ at typical write rates; if it happens, second insert is a no-op which is the right outcome |

## Testing

New file `tests/graph_temporal.rs`:

1. **`sync_creates_active_edges_for_simple_memory`** — ingest a memory with `project='foo'` and `repo='mem'`, call `neighbors(memory:<id>)`, assert two active edges (applies_to, observed_in) with `valid_to: None` and reasonable `valid_from`
2. **`sync_is_idempotent_when_called_twice`** — ingest then re-`sync_memory` for same memory, assert exactly the same edges (same count, same `valid_from` if first call's row still active)
3. **`supersede_closes_edges_for_original_memory`** — ingest v1 with project=foo; supersede with v2 having project=bar; assert v1's `applies_to project:foo` edge has `valid_to` set and v2 has a new active `applies_to project:bar`
4. **`all_edges_for_memory_returns_history`** — after the supersede in (3), `all_edges_for_memory(v1)` returns the closed edges with their `valid_to` populated
5. **`neighbors_at_filters_by_timestamp`** — capture `t0` before supersede, supersede, capture `t1` after. `neighbors_at("project:foo", t0)` includes v1; `neighbors_at("project:foo", t1)` does not
6. **`reopened_edge_creates_new_row`** — manually `close_edges_for_memory(v1)`, then `sync_memory(v1)` again. Assert two rows in `graph_edges` for the same `(from, to, relation)` triple — one closed, one active
7. **`related_memory_ids_excludes_superseded`** — ingest v1, v2 (independent), supersede v1, query `related_memory_ids(["project:foo"])`, assert v2 is present and v1 is not

Plus end-to-end smoke in an updated `tests/hybrid_search.rs` (or a new test): supersede a memory and confirm that subsequent `merge_and_rank_hybrid` graph_boost no longer includes it. This guards the contract that retrieval automatically benefits from the temporal filter without explicit code changes.

## Module Layout

**Modify**:
- `Cargo.toml` — remove `indradb-lib`
- `db/schema/` — add `003_graph.sql`
- `src/config.rs` — remove `graph_backend`, `indradb_path`, `GraphBackendKind`, env-var parsing, `InvalidGraphBackend` error variant
- `src/storage/mod.rs` — re-export `DuckDbGraphStore` and `GraphError`; drop trait re-exports
- `src/storage/graph.rs` — full rewrite (~340 LOC of IndraDB code → ~200 LOC of SQL)
- `src/storage/duckdb.rs` — extend `bootstrap()` to apply `003_graph.sql`
- `src/storage/schema.rs` — add the new schema include
- `src/domain/memory.rs` — add `valid_from` / `valid_to` to `GraphEdge`
- `src/pipeline/ingest.rs` — minor: `extract_graph_edges` constructs `GraphEdge` with the two new fields as defaults (`String::new()`, `None`); document the contract that `sync_memory` overwrites `valid_from`
- `src/service/memory_service.rs` — construct `Arc<DuckDbGraphStore>` from repo; supersede path calls `close_edges_for_memory(original.memory_id)` then `sync_memory(superseding)`
- `src/app.rs` — graph store construction simplified (drop `graph_backend` branch, `INDRADB_PATH` reading)

**Delete**:
- `tests/graph_adapter.rs` — IndraDB-specific tests, no longer applicable

**Create**:
- `db/schema/003_graph.sql`
- `tests/graph_temporal.rs`

## Out of Scope (this PR)

- `mem repair --rebuild-graph` backfill subcommand (defer)
- Tenant-scoped graph edges (defer)
- Multi-hop graph traversal API (defer)
- Archive-triggered edge close (defer; supersede is the only trigger)
- Graph-edge JSON shape backward compatibility shim (additive change is acceptable)
- Vacuuming closed edges that are old enough to discard (no automatic pruning; data grows monotonically until a future feature)

## Verification Checklist (pre-merge)

- `cargo test -q` passes (full suite + new `tests/graph_temporal.rs`)
- `cargo fmt --check` clean
- `cargo clippy --all-targets -- -D warnings` clean
- Smoke `cargo run` on a fresh DB: server starts, tracing shows no `INDRADB_PATH` lookups
- HTTP `GET /memories/{id}` JSON response contains `valid_from` / `valid_to` on each `graph_links` entry
- A manual supersede flow (via HTTP) exhibits: pre-supersede `graph/neighbors` shows v1; post-supersede shows v2 only
- Update mempalace-diff §8 row #5 with ✅ + the deviation note (DuckDB migration vs IndraDB property route)

## References

- mempalace-diff §5 (current vs MemPalace temporal-graph comparison)
- mempalace-diff §8 row #5 (the roadmap item being closed)
- `src/storage/graph.rs` — the file being entirely rewritten
- `src/pipeline/ingest.rs::extract_graph_edges` — preserved as-is, only the consumed `GraphEdge` shape changes
- `src/service/memory_service.rs:493` — supersede integration point
- `docs/superpowers/specs/2026-04-27-vector-index-sidecar-design.md` — pattern reference for "DuckDB-as-source-of-truth + per-edge transaction" model
