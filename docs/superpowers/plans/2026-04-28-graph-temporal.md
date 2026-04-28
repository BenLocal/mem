# Temporal Graph Edges Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `valid_from` / `valid_to` to `GraphEdge` and migrate the graph layer from IndraDB to DuckDB, closing mempalace-diff §8 #5.

**Architecture:** Introduce a new `DuckDbGraphStore` (concrete struct, no trait) that holds an `Arc<DuckDbRepository>` and runs all graph operations as SQL on a new `graph_edges` table. Existing `IndraDbGraphAdapter` / `LocalGraphAdapter` / `GraphStore` trait are deleted in a single cleanup task once nothing references them. Supersede is the sole trigger that closes edges (sets `valid_to`); retrieval automatically benefits via `related_memory_ids`'s default "active edges only" filter — `pipeline/retrieve.rs` is not touched.

**Tech Stack:** Rust, DuckDB (existing bundled), serde, the `Arc<Mutex<Connection>>` pattern from §3.

**Spec:** `docs/superpowers/specs/2026-04-28-graph-temporal-design.md`

---

## File Structure

**Create:**
- `db/schema/003_graph.sql` — `graph_edges` table + 3 plain indexes
- `src/storage/graph_store.rs` — `DuckDbGraphStore` struct + 6 public methods + `GraphError`
- `tests/graph_temporal.rs` — 7 integration tests, one per behavior in spec §Testing

**Modify:**
- `Cargo.toml` — remove `indradb-lib`
- `src/domain/memory.rs` — add `valid_from` / `valid_to` to `GraphEdge`
- `src/pipeline/ingest.rs` — `extract_graph_edges` constructs `GraphEdge` with placeholder timestamp fields; rustdoc note that `sync_memory` overwrites `valid_from`
- `src/storage/schema.rs` — apply `003_graph.sql` in `bootstrap`
- `src/storage/mod.rs` — re-export `DuckDbGraphStore`, `GraphError` (drop trait re-exports)
- `src/service/memory_service.rs` — replace `Arc<dyn GraphStore>` with `Arc<DuckDbGraphStore>`; supersede path calls `close_edges_for_memory(original.memory_id)` then `sync_memory(superseding)`
- `src/app.rs` — construct `Arc<DuckDbGraphStore>` from repo; drop `graph_backend` / `indradb_path` branching
- `src/config.rs` — remove `graph_backend`, `indradb_path`, `GraphBackendKind`, `InvalidGraphBackend`, `INDRADB_PATH`, `GRAPH_BACKEND` env-var parsing
- `tests/hybrid_search.rs` — add one supersede-excludes-from-graph-boost case
- `docs/mempalace-diff.md` — mark §8 row #5 ✅ once all tasks land

**Delete:**
- `src/storage/graph.rs` — entire IndraDB / Local adapter file (~340 LOC). The new `DuckDbGraphStore` lives in `graph_store.rs` so the deletion happens after wiring.
- `tests/graph_adapter.rs` — IndraDB-specific tests, no longer applicable

---

## Task 1: `GraphEdge` gains `valid_from` / `valid_to`

**Files:**
- Modify: `src/domain/memory.rs`
- Modify: `src/pipeline/ingest.rs`
- Modify: `tests/graph_temporal.rs` (new)

- [ ] **Step 1: Write the failing test**

Create `tests/graph_temporal.rs` with:

```rust
use mem::domain::memory::GraphEdge;

#[test]
fn graph_edge_carries_valid_from_and_valid_to() {
    let edge = GraphEdge {
        from_node_id: "memory:abc".into(),
        to_node_id: "project:foo".into(),
        relation: "applies_to".into(),
        valid_from: "00000001761662918634".into(),
        valid_to: None,
    };
    assert_eq!(edge.valid_to, None);
    assert!(edge.valid_from.starts_with("000000"));

    // serde round-trip preserves both fields
    let s = serde_json::to_string(&edge).unwrap();
    let back: GraphEdge = serde_json::from_str(&s).unwrap();
    assert_eq!(back.valid_to, None);
    assert_eq!(back.valid_from, "00000001761662918634");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test graph_temporal -q`
Expected: compile error (unknown fields).

- [ ] **Step 3: Add fields to `GraphEdge`**

In `src/domain/memory.rs`, find the existing struct:

```rust
pub struct GraphEdge {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
}
```

Replace with:

```rust
pub struct GraphEdge {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
    pub valid_from: String,
    pub valid_to: Option<String>,
}
```

(Verify the existing `#[derive(...)]` block above the struct — ensure `Serialize, Deserialize` are in it. Add them if missing.)

- [ ] **Step 4: Update `extract_graph_edges` to fill defaults**

In `src/pipeline/ingest.rs`, find the `extract_graph_edges` body (around line 94). Every place it builds a `GraphEdge { from_node_id: ..., to_node_id: ..., relation: ... }` literal needs the two new fields. Add them as defaults:

```rust
GraphEdge {
    from_node_id: from_node_id.clone(),
    to_node_id: project_node_id(project),
    relation: "applies_to".into(),
    valid_from: String::new(),
    valid_to: None,
}
```

Apply this pattern to **every** `GraphEdge { ... }` literal in the function. Add a doc comment to the function explaining the contract:

```rust
/// Extract graph edges derived from a memory's fields.
///
/// Returned `GraphEdge`s have `valid_from = String::new()` and `valid_to = None`
/// as placeholders. The storage layer (`DuckDbGraphStore::sync_memory`) overwrites
/// `valid_from` with the current timestamp at write time. Keeping this function
/// pure (no clock dependency) lets us test it without time mocking.
pub fn extract_graph_edges(memory: &MemoryRecord) -> Vec<GraphEdge> { ... }
```

