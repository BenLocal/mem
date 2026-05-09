//! DuckDB SQL query layer over Lance datasets.
//!
//! Reads-only client. Pairs with [`crate::storage::lance_store::LanceStore`]
//! (the writer) — both point at the same on-disk lance directory; rows
//! written through `LanceStore`'s Rust API are immediately visible to
//! SQL here without any DETACH/re-ATTACH ceremony (verified by
//! `examples/lance_duckdb_poc.rs`).
//!
//! Architecture: in-process DuckDB connection. `INSTALL lance; LOAD
//! lance;` resolves the core extension; `ATTACH '<path>' AS ns (TYPE
//! LANCE)` exposes every dataset under the directory as
//! `ns.main.<table>`. From there, all reads are plain SQL — including
//! GROUP BY / window functions / subqueries that the LanceDB native
//! query API doesn't expose. ANN and FTS go through the extension's
//! `lance_vector_search()` / `lance_fts()` table functions.
//!
//! Concurrency: DuckDB is single-writer. We hold the connection in an
//! `Arc<Mutex<Connection>>` so concurrent reads serialize through one
//! mutex. Methods are `async fn` for ergonomic call sites — bodies use
//! `tokio::task::spawn_blocking` to run the blocking SQL on the thread
//! pool, so the runtime worker thread isn't pinned. This mirrors the
//! pattern the legacy `DuckDbRepository` used (and is the only
//! reasonable way to bridge sync `duckdb-rs` 1.x into an async service
//! layer).
//!
//! **Coverage so far** (memories table):
//!   - `list_memories_for_tenant`
//!   - `get_memory_for_tenant`
//!   - `get_pending`
//!   - `find_by_idempotency_or_hash`
//!   - `list_pending_review`
//!   - `recent_active_memories`
//!
//! Subsequent commits add `search_candidates`, `fetch_memories_by_ids`,
//! `bm25_candidates` (via `lance_fts`), `semantic_search_memories`
//! (via `lance_vector_search`), the transcript reads, the graph reads,
//! and the entity-registry reads — one cluster per commit so each
//! addition is reviewable.

use std::path::Path;
use std::sync::{Arc, Mutex};

use duckdb::types::Value;
use duckdb::{params, Connection, OptionalExt};
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::StorageError;
use crate::domain::memory::{MemoryRecord, MemoryStatus};

/// Read-only DuckDB SQL client backed by lance datasets ATTACHed at
/// open time. See module-level docs for the architecture.
#[derive(Clone)]
pub struct DuckDbQuery {
    conn: Arc<Mutex<Connection>>,
}

