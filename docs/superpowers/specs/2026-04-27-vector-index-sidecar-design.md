# Vector Index Sidecar (usearch) — Design

> Closes mempalace-diff §8 #3.

## Summary

`semantic_search_memories` currently scans `memory_embeddings` linearly and computes cosine similarity in Rust. Worse, the SQL has a hard-coded `LIMIT 2000 ORDER BY updated_at DESC`, so older memories silently fall out of semantic recall — a correctness boundary, not just a performance issue.

This spec replaces the linear scan with a **`usearch` HNSW sidecar index**: a single binary index file living alongside the DuckDB database, kept in sync with `memory_embeddings` by the embedding worker. DuckDB remains the authoritative source; the sidecar is rebuildable on demand. The hard `LIMIT 2000` truncation goes away.

## Goals

- Replace the O(N) cosine scan in `DuckDbRepository::semantic_search_memories` with HNSW ANN (`usearch`)
- Eliminate the `LIMIT 2000` silent truncation
- Keep DuckDB as the authoritative source — the sidecar is reconstructable
- Maintain the public signature of `semantic_search_memories(tenant, &[f32], limit) -> Vec<(MemoryRecord, f32)>` so callers (`memory_service.rs:508`) and `merge_and_rank_hybrid` are unaffected
- Make startup deterministic: index and DuckDB are always consistent at the moment `mem serve` accepts requests

## Non-Goals