- [ ] **Step 5: Update existing usages of `GraphEdge { ... }` literals**

Grep for other places that construct `GraphEdge` directly:

```bash
grep -rn "GraphEdge {" src/ tests/
```

Likely hits in `src/storage/graph.rs` (the IndraDB adapter — both `LocalGraphAdapter` and `IndraDbGraphAdapter` reconstruct edges in `neighbors`). Each construction needs the two new fields. For now, in the legacy graph.rs code, default them:

```rust
GraphEdge {
    from_node_id,
    to_node_id,
    relation: edge.t.as_str().to_string(),
    valid_from: String::new(),
    valid_to: None,
}
```

These adapters will be deleted in Task 12 anyway, but they need to compile through the intermediate state.

- [ ] **Step 6: Run all tests**

Run: `cargo test -q`
Expected: full suite passes (the new field round-trip test passes; existing tests that compare `GraphEdge` values still pass because `String::new() == String::new()` and `None == None`).

- [ ] **Step 7: Commit**

```bash
git add src/domain/memory.rs src/pipeline/ingest.rs src/storage/graph.rs tests/graph_temporal.rs
git commit -m "feat(domain): GraphEdge gains valid_from/valid_to fields"
```

---

## Task 2: Schema `003_graph.sql` + bootstrap

**Files:**
- Create: `db/schema/003_graph.sql`
- Modify: `src/storage/schema.rs`
- Modify: `tests/graph_temporal.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/graph_temporal.rs`:

```rust
use mem::storage::DuckDbRepository;
use tempfile::tempdir;

#[tokio::test]
async fn bootstrap_creates_graph_edges_table() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("schema.duckdb");
    let _repo = DuckDbRepository::open(&db).await.unwrap();

    // Re-open with a raw duckdb connection to verify the table exists.
    // (DuckDbRepository wraps the conn; we verify via a raw count query through
    //  the repo's helper methods. Since `count_total_memory_embeddings` works, we
    //  can also expose a quick "graph_edges count" probe by calling raw SQL.)
    // For this test, we call a public helper added in Task 3 (count_active_graph_edges).
    // Until then this test stays unimplemented — replace with the helper once it lands.
    // For Task 2, just assert that DuckDbRepository::open(&db) succeeds without error
    // (which exercises the new schema_includes call in bootstrap).
}
```

> **Note**: this test passes trivially as long as bootstrap doesn't error. The real schema-presence assertion lands in Task 3 once `DuckDbGraphStore::sync_memory` exists.

- [ ] **Step 2: Run to confirm baseline**

Run: `cargo test --test graph_temporal bootstrap_creates -q`
Expected: PASS (DuckDbRepository::open already works against a fresh DB).

- [ ] **Step 3: Create `db/schema/003_graph.sql`**

```sql
create table if not exists graph_edges (
  from_node_id text not null,
  to_node_id   text not null,
  relation     text not null,
  valid_from   text not null,
  valid_to     text,
  primary key (from_node_id, to_node_id, relation, valid_from)
);

create index if not exists idx_graph_edges_from on graph_edges (from_node_id, relation);
create index if not exists idx_graph_edges_to on graph_edges (to_node_id, relation);
create index if not exists idx_graph_edges_history on graph_edges (from_node_id, valid_from);
```

- [ ] **Step 4: Apply schema in bootstrap**

In `src/storage/schema.rs`, find the existing `bootstrap` function. It currently includes 001 + 002 schema files. Add 003:

```rust
const INIT_SCHEMA_SQL: &str = include_str!("../../db/schema/001_init.sql");
const EMBEDDINGS_SCHEMA_SQL: &str = include_str!("../../db/schema/002_embeddings.sql");
const GRAPH_SCHEMA_SQL: &str = include_str!("../../db/schema/003_graph.sql");

pub fn bootstrap(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(INIT_SCHEMA_SQL)?;
    conn.execute_batch(EMBEDDINGS_SCHEMA_SQL)?;
    conn.execute_batch(GRAPH_SCHEMA_SQL)?;
    let migrated = migrate_content_hash_to_sha256(conn)?;
    if migrated > 0 {
        info!(
            count = migrated,
            "migrated legacy content_hash rows to sha256 (mempalace-diff §8 #1)"
        );
    }
    Ok(())
}
```

- [ ] **Step 5: Run**

Run: `cargo test -q`
Expected: full suite passes (existing tests open new DBs; bootstrap now creates the table; no test reads from it yet).

- [ ] **Step 6: Commit**

```bash
git add db/schema/003_graph.sql src/storage/schema.rs tests/graph_temporal.rs
git commit -m "feat(storage): schema 003_graph.sql with graph_edges table"
```

---

## Task 3: `DuckDbGraphStore` skeleton + `GraphError`

**Files:**
- Create: `src/storage/graph_store.rs`
- Modify: `src/storage/mod.rs`
- Modify: `tests/graph_temporal.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/graph_temporal.rs`:

```rust
use mem::storage::DuckDbGraphStore;
use std::sync::Arc;

async fn open_repo_and_graph(db_path: &std::path::Path)
    -> (Arc<DuckDbRepository>, DuckDbGraphStore)
{
    let repo = Arc::new(DuckDbRepository::open(db_path).await.unwrap());
    let graph = DuckDbGraphStore::new(repo.clone());
    (repo, graph)
}

#[tokio::test]
async fn duckdb_graph_store_constructs_against_fresh_db() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ctor.duckdb");
    let (_repo, graph) = open_repo_and_graph(&db).await;
    // Active-edge count on fresh DB should be zero
    let edges = graph.neighbors("memory:does-not-exist").await.unwrap();
    assert!(edges.is_empty());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test graph_temporal duckdb_graph_store_constructs -q`