impl DuckDbQuery {
    /// Open an in-memory DuckDB, install + load the `lance` core
    /// extension, and ATTACH `lance_path` as namespace `ns`. The
    /// directory must already exist with at least one Lance dataset
    /// inside (typically created by `LanceStore::open` before this
    /// method is called).
    ///
    /// Apostrophes in the path are SQL-escaped (doubled) for the ATTACH
    /// statement; the path is otherwise embedded verbatim.
    ///
    /// **Network:** first run downloads the lance extension binary
    /// (~few MB) from `extensions.duckdb.org` into
    /// `~/.duckdb/extensions/<duckdb-version>/<platform>/`. Subsequent
    /// runs are offline.
    pub async fn open(lance_path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = lance_path.as_ref().to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, StorageError> {
            let path_str = path
                .to_str()
                .ok_or(StorageError::InvalidData("lance path must be UTF-8"))?;
            let escaped = path_str.replace('\'', "''");
            let c = Connection::open_in_memory()?;
            c.execute_batch("INSTALL lance; LOAD lance;")?;
            c.execute_batch(&format!("ATTACH '{escaped}' AS ns (TYPE LANCE);"))?;
            Ok(c)
        })
        .await
        .map_err(|e| StorageError::InvalidInput(format!("spawn_blocking join: {e}")))??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// All memories for `tenant`. Mirrors the DuckDB-as-storage
    /// implementation's signature 1:1 so the eventual service-layer
    /// switch is a method-call swap, not a type swap.
    pub async fn list_memories_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories WHERE tenant = ?1",
                MEMORY_COLS = MEMORY_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tenant], row_to_memory_record)?;
            collect_memories(rows)
        })
        .await
    }

    /// Single memory by `(tenant, memory_id)`. Returns `Ok(None)` when
    /// no row matches (the canonical "not found" path — distinct from
    /// errors).
    pub async fn get_memory_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let memory_id = memory_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND memory_id = ?2",
                MEMORY_COLS = MEMORY_COLS,
            );
            conn.query_row(&sql, params![tenant, memory_id], row_to_memory_record)
                .optional()
                .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// Single pending-confirmation memory by `(tenant, memory_id)`.
    /// Used by the review-queue UI's edit/inspect flow — surfaces
    /// `Ok(None)` if the row is gone or has already been
    /// accepted/rejected (status moved off `pending_confirmation`).
    pub async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let memory_id = memory_id.to_string();
        let status = enum_to_text(&MemoryStatus::PendingConfirmation)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND memory_id = ?2 AND status = ?3",
                MEMORY_COLS = MEMORY_COLS,
            );
            conn.query_row(
                &sql,
                params![tenant, memory_id, status],
                row_to_memory_record,
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// Idempotency check used by `MemoryService::ingest`. Matches on
    /// either an `idempotency_key` (when caller supplied one) or the
    /// `content_hash` (always; functions as the natural-key duplicate
    /// check). Idempotency-key matches rank first (priority 0) so a
    /// caller-asserted identity wins over content-hash collisions; ties
    /// break by `updated_at DESC`. Returns the top row or `None`.
    pub async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let idempotency_key = idempotency_key.clone();
        let content_hash = content_hash.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories
                 WHERE tenant = ?1
                   AND ((?2 IS NOT NULL AND idempotency_key = ?2) OR content_hash = ?3)
                 ORDER BY
                    CASE WHEN ?2 IS NOT NULL AND idempotency_key = ?2 THEN 0 ELSE 1 END,
                    updated_at DESC
                 LIMIT 1",
                MEMORY_COLS = MEMORY_COLS,
            );
            conn.query_row(
                &sql,
                params![tenant, idempotency_key.as_deref(), content_hash],
                row_to_memory_record,
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// All memories awaiting review (status =
    /// `pending_confirmation`) under `tenant`, oldest-newest first
    /// (well, ordered `created_at DESC` per legacy convention — newest
    /// arrivals at the top of the queue).
    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let status = enum_to_text(&MemoryStatus::PendingConfirmation)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND status = ?2 \
                 ORDER BY created_at DESC",
                MEMORY_COLS = MEMORY_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tenant, status], row_to_memory_record)?;
            collect_memories(rows)
        })
        .await
    }

    /// Most-recent non-rejected, non-archived memories under `tenant`
    /// — the empty-query fallback for `mem wake-up`. Ordered
    /// `(updated_at DESC, version DESC, memory_id ASC)` to keep ties
    /// deterministic when a batch of rows shares an `updated_at`
    /// timestamp.
    ///
    /// `limit` is clamped to `[1, 1024]` (mirrors the legacy bound).
    pub async fn recent_active_memories(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        let rejected = enum_to_text(&MemoryStatus::Rejected)?;
        let archived = enum_to_text(&MemoryStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND status NOT IN (?2, ?3) \
                 ORDER BY updated_at DESC, version DESC, memory_id ASC \
                 LIMIT ?4",
                MEMORY_COLS = MEMORY_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![tenant, rejected, archived, lim],
                row_to_memory_record,
            )?;
            collect_memories(rows)
        })
        .await
    }
}

/// Run a synchronous DuckDB query body on a blocking-pool thread and
/// surface the result back to the async caller. Standardizes the
/// `spawn_blocking` ↔ `StorageError` conversion so individual methods
/// stay clean.
async fn spawn_blocking_storage<T, F>(f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| StorageError::InvalidInput(format!("spawn_blocking join: {e}")))?
}

/// Collect rows from a `query_map` iterator into a `Vec<MemoryRecord>`,
/// converting the per-row `duckdb::Error` to `StorageError`.
fn collect_memories<I>(rows: I) -> Result<Vec<MemoryRecord>, StorageError>
where
    I: Iterator<Item = duckdb::Result<MemoryRecord>>,
{
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(StorageError::DuckDb)?);
    }
    Ok(out)
}

/// Serialize an enum to its snake_case Utf8 storage form, matching what
/// LanceStore writes. Inverse of `parse_enum`. Used for SQL parameter
/// binding when filtering by enum-string columns (e.g.
/// `status = 'pending_confirmation'`).
fn enum_to_text<T: Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or(StorageError::InvalidData("enum serializes as non-string"))
}