- Building a `bin/mem-repair` CLI (mempalace-diff §8 #4)
- Refactoring scoring / RRF normalization (mempalace-diff §8 #6)
- Three-stage retrieval pipeline (mempalace-diff §8 #12)
- Multi-tenant index isolation (single global index per Q2 below)
- Multi-process file locking (existing constraint: single writer per `MEM_DB_PATH`)
- Quantization / `f16` compression / HNSW tuning beyond defaults
- Prometheus / metrics surface for index health (logs only)

## Decisions

The design rests on three resolved questions:

- **Q1 — startup consistency**: synchronous rebuild from DuckDB whenever the sidecar is missing, fingerprint-mismatched, or row-count-mismatched. No linear-scan fallback at runtime. Failed rebuild = `mem serve` panics.
- **Q2 — index scope**: one global `usearch` index. Tenant + status filtering happens in DuckDB via post-ANN re-fetch. Caller picks an oversample factor to compensate for filter selectivity.
- **Crate**: `usearch` (Unum cloud) — single-file persistence, mature HNSW, SIMD/quantization headroom. `hnsw_rs` rejected for multi-file persistence and weaker maintenance; DuckDB `vss` extension rejected because it requires runtime `INSTALL` and is incompatible with the offline / cross-compile / local-first posture.

## Architecture

A new module `src/storage/vector_index.rs` lives alongside `duckdb.rs` and `graph.rs`:

```rust
pub struct VectorIndex {
    index:        Arc<RwLock<usearch::Index>>,
    id_map:       Arc<RwLock<HashMap<u64, String>>>,  // u64 hash → memory_id
    path:         PathBuf,                            // <db_path>.usearch
    meta_path:    PathBuf,                            // <db_path>.usearch.meta.json
    fingerprint:  VectorIndexFingerprint,             // (provider, model, dim)
    dirty_count:  AtomicUsize,
}
```

Public API:

- `open_or_rebuild(repo, db_path, embedding_settings) -> Self`
- `upsert(memory_id, &[f32])`
- `remove(memory_id)`
- `search(query, k) -> Vec<(memory_id, sim)>`
- `save()` — explicit flush (called periodically + on graceful shutdown)
- `size()`

### `memory_id ↔ u64` bridge

`usearch` keys are `u64`. We hash `memory_id` with sha2 truncated to 8 bytes (collision probability < 10⁻¹⁵ at our scale; collisions panic with detailed log — they signal data integrity issues, not normal operation). Reverse lookup uses an in-process `HashMap<u64, String>` persisted alongside the index file.

`upsert` semantics on a colliding key (same `memory_id`, new vector) is `index.remove(key)` then `index.add(key, vec)` — `usearch::Index::remove` is idempotent.

### File layout

Two files, always read together and written atomically:

- `<MEM_DB_PATH>.usearch` — `usearch` binary index dump
- `<MEM_DB_PATH>.usearch.meta.json` — JSON object with fields:
  - `schema_version` (integer; bump when meta layout changes incompatibly — initial value `1`)
  - `provider`, `model`, `dim` — the fingerprint triple
  - `row_count` — number of vectors in the index at save time (cheap consistency oracle on next load)
  - `id_map` — `{ "<u64-as-decimal-string>": "<memory_id>" }`

`save()` writes both to a tempfile then atomic-renames; partial writes never land on the durable filename. Either file missing or corrupt → rebuild from DuckDB.

### Module dependency

`vector_index.rs` depends on `domain::memory` types and a narrow trait — call it `EmbeddingRowSource` — exposing two operations: a batched walk over `(memory_id, embedding_blob)` pairs and a total count. The exact Rust idiom (callback, async iterator, channel) is an implementation choice; the trait surface is what matters. `DuckDbRepository` implements it; tests can swap a fake. This keeps `vector_index.rs` unit-testable in isolation and gives mempalace-diff §8 #4 / #12 a clean injection point later.

## Data Flow

### Startup — `open_or_rebuild`

```
read meta.json + load index file
  ├─ either missing / parse error                     ─┐
  ├─ meta.{provider, model, dim} != current config    ─┤
  └─ meta.row_count != repo.count_total_embeddings()  ─┤
                                                       ↓
                                                  rebuild()

rebuild():
  1. Build new index in tempdir (avoid half-written destination)
  2. Stream repo.iter_memory_embeddings(batch=512):
       for (memory_id, blob): index.add(sha2_first8(memory_id), decode_f32(blob))
  3. Atomic-rename tempfiles → <db_path>.usearch + .meta.json
  4. log!("rebuilt vector index: N rows in T ms")
```

No linear-scan fallback at runtime. If rebuild itself fails (FFI crash / disk full / DuckDB read error), `mem serve` panics — startup-class errors must be visible.

### Write — append to `embedding_worker::tick`

```
existing tick:
  ... claim job + call provider + verify content_hash ...
  repo.upsert_memory_embedding(...)         // DuckDB row durable
  vector_index.upsert(memory_id, &emb)?     // ← new
  repo.complete_embedding_job(...)
```

`upsert` internally:

1. Compute `key = sha2_first8(memory_id)`
2. Acquire `id_map` + `index` write locks (briefly)
3. `index.remove(key)` (idempotent), `index.add(key, vec)`
4. `id_map.insert(key, memory_id.to_string())` — collision (same key, different id) → panic
5. `dirty_count.fetch_add(1)`; if `>= MEM_VECTOR_INDEX_FLUSH_EVERY` (default 100), call `save()` and reset

`save()` is synchronous; no background task. At local-first scale a full dump is tens of milliseconds. Crash before save → rebuild on restart catches it.

### Delete — mirror every DuckDB delete

Every site that removes a `memory_embeddings` row must also call `vector_index.remove(memory_id)`:

- `memory_service.rs:287` (delete_memory_embedding direct call)
- `duckdb.rs:834`, `duckdb.rs:1198` (`delete_embedding_references` inside supersede / restore flows)

Test discipline: a regression test asserts `count_memory_embeddings()` and `vector_index.size()` move together across every public mutation.

### Read — rewritten `semantic_search_memories`

```rust
pub async fn semantic_search_memories(
    &self, tenant: &str, query: &[f32], limit: usize,
) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
    if query.is_empty() || limit == 0 { return Ok(vec![]); }

    // Stage 1: ANN recall with oversample
    let k = (limit * oversample_factor()).max(limit);
    let hits = self.vector_index.search(query, k).await?;
    if hits.is_empty() { return Ok(vec![]); }

    // Stage 2: DuckDB filter (tenant + status)
    let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
    let rows = self.fetch_memories_by_ids(tenant, &ids).await?;

    // Stage 3: re-attach scores, sort, truncate
    let by_id: HashMap<&str, f32> = hits.iter().map(|(i, s)| (i.as_str(), *s)).collect();
    let mut scored: Vec<_> = rows.into_iter()
        .filter_map(|m| by_id.get(m.memory_id.as_str()).map(|s| (m, *s)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    scored.truncate(limit);
    Ok(scored)
}
```

`fetch_memories_by_ids` is a new repo method: `WHERE tenant=? AND memory_id IN (?, ?, ...) AND status NOT IN ('rejected', 'archived')`. The legacy `LIMIT 2000` is gone.

## Concurrency

- `VectorIndex` uses `RwLock` for both `index` and `id_map`. Search holds reader locks; `upsert`/`remove` hold writer locks (microseconds in practice).
- `DuckDbRepository`'s existing `Arc<Mutex<Connection>>` and `VectorIndex`'s `RwLock` **never nest in the same critical section** — write paths release the DuckDB lock before acquiring the VectorIndex write lock, eliminating ordering-induced deadlock.
- The embedding worker is single-threaded (one tick at a time). HTTP search is concurrent. RwLock fits this read-heavy / writer-rare profile without starvation risk.

## Crash / Recovery Windows

| Window | Outcome |
|---|---|
| DuckDB row written, process killed before VectorIndex update | Restart: `meta.row_count != count(memory_embeddings)` → rebuild |
| VectorIndex updated + saved, DuckDB transaction never committed | DuckDB row absent → restart: row_count check rebuilds |
| `save()` partially written | Atomic rename never landed → old files persist; row_count check rebuilds anyway |
| `usearch` FFI internal panic | Process aborts → systemd / Docker restart → rebuild |

The row_count fingerprint is intentionally cheap (one SQL `count(*)`) at the cost of false positives (e.g., delete-then-reinsert across crashes can hit the same count). For paranoid mode, mempalace-diff §8 #4 introduces a content-fingerprint repair flag; this spec does not.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `MEM_VECTOR_INDEX_FLUSH_EVERY` | `100` | Trigger `save()` every N mutations |
| `MEM_VECTOR_INDEX_OVERSAMPLE` | `4` | ANN recall `k = max(limit, oversample × limit)` |
| `MEM_VECTOR_INDEX_USE_LEGACY` | unset | Emergency fallback: when set to `1`, bypasses VectorIndex and runs the preserved `legacy_semantic_search_memories` linear scan (kept for one minor release; README documents removal pending) |

`USE_LEGACY` mirrors mempalace-diff §12 risk #1's A/B switch pattern. Default off; one-release sunset.

## Cargo.toml

Add:

```toml
usearch = "2"
```

`duckdb = features=["bundled"]` already exercises C++ compilation through `cross` and Docker, so `usearch`'s C++ binding incurs no new toolchain risk.

## Error Handling Matrix

| Scenario | Behavior |
|---|---|
| Startup rebuild fails (FFI crash, disk full, DuckDB read error) | Panic; `mem serve` refuses to start |
| Runtime `upsert` / `remove` returns error | Bubble `StorageError::VectorIndex(_)` up; embedding worker reschedules the job (existing retry loop, max_retries=4) |
| Runtime `save()` fails | Log warning; `dirty_count` not reset; next mutation re-attempts; restart rebuilds |
| `usearch` FFI panic outside our catch range | Process aborts → restart → rebuild |
| `u64` hash collision (two distinct `memory_id` → same key) | Panic with full diagnostics. Probability < 10⁻¹⁵ at expected scale; a real collision indicates data corruption, not a normal case |
| Atomic rename interrupted mid-flight | Either the prior durable pair persists (subsequent run loads it; row_count check rebuilds if needed) or no files exist yet (subsequent run treats it as missing → rebuild). Half-written files cannot land at the durable filename |

## Testing

### New `tests/vector_index.rs`

Real `usearch` (FFI is the test target; not mocked):

1. `open_or_rebuild` against empty DB → empty index, files written
2. `open_or_rebuild` against a DB pre-populated with N embeddings → `size() == N`
3. Upsert same `memory_id` with new vector → search returns new vector, not old
4. Remove → search no longer returns
5. **Fingerprint mismatch**: hand-write meta `dim: 128`, open with config `dim: 256` → triggers rebuild
6. **Row-count mismatch**: bypass service, delete a row directly in DuckDB → next `open_or_rebuild` rebuilds
7. **File corruption**: truncate `.usearch` to 0 bytes → rebuild
8. **Concurrency smoke**: tokio fires 10 concurrent searches + 1 upsert loop → no panic, results are consistent snapshots
9. **Legacy fallback**: with `MEM_VECTOR_INDEX_USE_LEGACY=1`, `semantic_search_memories` skips VectorIndex and produces equivalent top-1 to the ANN path on a small fixture

### Updates to existing tests

- `tests/embedding_worker.rs` — three existing cases assert `vector_index.size()` increments after each `tick`
- `tests/search_api.rs` / `tests/hybrid_search.rs` — verify top-1 doesn't regress (small-N HNSW returns exact results, so this guards oversample factor + filter logic)
- Supersede tests assert old `memory_id` not in `vector_index` after the operation

### Test infra

A new helper `tests/common/storage.rs::open_repo_with_index(dir)` standardizes setup so per-test sidecar paths are consistent and tempdir cleanup is automatic.

## Change Inventory

**New files**:

- `src/storage/vector_index.rs` (~400 LOC)
- `tests/vector_index.rs` (~200 LOC)
- `tests/common/storage.rs` (helper)

**Modified files**:

- `Cargo.toml` — add `usearch = "2"`
- `src/storage/mod.rs` — re-export `VectorIndex` and `VectorIndexError`
- `src/storage/duckdb.rs` — add `iter_memory_embeddings`, `count_total_memory_embeddings`, `fetch_memories_by_ids`; rewrite `semantic_search_memories` body; preserve `legacy_semantic_search_memories` under `MEM_VECTOR_INDEX_USE_LEGACY` flag
- `src/service/embedding_worker.rs` — call `vector_index.upsert` after `upsert_memory_embedding`
- `src/service/memory_service.rs` — accept `Arc<VectorIndex>` in constructor; mirror `vector_index.remove` in three delete sites
- `src/app.rs` — `VectorIndex::open_or_rebuild` in startup sequence; inject into service + worker
- `src/config.rs` — three new env vars
- `tests/embedding_worker.rs`, `tests/search_api.rs`, `tests/hybrid_search.rs` — assertions added
- `docs/mempalace-diff.md` — once landed, mark §8 row #3 ✅

## Rollout

First `mem serve` start with this code on an existing DB: no sidecar files exist → `open_or_rebuild` reads all `memory_embeddings` rows, builds a fresh index, writes both files, continues. Cost ≈ `count(memory_embeddings) × O(log N)` HNSW inserts plus one dump. For 10k rows expect under five seconds. No DuckDB schema migration is required.

## Verification Checklist (pre-merge)

- `cargo test -q` — all suites pass, including new `tests/vector_index.rs`
- `cargo fmt --check` — required by CI
- `cargo clippy --all-targets -- -D warnings` — required by CI
- Manual smoke on a non-empty DB: `cargo run` → first request returns matching memory; sidecar files written
- Set `MEM_VECTOR_INDEX_USE_LEGACY=1` and confirm legacy path still produces equivalent top-1 (regression safety net)
- Inspect logs on startup for `rebuilt vector index: N rows in T ms`

## References

- mempalace-diff §3 / §8 #3 (problem framing + roadmap entry)
- mempalace-diff §8 #4 (companion repair CLI; out of scope here)
- mempalace-diff §8 #12 (downstream three-stage retrieval; this spec leaves the API stable for that work)
- `src/storage/duckdb.rs:536` — current linear-scan implementation being replaced
- `src/service/memory_service.rs:508` — sole caller of `semantic_search_memories`
- `src/service/embedding_worker.rs:135` — the write point this spec extends