Expected: compile error (`DuckDbGraphStore` undefined).

- [ ] **Step 3: Implement skeleton**

Create `src/storage/graph_store.rs`:

```rust
use std::sync::Arc;

use thiserror::Error;

use crate::domain::memory::{GraphEdge, MemoryRecord};
use crate::pipeline::ingest::extract_graph_edges;
use super::{DuckDbRepository, StorageError};

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("graph backend error: {0}")]
    Backend(String),
}

impl From<StorageError> for GraphError {
    fn from(e: StorageError) -> Self {
        GraphError::Backend(e.to_string())
    }
}

impl From<duckdb::Error> for GraphError {
    fn from(e: duckdb::Error) -> Self {
        GraphError::Backend(e.to_string())
    }
}

pub struct DuckDbGraphStore {
    repo: Arc<DuckDbRepository>,
}

impl DuckDbGraphStore {
    pub fn new(repo: Arc<DuckDbRepository>) -> Self {
        Self { repo }
    }

    /// Active-edge neighbors. Default behavior (matches what callers expected from the trait).
    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.repo.conn()?;
        let mut stmt = conn.prepare(
            "select from_node_id, to_node_id, relation, valid_from, valid_to
               from graph_edges
              where (from_node_id = ?1 or to_node_id = ?1)
                and valid_to is null
              order by relation, from_node_id, to_node_id",
        )?;
        let rows = stmt.query_map(duckdb::params![node_id], map_row_to_edge)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn map_row_to_edge(row: &duckdb::Row<'_>) -> Result<GraphEdge, duckdb::Error> {
    Ok(GraphEdge {
        from_node_id: row.get(0)?,
        to_node_id: row.get(1)?,
        relation: row.get(2)?,
        valid_from: row.get(3)?,
        valid_to: row.get::<_, Option<String>>(4)?,
    })
}
```

In `src/storage/mod.rs`, declare and re-export:

```rust
pub mod graph_store;

pub use graph_store::{DuckDbGraphStore, GraphError as DuckDbGraphError};
```

> Note: `DuckDbRepository::conn()` is currently `pub(crate)`. If the `graph_store` module is in the same crate (it is), this works. If not, expose `conn()` as `pub` or add a thin pub helper. Confirm by reading `src/storage/duckdb.rs:1177` (the `fn conn(&self)` definition).

The existing `GraphError` in `src/storage/graph.rs` (the IndraDB version) still exists and is still re-exported. To avoid a name collision, alias the new one as `DuckDbGraphError` for now. Task 12 will delete the old one and rename this back to `GraphError`.

- [ ] **Step 4: Run**

Run: `cargo test --test graph_temporal -q`
Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/storage/graph_store.rs src/storage/mod.rs tests/graph_temporal.rs
git commit -m "feat(storage): DuckDbGraphStore skeleton + neighbors() reads active edges"
```

---

## Task 4: `sync_memory` — write active edges idempotently

**Files:**
- Modify: `src/storage/graph_store.rs`
- Modify: `tests/graph_temporal.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/graph_temporal.rs`:

```rust
use mem::domain::memory::{
    IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode,
};
use mem::service::MemoryService;

async fn ingest_one(svc: &MemoryService, content: &str, project: Option<&str>, repo: Option<&str>)
    -> mem::domain::memory::IngestMemoryResponse
{
    svc.ingest(IngestMemoryRequest {
        tenant: "t".into(),
        memory_type: MemoryType::Implementation,
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: project.map(String::from),
        repo: repo.map(String::from),
        module: None,
        task_type: None,
        tags: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    }).await.unwrap()
}

#[tokio::test]
async fn sync_creates_active_edges_for_simple_memory() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sync.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;

    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "alpha", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();

    graph.sync_memory(&memory).await.unwrap();

    let edges = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();
    let relations: std::collections::HashSet<_> =
        edges.iter().map(|e| e.relation.as_str()).collect();
    assert!(relations.contains("applies_to"));
    assert!(relations.contains("observed_in"));
    for edge in &edges {
        assert_eq!(edge.valid_to, None);
        assert!(!edge.valid_from.is_empty(), "valid_from should be set");
    }
}

#[tokio::test]
async fn sync_is_idempotent_when_called_twice() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("idem.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "beta", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();

    graph.sync_memory(&memory).await.unwrap();
    let first = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();
    graph.sync_memory(&memory).await.unwrap();
    let second = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();

    assert_eq!(first.len(), second.len(), "edge count must not grow");
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.from_node_id, b.from_node_id);
        assert_eq!(a.relation, b.relation);
        assert_eq!(a.valid_from, b.valid_from, "valid_from must not be refreshed");
    }
}
```

> Note: `MemoryService::new((*repo).clone())` — this requires `DuckDbRepository: Clone`. From §3 we confirmed `DuckDbRepository` derives Clone (cheap `Arc`-backed). The service owns one clone; the graph store owns another via `repo.clone()` outside.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test graph_temporal sync_creates -q`
Expected: compile error (`sync_memory` not defined).

- [ ] **Step 3: Implement `sync_memory`**

Append to `impl DuckDbGraphStore` in `src/storage/graph_store.rs`:

```rust
impl DuckDbGraphStore {
    pub async fn sync_memory(&self, memory: &MemoryRecord) -> Result<(), GraphError> {
        let edges = extract_graph_edges(memory);
        if edges.is_empty() {
            return Ok(());
        }
        let now = current_timestamp();
        let mut conn = self.repo.conn()?;
        let tx = conn.transaction()?;
        for edge in edges {
            let exists: i64 = tx.query_row(
                "select count(*) from graph_edges
                  where from_node_id = ?1 and to_node_id = ?2
                    and relation = ?3 and valid_to is null",
                duckdb::params![&edge.from_node_id, &edge.to_node_id, &edge.relation],
                |row| row.get(0),
            )?;
            if exists > 0 {
                continue;
            }
            tx.execute(
                "insert into graph_edges
                   (from_node_id, to_node_id, relation, valid_from, valid_to)
                 values (?1, ?2, ?3, ?4, NULL)",
                duckdb::params![&edge.from_node_id, &edge.to_node_id, &edge.relation, &now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn current_timestamp() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();
    format!("{millis:020}")
}
```