/// 27-column projection shared by every memory-row read method.
/// Order must match `row_to_memory_record` below — keep in sync.
const MEMORY_COLS: &str = "memory_id, tenant, memory_type, status, scope, visibility, version, \
    summary, content, evidence, code_refs, project, repo, module, task_type, \
    tags, topics, confidence, decay_score, content_hash, idempotency_key, \
    session_id, supersedes_memory_id, source_agent, created_at, updated_at, \
    last_validated_at";

/// Parse one row of the 27-column SELECT above into a [`MemoryRecord`].
///
/// Type expectations (Lance Arrow → DuckDB SQL via the lance extension):
///   - `Utf8` → `VARCHAR` → `String` / `Option<String>`
///   - `List<Utf8>` → `VARCHAR[]` → `Vec<String>`
///   - `UInt64` → `UBIGINT` → `u64`
///   - `Float32` → `FLOAT` (a.k.a. `REAL`) → `f32`
///
/// Enum fields (`memory_type`, `status`, `scope`, `visibility`) live as
/// snake_case Utf8 strings on the Lance side per LanceStore's schema;
/// here we round-trip them through `serde_json::Value::String` so
/// `#[serde(rename_all = "snake_case")]` on the enum lookups them
/// without needing a hand-written from-str table.
fn row_to_memory_record(row: &duckdb::Row<'_>) -> duckdb::Result<MemoryRecord> {
    Ok(MemoryRecord {
        memory_id: row.get(0)?,
        tenant: row.get(1)?,
        memory_type: parse_enum(&row.get::<_, String>(2)?)?,
        status: parse_enum(&row.get::<_, String>(3)?)?,
        scope: parse_enum(&row.get::<_, String>(4)?)?,
        visibility: parse_enum(&row.get::<_, String>(5)?)?,
        version: row.get::<_, u64>(6)?,
        summary: row.get(7)?,
        content: row.get(8)?,
        evidence: get_string_list(row, 9)?,
        code_refs: get_string_list(row, 10)?,
        project: row.get(11)?,
        repo: row.get(12)?,
        module: row.get(13)?,
        task_type: row.get(14)?,
        tags: get_string_list(row, 15)?,
        topics: get_string_list(row, 16)?,
        confidence: row.get::<_, f32>(17)?,
        decay_score: row.get::<_, f32>(18)?,
        content_hash: row.get(19)?,
        idempotency_key: row.get(20)?,
        session_id: row.get(21)?,
        supersedes_memory_id: row.get(22)?,
        source_agent: row.get(23)?,
        created_at: row.get(24)?,
        updated_at: row.get(25)?,
        last_validated_at: row.get(26)?,
    })
}

fn parse_enum<T: DeserializeOwned>(value: &str) -> duckdb::Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|e| {
        duckdb::Error::FromSqlConversionFailure(0, duckdb::types::Type::Text, Box::new(e))
    })
}

/// Extract a `LIST(VARCHAR)` column as `Vec<String>`. duckdb-rs 1.x
/// doesn't ship a `FromSql` impl for `Vec<String>`, so we go through the
/// `Value` enum. NULL list → empty Vec (mem semantics: missing list ==
/// no items).
fn get_string_list(row: &duckdb::Row<'_>, idx: usize) -> duckdb::Result<Vec<String>> {
    let v: Value = row.get(idx)?;
    let items = match v {
        Value::List(items) | Value::Array(items) => items,
        Value::Null => return Ok(Vec::new()),
        other => {
            return Err(duckdb::Error::FromSqlConversionFailure(
                idx,
                duckdb::types::Type::Any,
                format!("expected LIST(VARCHAR) at column {idx}, got {other:?}").into(),
            ));
        }
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::Text(s) => Ok(s),
            Value::Null => Ok(String::new()),
            other => Err(duckdb::Error::FromSqlConversionFailure(
                idx,
                duckdb::types::Type::Any,
                format!("expected VARCHAR list element, got {other:?}").into(),
            )),
        })
        .collect()
}

#[cfg(all(test, feature = "lancedb"))]
mod tests {
    use super::*;
    use crate::domain::memory::{MemoryStatus, MemoryType, Scope, Visibility};
    use crate::storage::lance_store::LanceStore;
    use crate::storage::MemoryRepository;
    use tempfile::tempdir;

