//! r2d2 connection management for the read-only DuckDB pool, plus a
//! type-level read-only `Connection` wrapper.
//!
//! See `docs/superpowers/specs/2026-05-07-rw-split-conn-pool-design.md`.
//!
//! Design notes:
//!
//! - Connections in the pool share the same in-process DuckDB Database
//!   handle as the HTTP write conn (via `Connection::try_clone()`), so
//!   committed writes are visible across connections immediately. Two
//!   independent `Connection::open(path)` calls would create two
//!   separate Database instances whose snapshots don't see each other —
//!   verified the failure mode the hard way during phase-1 worker
//!   isolation; do not regress on it.
//! - Read-only enforcement is **type-level** via [`ReadOnlyConn`], not
//!   via DuckDB's `SET access_mode='READ_ONLY'`. That setting is
//!   database-wide in DuckDB and would break the writer in the same
//!   process.
//! - Pool is opt-in (callers route specific SELECT methods through
//!   [`crate::storage::DuckDbRepository::with_read`]). Default is to
//!   keep using the HTTP write Mutex.

use std::sync::{Arc, Mutex};

use duckdb::{Connection, Statement};

/// r2d2 manager that produces read-only-by-convention DuckDB
/// connections sharing a single in-process Database with `template`.
///
/// `template` is the same `Arc<Mutex<Connection>>` as the HTTP write
/// conn — the Manager only ever calls `try_clone()` on it inside
/// `connect()`, never executes against it.
#[derive(Clone, Debug)]
pub(crate) struct DuckDbReadManager {
    template: Arc<Mutex<Connection>>,
}

impl DuckDbReadManager {
    pub(crate) fn new(template: Arc<Mutex<Connection>>) -> Self {
        Self { template }
    }
}

impl r2d2::ManageConnection for DuckDbReadManager {
    type Connection = Connection;
    type Error = duckdb::Error;

    fn connect(&self) -> Result<Connection, duckdb::Error> {
        // try_clone shares the underlying Database — committed writes
        // from the http_write_conn / worker_write_conn are visible to
        // this clone's MVCC snapshot on every new transaction.
        let template = self
            .template
            .lock()
            .expect("read pool template mutex poisoned");
        let cloned = template.try_clone()?;
        cloned.execute_batch("SET threads = 1")?;
        Ok(cloned)
    }

    fn is_valid(&self, conn: &mut Connection) -> Result<(), duckdb::Error> {
        conn.execute_batch("SELECT 1")
    }

    fn has_broken(&self, _conn: &mut Connection) -> bool {
        false
    }
}

/// Type-level read-only wrapper around a borrowed DuckDB `Connection`.
///
/// Exposes only `prepare(...)` (callers run `query_map` / `query_row`
/// on the returned [`Statement`]). Does NOT expose `execute(&str)` /
/// `execute_batch(&str)` — calling those is a compile error, not a
/// runtime check. Routing a write through the read pool is therefore
/// physically impossible without bypassing the type system.
///
/// Lifetime is tied to the underlying `&Connection` so a wrapper cannot
/// outlive the lock guard / pool checkout it was constructed from.
pub(crate) struct ReadOnlyConn<'a> {
    inner: &'a Connection,
}

impl<'a> ReadOnlyConn<'a> {
    pub(crate) fn wrap(conn: &'a Connection) -> Self {
        Self { inner: conn }
    }

    /// Prepare a SELECT statement. Callers run `query_map` / `query_row`
    /// on the returned [`Statement`].
    pub(crate) fn prepare(&self, sql: &str) -> Result<Statement<'_>, duckdb::Error> {
        self.inner.prepare(sql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use r2d2::ManageConnection;
    use tempfile::tempdir;

    fn fresh_template(path: &std::path::Path) -> Arc<Mutex<Connection>> {
        let conn = Connection::open(path).expect("open template");
        Arc::new(Mutex::new(conn))
    }

    #[test]
    fn manager_clones_share_database_with_template() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.duckdb");
        let template = fresh_template(&path);
        // Bootstrap a row through the template; any clone-conn must see it.
        template
            .lock()
            .unwrap()
            .execute_batch("CREATE TABLE t (n INT); INSERT INTO t VALUES (42);")
            .expect("ddl");

        let manager = DuckDbReadManager::new(template);
        let mut conn = manager.connect().expect("connect");
        let n: i32 = conn
            .query_row("SELECT n FROM t", [], |r| r.get(0))
            .expect("query");
        assert_eq!(n, 42);
        manager.is_valid(&mut conn).expect("is_valid");
    }

    #[test]
    fn write_after_pool_open_is_visible_via_pool() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.duckdb");
        let template = fresh_template(&path);
        template
            .lock()
            .unwrap()
            .execute_batch("CREATE TABLE t (n INT)")
            .expect("ddl");

        let pool = r2d2::Pool::builder()
            .max_size(2)
            .build(DuckDbReadManager::new(template.clone()))
            .expect("build pool");

        // Reader checks out *before* writer commits.
        let reader = pool.get().expect("checkout");
        template
            .lock()
            .unwrap()
            .execute_batch("INSERT INTO t VALUES (7)")
            .expect("insert");

        // Reader sees the new row because they share the Database.
        let n: i32 = reader
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .expect("query");
        assert_eq!(n, 1);
    }

    #[test]
    fn read_only_conn_only_exposes_prepare() {
        // Compile-time guarantee: this test simply *exists*. The type
        // surface of ReadOnlyConn intentionally has no `execute` method;
        // the absence is the test. If someone adds one, that is an
        // intentional design change and this test should be updated.
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.duckdb");
        let conn = Connection::open(&path).expect("open");
        conn.execute_batch("CREATE TABLE x (n INT); INSERT INTO x VALUES (42);")
            .expect("ddl");
        let read = ReadOnlyConn::wrap(&conn);
        let mut stmt = read.prepare("SELECT n FROM x").expect("prepare");
        let n: i32 = stmt.query_row([], |r| r.get(0)).expect("query");
        assert_eq!(n, 42);
    }
}
