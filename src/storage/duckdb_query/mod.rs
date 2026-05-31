//! DuckDB SQL read half of the storage stack.
//!
//! Pairs with [`super::lance_store::LanceStore`] (the writer) — both
//! point at the same on-disk lance directory. Rows written through
//! `LanceStore`'s Rust API are visible here as soon as
//! [`Self::refresh`] swaps a fresh DuckDB connection (handled
//! automatically by [`super::Store::commit_lance_write`]).
//!
//! Architecture: in-process DuckDB connection. `INSTALL lance; LOAD
//! lance;` resolves the core extension; `ATTACH '<path>' AS ns (TYPE
//! LANCE)` exposes every dataset under the directory as
//! `ns.main.<table>`. From there, all reads are plain SQL — including
//! GROUP BY / window functions / subqueries / recursive CTEs that the
//! LanceDB native query API doesn't expose. ANN goes through the
//! extension's `lance_vector_search()` table function; BM25 goes
//! through `lance_fts()` (LanceDB 0.27 native FTS, indexed at
//! table-open time in `LanceStore::open` — see `ensure_fts_index`).
//!
//! Concurrency: DuckDB is single-writer. We hold the connection in an
//! `Arc<Mutex<Connection>>` so concurrent reads serialize through one
//! mutex. Methods are `async fn` for ergonomic call sites — bodies use
//! `tokio::task::spawn_blocking` to run the blocking SQL on the thread
//! pool, so the runtime worker thread isn't pinned.
//!
//! Like `LanceStore`, this module is `pub(crate)` since Phase 5+ —
//! external callers reach reads through the `Backend` umbrella trait
//! or one of the 9 sub-traits in `src/storage/*.rs`. The split
//! between WHICH read sits here vs on `LanceStore` is canonically
//! tracked by `Store`'s delegate methods (`Store::method_x` → either
//! `self.query.method_x` here or, for a handful of write-hot-path
//! reads, `self.lance.method_x`). Adding a new read method: prefer
//! this module unless the caller absolutely cannot afford a DuckDB
//! round-trip.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use duckdb::types::Value;
use duckdb::Connection;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::{GraphError, StorageError};
use crate::domain::capability_capsule::GraphEdge;
use crate::domain::{Entity, EntityKind};

mod capability_capsules;
mod decay;
mod embedding_jobs;
mod entities;
mod graph;
mod transcripts;