    fn fixture(memory_id: &str, tenant: &str) -> MemoryRecord {
        MemoryRecord {
            memory_id: memory_id.into(),
            tenant: tenant.into(),
            memory_type: MemoryType::Implementation,
            status: MemoryStatus::Active,
            scope: Scope::Project,
            visibility: Visibility::Shared,
            version: 1,
            summary: "round-trip".into(),
            content: "use bun for fast installs".into(),
            evidence: vec!["src/main.rs:42".into(), "Cargo.toml:11".into()],
            code_refs: vec!["foo::bar()".into()],
            project: Some("mem".into()),
            repo: Some("mem".into()),
            module: None,
            task_type: None,
            tags: vec!["tooling".into()],
            topics: vec!["bun".into()],
            confidence: 0.7,
            decay_score: 0.0,
            content_hash: "h".repeat(64),
            idempotency_key: Some("idemp-1".into()),
            session_id: None,
            supersedes_memory_id: None,
            source_agent: "test".into(),
            created_at: "00000001778000000000".into(),
            updated_at: "00000001778000000000".into(),
            last_validated_at: None,
        }
    }

    /// Cross-stack round-trip: insert via LanceStore (Rust API write),
    /// list via DuckDbQuery (DuckDB SQL read against the same on-disk
    /// lance dataset). Validates:
    ///   - `INSTALL lance; LOAD lance; ATTACH ...` against a freshly
    ///     created lance directory.
    ///   - All 27 column types parse correctly through the SQL boundary
    ///     (incl. `List<Utf8>` → `VARCHAR[]` → `Vec<String>`,
    ///     `UInt64` → `UBIGINT` → `u64`, `Float32` → `FLOAT` → `f32`).
    ///   - Tenant filter scopes correctly.
    #[tokio::test(flavor = "multi_thread")]
    async fn lance_write_then_duckdb_read_memories() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");

        // 1. Create + populate Lance dataset via the writer.
        let lance = LanceStore::open(&path).await.expect("LanceStore::open");
        lance
            .insert_memory(fixture("m1", "tenant-a"))
            .await
            .expect("insert m1");
        lance
            .insert_memory(fixture("m2", "tenant-a"))
            .await
            .expect("insert m2");
        lance
            .insert_memory(fixture("m3", "tenant-b"))
            .await
            .expect("insert m3");

        // 2. Open DuckDB query layer on the same path.
        let q = DuckDbQuery::open(&path).await.expect("DuckDbQuery::open");