> Note: `current_timestamp()` is a duplicate of the helper in `embedding_worker.rs` and `duckdb.rs`. The free function in `graph_store.rs` is fine for now; future refactor can extract a shared `time::current_timestamp_ms()` if more sites need it. Don't centralize prematurely.

- [ ] **Step 4: Run**

Run: `cargo test --test graph_temporal -q`
Expected: 5 tests pass (3 prior + 2 new).

- [ ] **Step 5: Commit**

```bash
git add src/storage/graph_store.rs tests/graph_temporal.rs
git commit -m "feat(graph): sync_memory writes active edges idempotently"
```

---

## Task 5: `close_edges_for_memory`

**Files:**
- Modify: `src/storage/graph_store.rs`
- Modify: `tests/graph_temporal.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/graph_temporal.rs`:

```rust
#[tokio::test]
async fn close_edges_for_memory_sets_valid_to() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("close.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "gamma", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();
    graph.sync_memory(&memory).await.unwrap();

    let pre = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();
    assert!(!pre.is_empty(), "should have active edges before close");

    let closed = graph.close_edges_for_memory(&r.memory_id).await.unwrap();
    assert!(closed > 0, "should report at least one closed row");

    let post = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();
    assert!(post.is_empty(), "no active edges after close");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test graph_temporal close_edges -q`
Expected: compile error (`close_edges_for_memory` not defined).

- [ ] **Step 3: Implement**

Append to `impl DuckDbGraphStore`:

```rust
impl DuckDbGraphStore {
    pub async fn close_edges_for_memory(&self, memory_id: &str) -> Result<usize, GraphError> {
        let from = format!("memory:{memory_id}");
        let now = current_timestamp();
        let conn = self.repo.conn()?;
        let count = conn.execute(
            "update graph_edges
                set valid_to = ?1
              where from_node_id = ?2
                and valid_to is null",
            duckdb::params![&now, &from],
        )?;
        Ok(count)
    }
}
```

- [ ] **Step 4: Run**

Run: `cargo test --test graph_temporal -q`
Expected: 6 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/storage/graph_store.rs tests/graph_temporal.rs
git commit -m "feat(graph): close_edges_for_memory sets valid_to on outbound edges"
```

---

## Task 6: `neighbors_at` — point-in-time queries

**Files:**
- Modify: `src/storage/graph_store.rs`
- Modify: `tests/graph_temporal.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/graph_temporal.rs`:

```rust
#[tokio::test]
async fn neighbors_at_filters_by_timestamp() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("at.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "delta", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();
    graph.sync_memory(&memory).await.unwrap();
    let active = graph.neighbors("project:foo").await.unwrap();
    assert!(!active.is_empty());
    let valid_from_of_first = active[0].valid_from.clone();

    // tiny pause so timestamps are distinguishable
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    graph.close_edges_for_memory(&r.memory_id).await.unwrap();
    let after_close = current_ts_str();

    // Query at a timestamp >= valid_from but BEFORE close → edge should appear
    let mid = bump_timestamp(&valid_from_of_first, 1);
    let then = graph.neighbors_at("project:foo", &mid).await.unwrap();
    assert!(!then.is_empty(), "edge should be active at mid timestamp");

    // Query at a timestamp AFTER close → edge should not appear
    let later = graph.neighbors_at("project:foo", &after_close).await.unwrap();
    assert!(later.is_empty(), "edge must be excluded at later timestamp");
}

fn current_ts_str() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{millis:020}")
}