/// Read-only DuckDB SQL client backed by lance datasets ATTACHed at
/// open time. See module-level docs for the architecture.
#[derive(Clone)]
pub struct DuckDbQuery {
    pub(super) conn: Arc<Mutex<Connection>>,
    /// Original lance directory path. Stored so [`Self::refresh`]
    /// can re-ATTACH after lance writes from outside the DuckDB
    /// connection (which the extension's snapshot caching otherwise
    /// hides).
    pub(super) lance_path: PathBuf,
    /// Dirty flag — `true` when a Lance write has happened since the
    /// last `refresh()` and a subsequent read needs to re-attach to
    /// see the latest version (v3 D2 — defer refresh until a reader
    /// actually needs it).
    ///
    /// Set by [`Store::commit_lance_write`] after every mutating
    /// call; cleared by [`Self::ensure_fresh`] right before refresh.
    /// Multiple writes between reads coalesce into a single refresh.
    /// This dropped idle baseline CPU from ~510% to ~56% in measured
    /// workloads pre-D1; D2 closes the per-tick refresh churn so we
    /// can return poll cadence to 1 Hz without re-incurring the
    /// 100ms-per-write hit.
    pub(super) dirty: Arc<std::sync::atomic::AtomicBool>,
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
    ///
    /// **Snapshot caching:** the lance extension caches the dataset
    /// version at first query post-ATTACH. Subsequent writes via the
    /// LanceDB Rust API (which is how `LanceStore` mutates) are
    /// invisible to this connection until [`Self::refresh`] is
    /// called. The `Store` wrapper does that refresh after every
    /// mutating call; direct `DuckDbQuery` users (only the
    /// per-module unit tests today) need to do it themselves.
    pub async fn open(lance_path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = lance_path.as_ref().to_path_buf();
        let path_for_thread = path.clone();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, StorageError> {
            let path_str = path_for_thread
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
            lance_path: path,
            dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Mark the connection dirty — call from the write path after a
    /// lance mutation. Cheap (one atomic store); the actual refresh
    /// is deferred until a reader calls [`Self::ensure_fresh`].
    pub fn mark_dirty(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Refresh the connection iff the dirty flag is set; clear it
    /// either way. Called by every read entry point in
    /// `duckdb_query/*` so reads always see the latest version after
    /// any pending writes.
    ///
    /// Concurrency: the swap-then-refresh ordering means a write that
    /// arrives between the swap and the refresh sets dirty=true
    /// again, which the NEXT reader will see and act on. No write is
    /// ever lost; in the worst case, two consecutive reads both pay
    /// the refresh cost (one from the original swap, one from the
    /// racing write that arrived during the first refresh).
    pub async fn ensure_fresh(&self) -> Result<(), StorageError> {
        // swap clears + tells us the prior value atomically.
        if self.dirty.swap(false, std::sync::atomic::Ordering::AcqRel) {
            self.refresh().await?;
        }
        Ok(())
    }

    /// Convenience: `ensure_fresh().await?` + `conn.clone()` in one
    /// step. Read methods should replace `let conn = self.conn.clone();`
    /// with `let conn = self.fresh_conn().await?;` so the returned
    /// connection has already picked up any pending lance writes.
    pub(super) async fn fresh_conn(&self) -> Result<Arc<Mutex<Connection>>, StorageError> {
        self.ensure_fresh().await?;
        Ok(self.conn.clone())
    }

    /// Same as `fresh_conn` but for graph read methods that surface
    /// `GraphError`. The refresh error is wrapped in
    /// `GraphError::Backend`.
    pub(super) async fn fresh_conn_for_graph(
        &self,
    ) -> Result<Arc<Mutex<Connection>>, crate::storage::types::GraphError> {
        self.ensure_fresh()
            .await
            .map_err(|e| crate::storage::types::GraphError::Backend(e.to_string()))?;
        Ok(self.conn.clone())
    }

    /// Replace the in-process DuckDB connection with a fresh one
    /// (re-INSTALL/LOAD the lance extension and re-ATTACH the
    /// dataset). The lance extension caches the dataset version
    /// inside a connection's extension state; DETACH + re-ATTACH on
    /// the same connection isn't enough to clear that cache —
    /// empirically (see `store_open_write_read_round_trip` test
    /// probes), only a brand-new Connection picks up writes done
    /// via the LanceDB Rust API since the previous attach.
    ///
    /// Cost: maybe 100ms per call (connection setup + extension
    /// load + ATTACH). Called by `Store` after every mutating method
    /// so reads from the same `DuckDbQuery` instance always see the
    /// latest version. Read-heavy workloads pay nothing extra
    /// because writes are the trigger.
    ///
    /// (TODO: investigate `lance-duckdb` extension internals — if
    /// there's a cheaper way to invalidate the cache, e.g. a
    /// `lance_refresh()` SQL function the extension may expose,
    /// substitute it here.)
    pub async fn refresh(&self) -> Result<(), StorageError> {
        let conn_arc = self.conn.clone();
        let path = self.lance_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            let path_str = path
                .to_str()
                .ok_or(StorageError::InvalidData("lance path must be UTF-8"))?;
            let escaped = path_str.replace('\'', "''");
            let new_conn = Connection::open_in_memory()?;
            new_conn.execute_batch("INSTALL lance; LOAD lance;")?;
            new_conn.execute_batch(&format!("ATTACH '{escaped}' AS ns (TYPE LANCE);"))?;
            // Swap the inner connection. Previous prepared
            // statements are dropped along with the old conn — that
            // matters if a caller cached a `Statement` outside the
            // mutex, but `DuckDbQuery` always re-prepares per call,
            // so it's safe.
            *conn_arc.lock().expect("duckdb_query mutex poisoned") = new_conn;
            Ok(())
        })
        .await
        .map_err(|e| StorageError::InvalidInput(format!("spawn_blocking join: {e}")))?
    }
}

/// Run a synchronous DuckDB query body on a blocking-pool thread and
/// surface the result back to the async caller. Standardizes the
/// `spawn_blocking` ↔ `StorageError` conversion so individual methods
/// stay clean.
pub(super) async fn spawn_blocking_storage<T, F>(f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| StorageError::InvalidInput(format!("spawn_blocking join: {e}")))?
}

