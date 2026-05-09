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
//! **Skeleton state:** only `list_memories_for_tenant` is implemented.
//! Subsequent commits add `search_candidates`, `get_memory_for_tenant`,
//! `recent_active_memories`, `bm25_candidates` (via `lance_fts`),
//! `semantic_search_memories` (via `lance_vector_search`), the
//! transcript reads, the graph reads, and the entity-registry reads —
//! one cluster per commit so each addition is reviewable.

use std::path::Path;
use std::sync::{Arc, Mutex};

use duckdb::types::Value;
use duckdb::{params, Connection};
use serde::de::DeserializeOwned;

use super::StorageError;
use crate::domain::memory::MemoryRecord;

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
        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryRecord>, StorageError> {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(MEMORY_SELECT_PREFIX_WHERE_TENANT)?;
            let rows = stmt.query_map(params![tenant], row_to_memory_record)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| StorageError::InvalidInput(format!("spawn_blocking join: {e}")))?
    }
}

/// SELECT clause shared by every memory-row read method on this struct.
/// Column ordering matches `row_to_memory_record` below — keep in sync.
const MEMORY_SELECT_PREFIX_WHERE_TENANT: &str = "
    SELECT memory_id, tenant, memory_type, status, scope, visibility, version,
           summary, content, evidence, code_refs, project, repo, module, task_type,
           tags, topics, confidence, decay_score, content_hash, idempotency_key,
           session_id, supersedes_memory_id, source_agent, created_at, updated_at,
           last_validated_at
    FROM ns.main.memories WHERE tenant = ?1
";

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
}