        // 3. Read back through SQL. tenant-a → 2 rows; tenant-b → 1 row.
        let mut a = q
            .list_memories_for_tenant("tenant-a")
            .await
            .expect("list tenant-a");
        a.sort_by(|x, y| x.memory_id.cmp(&y.memory_id));
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].memory_id, "m1");
        assert_eq!(a[1].memory_id, "m2");
        // Spot-check rich types preserved through the SQL boundary.
        assert_eq!(a[0].evidence, vec!["src/main.rs:42", "Cargo.toml:11"]);
        assert_eq!(a[0].code_refs, vec!["foo::bar()"]);
        assert_eq!(a[0].tags, vec!["tooling"]);
        assert_eq!(a[0].topics, vec!["bun"]);
        assert_eq!(a[0].version, 1u64);
        assert!((a[0].confidence - 0.7).abs() < 1e-6);
        assert_eq!(a[0].project.as_deref(), Some("mem"));
        assert!(a[0].module.is_none());
        assert_eq!(a[0].status, MemoryStatus::Active);
        assert_eq!(a[0].memory_type, MemoryType::Implementation);

        let b = q
            .list_memories_for_tenant("tenant-b")
            .await
            .expect("list tenant-b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].memory_id, "m3");

        // Tenant that has no rows returns empty (not an error).
        let none = q
            .list_memories_for_tenant("does-not-exist")
            .await
            .expect("list missing tenant");
        assert!(none.is_empty());
    }

    /// Exercises the 4 single-row / filtered-list methods that build
    /// on the same SELECT prefix as `list_memories_for_tenant`:
    /// `get_memory_for_tenant`, `get_pending`,
    /// `find_by_idempotency_or_hash`, `list_pending_review`,
    /// `recent_active_memories`.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_memory_filters() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // Seed: 1 active, 1 pending, 1 archived (excluded from
        // recent_active_memories), 1 rejected (also excluded), 1 in
        // tenant-b (cross-tenant exclusion).
        let mut active = fixture("m_active", "tenant-a");
        active.idempotency_key = Some("idemp-active".into());
        active.content_hash = "hash-active".into();
        active.updated_at = "00000001778000000020".into();
        let mut pending = fixture("m_pending", "tenant-a");
        pending.status = MemoryStatus::PendingConfirmation;
        pending.idempotency_key = Some("idemp-pending".into());
        pending.content_hash = "hash-pending".into();
        pending.updated_at = "00000001778000000010".into();
        let mut archived = fixture("m_archived", "tenant-a");
        archived.status = MemoryStatus::Archived;
        archived.updated_at = "00000001778000000005".into();
        let mut rejected = fixture("m_rejected", "tenant-a");
        rejected.status = MemoryStatus::Rejected;
        rejected.updated_at = "00000001778000000006".into();
        let other_tenant = fixture("m_other", "tenant-b");

        for m in [&active, &pending, &archived, &rejected, &other_tenant] {
            lance.insert_memory(m.clone()).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // get_memory_for_tenant — hit + miss + cross-tenant.
        let hit = q
            .get_memory_for_tenant("tenant-a", "m_active")
            .await
            .unwrap()
            .expect("active memory should exist");
        assert_eq!(hit.memory_id, "m_active");
        assert_eq!(hit.status, MemoryStatus::Active);
        let miss = q
            .get_memory_for_tenant("tenant-a", "does-not-exist")
            .await
            .unwrap();
        assert!(miss.is_none());
        let cross = q
            .get_memory_for_tenant("tenant-b", "m_active")
            .await
            .unwrap();
        assert!(cross.is_none(), "tenant filter must scope cross-tenant");

        // get_pending — only pending status surfaces.
        let pend = q
            .get_pending("tenant-a", "m_pending")
            .await
            .unwrap()
            .expect("pending row");
        assert_eq!(pend.memory_id, "m_pending");
        let pend_active = q.get_pending("tenant-a", "m_active").await.unwrap();
        assert!(
            pend_active.is_none(),
            "active row must not surface through get_pending"
        );

        // find_by_idempotency_or_hash:
        //   (a) idempotency-key match wins over content-hash match,
        //   (b) None idempotency_key falls through to hash,
        //   (c) miss → None.
        let by_idemp = q
            .find_by_idempotency_or_hash(
                "tenant-a",
                &Some("idemp-active".into()),
                "hash-pending", // would also match m_pending by hash
            )
            .await
            .unwrap()
            .expect("idempotency-key match should win");
        assert_eq!(by_idemp.memory_id, "m_active");
        let by_hash_only = q
            .find_by_idempotency_or_hash("tenant-a", &None, "hash-pending")
            .await
            .unwrap()
            .expect("hash match");
        assert_eq!(by_hash_only.memory_id, "m_pending");
        let by_miss = q
            .find_by_idempotency_or_hash("tenant-a", &None, "no-such-hash")
            .await
            .unwrap();
        assert!(by_miss.is_none());

        // list_pending_review — only pending_confirmation.
        let pending_list = q.list_pending_review("tenant-a").await.unwrap();
        assert_eq!(pending_list.len(), 1);
        assert_eq!(pending_list[0].memory_id, "m_pending");
        let other = q.list_pending_review("tenant-b").await.unwrap();
        assert!(other.is_empty(), "no pending in tenant-b");

        // recent_active_memories — pending + active stay; archived +
        // rejected drop. Cross-tenant excluded.
        let recent = q.recent_active_memories("tenant-a", 50).await.unwrap();
        let recent_ids: Vec<&str> = recent.iter().map(|m| m.memory_id.as_str()).collect();
        assert_eq!(
            recent_ids,
            vec!["m_active", "m_pending"],
            "ordered by updated_at DESC; archived/rejected excluded"
        );
        let recent_b = q.recent_active_memories("tenant-b", 50).await.unwrap();
        assert_eq!(recent_b.len(), 1);
        assert_eq!(recent_b[0].memory_id, "m_other");

        // limit clamps to >=1 even when caller passes 0 (mirrors the
        // legacy DuckDB-as-storage clamp).
        let recent_clamped = q.recent_active_memories("tenant-a", 0).await.unwrap();
        assert_eq!(recent_clamped.len(), 1);
    }
}