/// `spawn_blocking_storage` analogue for graph methods, which
/// surface `GraphError` instead of `StorageError`. Returns
/// `GraphError::Backend` for both spawn-join failures and per-row
/// `duckdb::Error`s.
pub(super) async fn spawn_blocking_graph<T, F>(f: F) -> Result<T, GraphError>
where
    F: FnOnce() -> Result<T, GraphError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| GraphError::Backend(format!("spawn_blocking join: {e}")))?
}

/// Decode a `graph_edges` row into a [`GraphEdge`]. The `valid_to`
/// column is nullable (closed edges have a timestamp; active edges
/// are NULL).
pub(super) fn row_to_graph_edge(row: &duckdb::Row<'_>) -> duckdb::Result<GraphEdge> {
    Ok(GraphEdge {
        from_node_id: row.get(0)?,
        to_node_id: row.get(1)?,
        relation: row.get(2)?,
        valid_from: row.get(3)?,
        valid_to: row.get(4)?,
        confidence: row.get(5)?,
        extractor: row.get(6)?,
    })
}

/// Decode an `entities` row into an [`Entity`]. The `kind` column is
/// stored as a snake_case Utf8 string (matches LanceStore's writer
/// encoding); we go through `EntityKind::from_db_str` rather than a
/// serde round trip because the domain type already exposes that
/// helper and it's cheaper than the serde detour.
pub(super) fn row_to_entity(row: &duckdb::Row<'_>) -> duckdb::Result<Entity> {
    let kind: String = row.get(3)?;
    Ok(Entity {
        entity_id: row.get(0)?,
        tenant: row.get(1)?,
        canonical_name: row.get(2)?,
        kind: EntityKind::from_db_str(&kind).ok_or_else(|| {
            duckdb::Error::FromSqlConversionFailure(
                3,
                duckdb::types::Type::Text,
                format!("invalid entity kind {kind:?}").into(),
            )
        })?,
        created_at: row.get(4)?,
    })
}

/// Serialize an enum to its snake_case Utf8 storage form, matching what
/// LanceStore writes. Inverse of `parse_enum`. Used for SQL parameter
/// binding when filtering by enum-string columns (e.g.
/// `status = 'pending_confirmation'`).
pub(super) fn enum_to_text<T: Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or(StorageError::InvalidData("enum serializes as non-string"))
}

pub(super) fn parse_enum<T: DeserializeOwned>(value: &str) -> duckdb::Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|e| {
        duckdb::Error::FromSqlConversionFailure(0, duckdb::types::Type::Text, Box::new(e))
    })
}

/// Extract a `LIST(VARCHAR)` column as `Vec<String>`. duckdb-rs 1.x
/// doesn't ship a `FromSql` impl for `Vec<String>`, so we go through the
/// `Value` enum. NULL list → empty Vec (mem semantics: missing list ==
/// no items).
pub(super) fn get_string_list(row: &duckdb::Row<'_>, idx: usize) -> duckdb::Result<Vec<String>> {
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
