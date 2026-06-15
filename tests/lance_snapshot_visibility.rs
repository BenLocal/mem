//! Regression guard for the linchpin assumption behind
//! `DuckDbQuery::refresh()` / `ensure_fresh()` / the `dirty` flag: a
//! read-only DuckDB connection that has ATTACHed a lance dataset pins
//! the dataset version at first query, and the lance extension does NOT
//! surface subsequent Rust-API writes (append OR update) to that
//! connection through ANY same-connection re-attach primitive. Only a
//! brand-new `Connection` (fresh INSTALL/LOAD/ATTACH) sees them.
//!
//! This was originally a throwaway `examples/lance_refresh_probe.rs`
//! used to settle a design question (can the read path use a cheap
//! warm-connection refresh, or a r2d2 pool with cheap staleness
//! invalidation?). The probe proved BOTH cheap options are blocked at
//! the lance-extension layer, so the read path stays single-connection
//! (mutex-serialized, full-rebuild-on-dirty). See
//! `docs/duckdb-read-path-strategy.md`.
//!
//! Promoting it to a test makes that assumption a CI-enforced invariant:
//! if a future lance/duckdb upgrade makes a same-connection re-attach
//! (or no refresh at all) start surfacing writes, the relevant assert
//! below flips and forces us to revisit the warm-connection design (the
//! ~100ms full rebuild could then be replaced by a cheap re-attach).

use std::sync::Arc;

use arrow_array::builder::{Int32Builder, StringBuilder};
use arrow_array::{Array, RecordBatch};
use duckdb::Connection;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};

fn items_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("status", DataType::Utf8, false),
    ]))
}

fn rows_batch(schema: &Arc<Schema>, ids: &[(i32, &str)]) -> RecordBatch {
    let mut idb = Int32Builder::new();
    let mut sb = StringBuilder::new();
    for (id, status) in ids {
        idb.append_value(*id);
        sb.append_value(status);
    }
    RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(idb.finish()) as Arc<dyn Array>,
            Arc::new(sb.finish()),
        ],
    )
    .unwrap()
}

/// Open a fresh in-memory DuckDB, load the lance extension, ATTACH the
/// dataset dir. Mirrors `DuckDbQuery::open` / `refresh`.
fn fresh_conn(lance_dir: &str) -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("INSTALL lance; LOAD lance;").unwrap();
    c.execute_batch(&format!("ATTACH '{lance_dir}' AS ns (TYPE LANCE);"))
        .unwrap();
    c
}

fn count_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM ns.main.items", [], |r| r.get(0))
        .unwrap()
}

fn count_active(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM ns.main.items WHERE status = 'active'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn lance_writes_only_visible_to_fresh_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let lance_dir = tmp.path().to_str().unwrap().to_string();
    let schema = items_schema();

    // Seed: 3 rows, all status='pending'.
    let db = lancedb::connect(&lance_dir).execute().await.unwrap();
    let table = db
        .create_table(
            "items",
            rows_batch(&schema, &[(1, "pending"), (2, "pending"), (3, "pending")]),
        )
        .execute()
        .await
        .unwrap();

    // Read-only probe connection: attach + SELECT pins the snapshot.
    let conn = fresh_conn(&lance_dir);
    assert_eq!(count_rows(&conn), 3, "baseline rows");
    assert_eq!(count_active(&conn), 0, "baseline active");

    // ── CASE 1: Rust-API APPEND (id=4) ────────────────────────────────
    table
        .add(rows_batch(&schema, &[(4, "pending")]))
        .execute()
        .await
        .unwrap();

    // (a) no refresh — pinned snapshot, append invisible.
    assert_eq!(
        count_rows(&conn),
        3,
        "pinned conn must NOT see the Rust-API append without a refresh"
    );
    // (b) DETACH + re-ATTACH on the SAME connection — still invisible.
    conn.execute_batch("DETACH ns;").unwrap();
    conn.execute_batch(&format!("ATTACH '{lance_dir}' AS ns (TYPE LANCE);"))
        .unwrap();
    assert_eq!(
        count_rows(&conn),
        3,
        "DETACH+re-ATTACH must NOT clear the lance snapshot cache (append)"
    );
    // (b2) ATTACH OR REPLACE on the SAME connection — still invisible.
    conn.execute_batch(&format!(
        "ATTACH OR REPLACE '{lance_dir}' AS ns (TYPE LANCE);"
    ))
    .unwrap();
    assert_eq!(
        count_rows(&conn),
        3,
        "ATTACH OR REPLACE must NOT clear the lance snapshot cache (append)"
    );
    // (c) brand-new Connection — append finally visible.
    let c_fresh = fresh_conn(&lance_dir);
    assert_eq!(
        count_rows(&c_fresh),
        4,
        "only a brand-new Connection sees the Rust-API append"
    );

    // ── CASE 2: Rust-API UPDATE (id=1 → active) — the hard case ───────
    // Re-pin on a fresh read-only conn so CASE 1's re-attaches don't
    // mask CASE 2.
    let conn = fresh_conn(&lance_dir);
    assert_eq!(count_active(&conn), 0, "re-pinned active before update");
    table
        .update()
        .only_if("id = 1")
        .column("status", "'active'")
        .execute()
        .await
        .unwrap();

    // (a) no refresh — update invisible.
    assert_eq!(
        count_active(&conn),
        0,
        "pinned conn must NOT see the Rust-API update without a refresh"
    );
    // (b) DETACH + re-ATTACH — still invisible.
    conn.execute_batch("DETACH ns;").unwrap();
    conn.execute_batch(&format!("ATTACH '{lance_dir}' AS ns (TYPE LANCE);"))
        .unwrap();
    assert_eq!(
        count_active(&conn),
        0,
        "DETACH+re-ATTACH must NOT clear the lance snapshot cache (update)"
    );
    // (b2) ATTACH OR REPLACE — still invisible.
    conn.execute_batch(&format!(
        "ATTACH OR REPLACE '{lance_dir}' AS ns (TYPE LANCE);"
    ))
    .unwrap();
    assert_eq!(
        count_active(&conn),
        0,
        "ATTACH OR REPLACE must NOT clear the lance snapshot cache (update)"
    );
    // (c) brand-new Connection — update finally visible.
    let c_fresh = fresh_conn(&lance_dir);
    assert_eq!(
        count_active(&c_fresh),
        1,
        "only a brand-new Connection sees the Rust-API update"
    );
}