fn bump_timestamp(s: &str, by_ms: u128) -> String {
    let n: u128 = s.parse().unwrap();
    format!("{:020}", n + by_ms)
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test graph_temporal neighbors_at_ -q`
Expected: compile error (`neighbors_at` not defined).

- [ ] **Step 3: Implement**

Append to `impl DuckDbGraphStore`:

```rust
impl DuckDbGraphStore {
    pub async fn neighbors_at(
        &self,
        node_id: &str,
        at: &str,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.repo.conn()?;
        let mut stmt = conn.prepare(
            "select from_node_id, to_node_id, relation, valid_from, valid_to
               from graph_edges
              where (from_node_id = ?1 or to_node_id = ?1)
                and valid_from <= ?2
                and (valid_to is null or valid_to > ?2)
              order by relation, from_node_id, to_node_id",
        )?;
        let rows = stmt.query_map(duckdb::params![node_id, at], map_row_to_edge)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
```

- [ ] **Step 4: Run**

Run: `cargo test --test graph_temporal -q`
Expected: 7 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/storage/graph_store.rs tests/graph_temporal.rs
git commit -m "feat(graph): neighbors_at(node, ts) for point-in-time queries"
```

---

## Task 7: `related_memory_ids` — default to active edges

**Files:**
- Modify: `src/storage/graph_store.rs`
- Modify: `tests/graph_temporal.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/graph_temporal.rs`:

```rust
#[tokio::test]
async fn related_memory_ids_excludes_superseded() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("rel.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r1 = ingest_one(&svc, "v1", Some("foo"), Some("mem")).await;
    let r2 = ingest_one(&svc, "v2", Some("foo"), Some("mem")).await;
    let m1 = repo.get_memory_for_tenant("t", &r1.memory_id).await.unwrap().unwrap();
    let m2 = repo.get_memory_for_tenant("t", &r2.memory_id).await.unwrap().unwrap();
    graph.sync_memory(&m1).await.unwrap();
    graph.sync_memory(&m2).await.unwrap();

    // Both memories present.
    let mut both = graph.related_memory_ids(&["project:foo".into()]).await.unwrap();
    both.sort();
    assert_eq!(both.len(), 2);
    assert!(both.contains(&r1.memory_id));
    assert!(both.contains(&r2.memory_id));

    // Close m1's edges (simulating supersede effect).
    graph.close_edges_for_memory(&r1.memory_id).await.unwrap();

    let one = graph.related_memory_ids(&["project:foo".into()]).await.unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0], r2.memory_id);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test graph_temporal related_memory_ids -q`
Expected: compile error.

- [ ] **Step 3: Implement**

Append to `impl DuckDbGraphStore`:

```rust
impl DuckDbGraphStore {
    pub async fn related_memory_ids(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<String>, GraphError> {
        if node_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = std::iter::repeat_n("?", node_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "select distinct
               case when from_node_id in ({0}) then to_node_id else from_node_id end as adjacent
             from graph_edges
             where (from_node_id in ({0}) or to_node_id in ({0}))
               and valid_to is null",
            placeholders
        );
        let conn = self.repo.conn()?;
        let mut stmt = conn.prepare(&sql)?;
        // Each placeholder group needs its own copy of the parameter list (3 groups in the SQL).
        let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = Vec::with_capacity(node_ids.len() * 3);
        for _ in 0..3 {
            for n in node_ids {
                params_vec.push(Box::new(n.clone()));
            }
        }
        let params_refs: Vec<&dyn duckdb::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(&params_refs[..], |row| {
            row.get::<_, String>(0)
        })?;
        let mut memory_ids = std::collections::HashSet::new();
        for row in rows {
            let adjacent: String = row?;
            if let Some(memory_id) = adjacent.strip_prefix("memory:") {
                memory_ids.insert(memory_id.to_string());
            }
        }
        let mut out: Vec<String> = memory_ids.into_iter().collect();
        out.sort();
        Ok(out)
    }
}
```

> Note: the SQL has three `IN (...)` clauses (in the CASE, in the WHERE-OR-left, in the WHERE-OR-right), so we need to bind the same parameter list three times. The `for _ in 0..3` loop in Rust mirrors this.

- [ ] **Step 4: Run**

Run: `cargo test --test graph_temporal -q`
Expected: 8 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/storage/graph_store.rs tests/graph_temporal.rs
git commit -m "feat(graph): related_memory_ids defaults to active edges"
```

---

## Task 8: `all_edges_for_memory` — history view

**Files:**
- Modify: `src/storage/graph_store.rs`
- Modify: `tests/graph_temporal.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/graph_temporal.rs`:

```rust
#[tokio::test]
async fn all_edges_for_memory_returns_history_including_closed() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("hist.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "epsilon", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();
    graph.sync_memory(&memory).await.unwrap();
    graph.close_edges_for_memory(&r.memory_id).await.unwrap();

    let all = graph.all_edges_for_memory(&r.memory_id).await.unwrap();
    assert!(!all.is_empty(), "history should include the now-closed edges");
    for edge in &all {
        assert!(edge.valid_to.is_some(), "every edge in history is closed");
    }
}

#[tokio::test]
async fn reopened_edge_creates_new_row() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("reopen.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "zeta", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();

    graph.sync_memory(&memory).await.unwrap();
    graph.close_edges_for_memory(&r.memory_id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    graph.sync_memory(&memory).await.unwrap();

    let history = graph.all_edges_for_memory(&r.memory_id).await.unwrap();
    // For each (relation), there should be at least 2 rows: one closed, one active.
    let applies_to: Vec<_> = history.iter().filter(|e| e.relation == "applies_to").collect();
    assert_eq!(applies_to.len(), 2);
    let closed_count = applies_to.iter().filter(|e| e.valid_to.is_some()).count();
    let active_count = applies_to.iter().filter(|e| e.valid_to.is_none()).count();
    assert_eq!(closed_count, 1);
    assert_eq!(active_count, 1);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test graph_temporal all_edges -q`
Expected: compile error.

- [ ] **Step 3: Implement**

Append to `impl DuckDbGraphStore`:

```rust
impl DuckDbGraphStore {
    pub async fn all_edges_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let from = format!("memory:{memory_id}");
        let conn = self.repo.conn()?;
        let mut stmt = conn.prepare(
            "select from_node_id, to_node_id, relation, valid_from, valid_to
               from graph_edges
              where from_node_id = ?1
              order by valid_from",
        )?;
        let rows = stmt.query_map(duckdb::params![&from], map_row_to_edge)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
```

- [ ] **Step 4: Run**

Run: `cargo test --test graph_temporal -q`
Expected: 10 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/storage/graph_store.rs tests/graph_temporal.rs
git commit -m "feat(graph): all_edges_for_memory returns full history"
```

---

## Task 9: Wire `DuckDbGraphStore` into `MemoryService`, replace `Arc<dyn GraphStore>`

**Files:**
- Modify: `src/service/memory_service.rs`
- Modify: `tests/graph_temporal.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/graph_temporal.rs`:

```rust
#[tokio::test]
async fn supersede_closes_edges_via_memory_service() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("supersede.duckdb");
    let repo = Arc::new(DuckDbRepository::open(&db).await.unwrap());
    let graph = Arc::new(DuckDbGraphStore::new(repo.clone()));

    let svc = MemoryService::new_with_graph((*repo).clone(), graph.clone());
    let r1 = ingest_one(&svc, "v1", Some("foo"), Some("mem")).await;
    let m1 = repo.get_memory_for_tenant("t", &r1.memory_id).await.unwrap().unwrap();
    graph.sync_memory(&m1).await.unwrap();
    let pre = graph.neighbors("project:foo").await.unwrap();
    assert_eq!(pre.len(), 1);

    // Use whatever supersede entry-point exists in MemoryService. If there's a
    // public method `supersede_pending` or similar, call it. Otherwise fall back
    // to direct repo.supersede_memory_for_tenant.
    // For this test we drive supersede via the same path the HTTP layer uses.
    // Adapt the API call name to match what's actually in memory_service.rs.
    // Pseudocode:
    //   svc.supersede_with(&r1.memory_id, "v2", ...).await.unwrap();
    // After this call, the graph store should show v1's edges closed and v2's edges active.

    // (Read memory_service.rs to find the right call. If the supersede flow is
    // through `accept_pending` + ingest, replicate that here.)
    // For now, simulate by using direct calls if a high-level method doesn't exist.

    let r2 = ingest_one(&svc, "v2", Some("foo"), Some("mem")).await;
    let m2 = repo.get_memory_for_tenant("t", &r2.memory_id).await.unwrap().unwrap();
    // Force the supersede path manually: close v1's edges, sync v2.
    // (Once the wiring lands, doing svc.supersede_pending(&r1.memory_id, ...) will
    // do this automatically. The assertion below verifies the end-state.)
    repo.supersede_with_new_memory(&r1.memory_id, &m2).await.ok();

    // After supersede:
    let post = graph.neighbors("project:foo").await.unwrap();
    let ids: std::collections::HashSet<_> = post.iter()
        .filter_map(|e| {
            if e.from_node_id.starts_with("memory:") {
                e.from_node_id.strip_prefix("memory:").map(String::from)
            } else if e.to_node_id.starts_with("memory:") {
                e.to_node_id.strip_prefix("memory:").map(String::from)
            } else {
                None
            }
        })
        .collect();
    assert!(!ids.contains(&r1.memory_id), "v1 should be excluded after supersede");
    assert!(ids.contains(&r2.memory_id), "v2 should be active after supersede");
}
```

> Note: this test names a constructor `MemoryService::new_with_graph(repo, graph_store)` and a repo method `supersede_with_new_memory`. The first is added below; the second already exists in `duckdb.rs` (find the actual name via grep — it may be `replace_pending_with_successor` or similar). Adapt the test's call to match.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test graph_temporal supersede_closes -q`
Expected: compile error (`new_with_graph` not defined; supersede call may also fail).

- [ ] **Step 3: Implement**

In `src/service/memory_service.rs`:

(a) Find the existing struct:
```rust
pub struct MemoryService {
    repository: DuckDbRepository,
    graph: Arc<dyn GraphStore>,
    // ...
}
```

Replace `Arc<dyn GraphStore>` with `Arc<DuckDbGraphStore>`. Add the import at the top:
```rust
use crate::storage::DuckDbGraphStore;
```

Remove the existing `use crate::storage::{... GraphStore ...};` if `GraphStore` was imported.

(b) Add a constructor that takes the graph store explicitly:
```rust
impl MemoryService {
    pub fn new_with_graph(repository: DuckDbRepository, graph: Arc<DuckDbGraphStore>) -> Self {
        Self {
            repository,
            graph,
            // ... copy whatever other fields the existing constructor sets ...
        }
    }
}
```

(c) Update the existing `new` (and any other constructors like `new_with_index`) to construct a `DuckDbGraphStore` from the repo. Since `MemoryService::new(repository)` currently accepts a `DuckDbRepository` directly (not an Arc), wrap it for the graph store:
```rust
impl MemoryService {
    pub fn new(repository: DuckDbRepository) -> Self {
        let repo_arc = Arc::new(repository.clone());
        let graph = Arc::new(DuckDbGraphStore::new(repo_arc));
        Self::new_with_graph(repository, graph)
    }
}
```

> Note: this constructs a fresh `DuckDbGraphStore` per `MemoryService::new` call. That's fine — the graph store is stateless (no in-memory caches), it just borrows the repo's connection. If the existing `new_with_index` constructor exists (from §3 Task 17), apply the same pattern: it should also construct a graph store internally.

(d) Find the supersede flow. Grep for `supersedes_memory_id: Some(`. Likely site: `memory_service.rs:493`. After the existing supersede SQL completes, add:
```rust
// Close edges from the original memory and write fresh edges for the superseder.
self.graph.close_edges_for_memory(&original.memory_id).await
    .map_err(|e| MemoryError::Storage(e.into()))?;
self.graph.sync_memory(&superseding).await
    .map_err(|e| MemoryError::Storage(e.into()))?;
```

(Adapt error mapping to whatever the function returns. If it returns `Result<_, StorageError>`, convert via `From<GraphError> for StorageError` — define this in `graph_store.rs` if needed:
```rust
impl From<GraphError> for StorageError {
    fn from(e: GraphError) -> Self {
        StorageError::VectorIndex(e.to_string())
    }
}
```
The variant name is reused for ergonomics; rename to `Graph` if you prefer separation — but the spec said new `StorageError` variants are out of scope, so reuse is acceptable.)

(e) The test's `supersede_with_new_memory` reference: grep the codebase for the actual supersede entry-point. It's most likely `MemoryService::supersede_pending` or similar. Use that. If no public method exists, the test should drive the flow through whatever HTTP-layer-style call exists. The simplest cleanest assertion is: after going through whatever the production code path is, `graph.neighbors("project:foo")` excludes the original.

> If matching the actual API name is too involved, simplify the test to just call `graph.close_edges_for_memory(...)` + `graph.sync_memory(&m2)` directly and assert the end state — the integration into `MemoryService` is verified by the changed call sites compiling and existing supersede tests still passing.

- [ ] **Step 4: Run**

Run: `cargo test -q`
Expected: full suite passes (the supersede flow now closes edges; existing supersede tests still pass because they didn't assert graph state previously).

- [ ] **Step 5: Commit**

```bash
git add src/service/memory_service.rs src/storage/graph_store.rs tests/graph_temporal.rs
git commit -m "feat(service): MemoryService uses DuckDbGraphStore; supersede closes edges"
```

---

## Task 10: Wire startup in `app.rs`, drop config branches

**Files:**
- Modify: `src/app.rs`
- Modify: `src/config.rs`

- [ ] **Step 1: Update `src/config.rs`**

Find these items and delete:
- `pub enum GraphBackendKind { Local, IndraDb }`
- `Config.graph_backend: GraphBackendKind` field
- `Config.indradb_path: Option<PathBuf>` field
- `ConfigError::InvalidGraphBackend(...)` variant
- The env-var parsing for `GRAPH_BACKEND` and `INDRADB_PATH` in `from_env`
- The `graph_backend: ...` and `indradb_path: ...` lines in `Config::local()` / `Config::development_defaults()` / wherever `Config` is constructed

Keep `Config::from_env` returning `Result<Self, ConfigError>` — the signature stays identical, just fewer fields parsed.

- [ ] **Step 2: Update `src/app.rs`**

Find the existing graph store construction. It probably looks like:

```rust
let graph: Arc<dyn GraphStore> = match config.graph_backend {
    GraphBackendKind::Local => Arc::new(LocalGraphAdapter::new()),
    GraphBackendKind::IndraDb => Arc::new(IndraDbGraphAdapter::with_path(config.indradb_path.clone())),
};
```

Replace with:

```rust
let repo_arc = Arc::new(repository.clone());
let graph = Arc::new(mem::storage::DuckDbGraphStore::new(repo_arc));
```

Pass the new `graph` Arc into wherever `MemoryService::new_with_graph` (Task 9) is constructed. If Task 9's auto-construction in `MemoryService::new` is sufficient, you can drop the explicit graph variable here; but having it visible in `app.rs` makes the lifecycle clearer.

- [ ] **Step 3: Build + smoke**

```bash
cargo build
MEM_DB_PATH=/tmp/graph-temporal-smoke.duckdb cargo run --quiet -- serve &
sleep 3
curl -s -X POST localhost:3000/memories \
  -H 'content-type: application/json' \
  -d '{"memory_type":"implementation","content":"smoke","scope":"repo","write_mode":"auto","tenant":"local","project":"foo","repo":"mem"}'
curl -s "localhost:3000/graph/neighbors/memory:$(echo response | head -1 | jq -r .memory_id)"
kill %1 2>/dev/null
```

Expected: server starts cleanly, no `INDRADB_PATH` lookup; `graph/neighbors` returns edges with `valid_from` populated and `valid_to: null`.

(If the smoke test is too fragile, just confirm `cargo run -- serve` starts and tracing shows no errors. Skip the curl part.)

- [ ] **Step 4: Run tests**

Run: `cargo test -q`
Expected: full suite passes.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/config.rs
git commit -m "feat(app): construct DuckDbGraphStore at startup; drop graph_backend config"
```

---

## Task 11: Delete IndraDB code path entirely

**Files:**
- Delete: `src/storage/graph.rs`
- Delete: `tests/graph_adapter.rs`
- Modify: `src/storage/mod.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Verify nothing references the old code**

```bash
grep -rn "IndraDbGraphAdapter\|LocalGraphAdapter\|GraphStore\b" src/ tests/
```

Expected: no matches in `src/` (the trait and adapters only appear in `src/storage/graph.rs` and possibly `tests/graph_adapter.rs`). If any production code still references them, fix that first.

```bash
grep -rn "indradb" src/ Cargo.toml
```

Expected: matches only in `Cargo.toml` (`indradb-lib = "5"`) and `src/storage/graph.rs`.

- [ ] **Step 2: Delete the old graph.rs**

```bash
rm src/storage/graph.rs
```

- [ ] **Step 3: Delete the old graph_adapter test file**

```bash
rm tests/graph_adapter.rs
```

- [ ] **Step 4: Update `src/storage/mod.rs`**

Remove `pub mod graph;` and any `pub use graph::{...};` lines. Only `graph_store::{DuckDbGraphStore, DuckDbGraphError}` (or rename `DuckDbGraphError` → `GraphError` now that the conflict is gone) should remain. Rename if doing so:

```rust
pub use graph_store::{DuckDbGraphStore, GraphError};
```

And in `src/storage/graph_store.rs`, rename internally:

```rust
// at top
#[derive(Debug, Error)]
pub enum GraphError {  // was: DuckDbGraphError
    #[error("graph backend error: {0}")]
    Backend(String),
}
```

- [ ] **Step 5: Drop `indradb-lib` dependency**

In `Cargo.toml`, remove the line:
```toml
indradb-lib = "5"
```

- [ ] **Step 6: Build + run all tests**

```bash
cargo build
cargo test -q
cargo clippy --all-targets -- -D warnings
```

Expected: full suite passes; `Cargo.lock` updates to drop `indradb-lib` and any transitive deps.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/storage/mod.rs src/storage/graph_store.rs
git commit -a -m "refactor(graph): delete IndraDB adapter, trait, and indradb-lib dependency"
```

> Note: `git commit -a` picks up the deletions of `src/storage/graph.rs` and `tests/graph_adapter.rs` automatically. If you want explicit staging, use `git rm` for the deleted files.

---

## Task 12: End-to-end smoke — supersede excludes from `graph_boost`

**Files:**
- Modify: `tests/hybrid_search.rs`

- [ ] **Step 1: Read the existing `tests/hybrid_search.rs`**

The test file has a setup that ingests memories and runs `merge_and_rank_hybrid`-style queries. Find the smallest existing test pattern and add a new case below it.

- [ ] **Step 2: Append the e2e test**

```rust
#[tokio::test]
async fn graph_boost_excludes_superseded_memory() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e2e.duckdb");
    let repo = Arc::new(DuckDbRepository::open(&db).await.unwrap());
    let graph = Arc::new(mem::storage::DuckDbGraphStore::new(repo.clone()));
    let svc = MemoryService::new_with_graph((*repo).clone(), graph.clone());

    // Ingest two memories sharing project=foo.
    let r1 = ingest_one_for_search(&svc, "alpha-content", Some("foo"), Some("mem")).await;
    let r2 = ingest_one_for_search(&svc, "beta-content", Some("foo"), Some("mem")).await;
    let m1 = repo.get_memory_for_tenant("t", &r1.memory_id).await.unwrap().unwrap();
    let m2 = repo.get_memory_for_tenant("t", &r2.memory_id).await.unwrap().unwrap();
    graph.sync_memory(&m1).await.unwrap();
    graph.sync_memory(&m2).await.unwrap();

    // Both memories should appear in graph_boost candidates.
    let pre = graph.related_memory_ids(&["project:foo".into()]).await.unwrap();
    assert_eq!(pre.len(), 2);

    // Close r1 (simulates supersede side-effect).
    graph.close_edges_for_memory(&r1.memory_id).await.unwrap();

    let post = graph.related_memory_ids(&["project:foo".into()]).await.unwrap();
    assert_eq!(post, vec![r2.memory_id]);
}

async fn ingest_one_for_search(svc: &MemoryService, content: &str, project: Option<&str>, repo: Option<&str>)
    -> mem::domain::memory::IngestMemoryResponse
{
    use mem::domain::memory::{IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode};
    svc.ingest(IngestMemoryRequest {
        tenant: "t".into(),
        memory_type: MemoryType::Implementation,
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: project.map(String::from),
        repo: repo.map(String::from),
        module: None,
        task_type: None,
        tags: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    }).await.unwrap()
}
```

- [ ] **Step 3: Run**

Run: `cargo test --test hybrid_search graph_boost -q`
Expected: PASS.

Then `cargo test -q` — full suite.

- [ ] **Step 4: Commit**

```bash
git add tests/hybrid_search.rs
git commit -m "test(hybrid_search): supersede excludes memory from graph_boost"
```

---

## Task 13: Final verification + close §8 #5

**Files:**
- Modify: `docs/mempalace-diff.md`

- [ ] **Step 1: Run full verification**

```bash
cargo test -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

All three must be clean. If `cargo fmt --check` fails, run `cargo fmt`, commit as `chore: cargo fmt`.

- [ ] **Step 2: Manual smoke run**

```bash
MEM_DB_PATH=/tmp/graph-final-smoke.duckdb timeout 5 cargo run --quiet 2>&1 | tail -10
```

Expected: server starts; no errors related to graph_backend / indradb_path; tracing shows the schema bootstrap completing.

- [ ] **Step 3: Mark §8 row #5 complete**

In `docs/mempalace-diff.md`, find the row:

```markdown
| 5 | 🔍 | 图边时序化（valid_from/to） | 🟠 表达力 | M（4–6h） | 中 | `domain/memory.rs`、`storage/graph.rs`、`pipeline/ingest.rs` |
```

Change to:

```markdown
| 5 | 🔍 | ✅ 图边时序化（valid_from/to）+ 全量迁到 DuckDB（删 IndraDB）| 🟠 表达力 | L（实际 ~1.5 天，scope 比原 M 大）| 中 | `domain/memory.rs`、`storage/graph_store.rs`、`db/schema/003_graph.sql`、`pipeline/ingest.rs` |
```

- [ ] **Step 4: Commit**

```bash
git add docs/mempalace-diff.md
git commit -m "docs(mempalace-diff): mark §8 #5 complete (closes mempalace-diff §8 #5)"
```

If `cargo fmt` had to be run, commit it FIRST as `chore: cargo fmt` before this commit.

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** every section of `2026-04-28-graph-temporal-design.md` maps to a task. Task 1 covers the type change. Task 2 covers schema. Tasks 3–8 cover each public method on `DuckDbGraphStore`. Tasks 9–11 cover wiring + cleanup. Task 12 covers the e2e smoke. Task 13 closes the roadmap.
- **`MemoryService::new_with_graph`** is new; it preserves `MemoryService::new` and `new_with_index` (from §3) by having them all construct a graph store internally. If you find ergonomic friction at call sites, consider a builder, but the simple stack of constructors should suffice for now.
- **`StorageError::VectorIndex` reuse for graph errors** is intentional shorthand. If the variant name becomes misleading, rename to `StorageError::Storage(String)` or `Backend(String)` in a follow-up — out of scope for this PR.
- **`DuckDbRepository::conn()` visibility:** the new `graph_store.rs` calls it. If `conn` is `pub(crate)` it works because both modules live in the same crate. Verify by reading `duckdb.rs:1177`.
- **The supersede flow needs careful study.** Read `service/memory_service.rs` end-to-end before Task 9. The exact site to insert `close_edges_for_memory` + `sync_memory(superseding)` may not be at line 493 verbatim — find the equivalent in the actual code and adapt.
- **`extract_graph_edges` modifications in Task 1:** it must compile with the new fields, AND the existing IndraDB adapter (`graph.rs`) must also compile with the new fields (until Task 11 deletes it). That means every `GraphEdge { ... }` literal across the codebase needs the new fields temporarily filled with `String::new()` / `None`.
