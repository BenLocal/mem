//! PoC: probe the Lance DuckDB extension's capabilities for the
//! "LanceDB-as-storage + DuckDB-as-query-engine" architecture.
//!
//! Goals (each numbered as a printed step):
//!   1. Build a Lance dataset via the LanceDB Rust API.
//!   2. Open an in-memory DuckDB connection in the same process,
//!      INSTALL/LOAD the lance community extension, ATTACH the dataset.
//!   3. Verify SELECT through DuckDB SQL.
//!   4. Probe row-level DML: INSERT INTO / UPDATE / DELETE through
//!      DuckDB SQL against the lance namespace. Each is wrapped in a
//!      match so an unsupported statement prints the error rather than
//!      aborting the rest of the run.
//!   5. Probe Rust→DuckDB write visibility: write a row via the LanceDB
//!      Rust API, immediately re-query through the DuckDB SQL client.
//!      Then DETACH+re-ATTACH and re-query; report whether either path
//!      sees the new row.
//!   6. Probe lance_vector_search() and lance_fts() — verify the
//!      extension functions actually run, return the expected
//!      _distance / _score columns, and respect prefilter.
//!
//! Run with:
//!   cargo run --example lance_duckdb_poc --features lancedb
//!
//! First run downloads the community extension over HTTPS into
//! ~/.duckdb/extensions/. Subsequent runs are offline.

use std::sync::Arc;

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder, StringBuilder};
use arrow_array::{Array, RecordBatch};
use duckdb::Connection;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let lance_dir = tmp
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("tempdir path not utf-8"))?;
    println!("lance dir: {lance_dir}");

    // ── Step 1: build Lance dataset via LanceDB Rust API ──────────────
    println!("\n=== 1. Create Lance table 'animals' (3 rows) via Rust API ===");
    let lance_conn = lancedb::connect(lance_dir).execute().await?;

    let schema = Arc::new(Schema::new(vec![
        Field::new("animal", DataType::Utf8, false),
        Field::new("noise", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), 3),
            false,
        ),
    ]));

    let mut animal = StringBuilder::new();
    let mut noise = StringBuilder::new();
    let mut vec_b = FixedSizeListBuilder::with_capacity(Float32Builder::new(), 3, 3);
    for (a, n, v) in &[
        ("duck", "quack", [0.9_f32, 0.7, 0.1]),
        ("horse", "neigh", [0.3, 0.1, 0.5]),
        ("dragon", "roar", [0.5, 0.2, 0.7]),
    ] {
        animal.append_value(a);
        noise.append_value(n);
        for x in v {
            vec_b.values().append_value(*x);
        }
        vec_b.append(true);
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(animal.finish()) as Arc<dyn Array>,
            Arc::new(noise.finish()),
            Arc::new(vec_b.finish()),
        ],
    )?;

    let table = lance_conn.create_table("animals", batch).execute().await?;
    println!("created Lance table 'animals' with 3 rows");

    // ── Step 2: install + load + attach Lance extension in DuckDB ─────
    // `lance` is a core (not community) extension — see
    //   https://duckdb.org/docs/current/core_extensions/lance.html
    println!("\n=== 2. DuckDB: INSTALL lance; LOAD lance; ATTACH (first run downloads) ===");
    let dconn = Connection::open_in_memory()?;
    match dconn.execute_batch("INSTALL lance; LOAD lance;") {
        Ok(()) => println!("INSTALL + LOAD ok"),
        Err(e) => {
            eprintln!("INSTALL lance failed: {e}\naborting PoC.");
            return Ok(());
        }
    }
    let attach = format!("ATTACH '{lance_dir}' AS ns (TYPE LANCE);");
    dconn.execute_batch(&attach)?;
    println!("ATTACH '{lance_dir}' AS ns ok");

    // ── Step 3: SELECT round-trip ─────────────────────────────────────
    println!("\n=== 3. SELECT through DuckDB SQL ===");
    let mut stmt = dconn.prepare("SELECT animal, noise FROM ns.main.animals ORDER BY animal")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    println!("rows: {rows:?}");

    // ── Step 4: probe DML through DuckDB SQL ──────────────────────────
    println!("\n=== 4a. INSERT INTO ns.main.animals ===");
    match dconn.execute_batch(
        "INSERT INTO ns.main.animals VALUES ('cat', 'meow', [0.1, 0.2, 0.3]::FLOAT[]);",
    ) {
        Ok(()) => println!("INSERT INTO: OK ✓"),
        Err(e) => println!("INSERT INTO: FAILED — {e}"),
    }

    println!("\n=== 4b. UPDATE ns.main.animals SET ... ===");
    match dconn.execute_batch("UPDATE ns.main.animals SET noise = 'cluck' WHERE animal = 'duck';") {
        Ok(()) => println!("UPDATE: OK ✓"),
        Err(e) => println!("UPDATE: FAILED — {e}"),
    }

    println!("\n=== 4c. DELETE FROM ns.main.animals ===");
    match dconn.execute_batch("DELETE FROM ns.main.animals WHERE animal = 'horse';") {
        Ok(()) => println!("DELETE: OK ✓"),
        Err(e) => println!("DELETE: FAILED — {e}"),
    }

    println!("\n=== 4d. CREATE OR REPLACE TABLE (known-good baseline) ===");
    match dconn.execute_batch(
        "CREATE OR REPLACE TABLE ns.main.fruits AS \
         SELECT * FROM (VALUES \
           ('apple', 'crunch', [0.1, 0.2, 0.3]::FLOAT[]), \
           ('lemon', 'splash', [0.4, 0.5, 0.6]::FLOAT[]) \
         ) AS t(animal, noise, vector);",
    ) {
        Ok(()) => println!("CTAS: OK ✓"),
        Err(e) => println!("CTAS: FAILED — {e}"),
    }

    // ── Step 5: Rust write → DuckDB read visibility ───────────────────
    println!("\n=== 5. LanceDB Rust API write → immediate DuckDB SELECT ===");
    let mut a2 = StringBuilder::new();
    let mut n2 = StringBuilder::new();
    let mut v2 = FixedSizeListBuilder::with_capacity(Float32Builder::new(), 3, 1);
    a2.append_value("snake");
    n2.append_value("hiss");
    for x in &[0.4_f32, 0.4, 0.4] {
        v2.values().append_value(*x);
    }
    v2.append(true);
    let batch2 = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(a2.finish()) as Arc<dyn Array>,
            Arc::new(n2.finish()),
            Arc::new(v2.finish()),
        ],
    )?;
    table.add(batch2).execute().await?;
    println!("LanceDB Rust API: added 'snake'");

    let mut stmt = dconn.prepare("SELECT count(*) FROM ns.main.animals")?;
    let n_immediate: i64 = stmt.query_row([], |r| r.get(0))?;
    println!("DuckDB count immediately after Rust write: {n_immediate}");

    let mut stmt = dconn.prepare("SELECT count(*) FROM ns.main.animals WHERE animal = 'snake'")?;
    let snake_count: i64 = stmt.query_row([], |r| r.get(0))?;
    println!("'snake' rows visible immediately: {snake_count}");

    println!("\n=== 5b. DETACH + re-ATTACH and re-query ===");
    dconn.execute_batch("DETACH ns;")?;
    dconn.execute_batch(&attach)?;
    let mut stmt = dconn.prepare("SELECT count(*) FROM ns.main.animals")?;
    let n_reattach: i64 = stmt.query_row([], |r| r.get(0))?;
    println!("DuckDB count after DETACH+ATTACH: {n_reattach}");
    let mut stmt = dconn.prepare("SELECT count(*) FROM ns.main.animals WHERE animal = 'snake'")?;
    let snake_after: i64 = stmt.query_row([], |r| r.get(0))?;
    println!("'snake' rows visible after re-ATTACH: {snake_after}");

    // ── Step 6: lance_vector_search + lance_fts ───────────────────────
    println!("\n=== 6a. lance_vector_search ===");
    let q = "SELECT animal, _distance FROM lance_vector_search( \
             'ns.main.animals', 'vector', [0.9, 0.7, 0.1]::FLOAT[], k => 2 \
           );";
    match dconn.prepare(q) {
        Ok(mut stmt) => {
            match stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f32>(1)?))) {
                Ok(it) => {
                    let rows: Result<Vec<(String, f32)>, _> = it.collect();
                    println!("vector_search rows: {rows:?}");
                }
                Err(e) => println!("vector_search query_map FAILED: {e}"),
            }
        }
        Err(e) => println!("vector_search prepare FAILED: {e}"),
    }

    println!("\n=== 6b. lance_fts (will only succeed if FTS index exists) ===");
    // Try without a pre-built FTS index first — most likely fails.
    let fts = "SELECT animal, _score FROM lance_fts('ns.main.animals', 'noise', 'quack', k => 2);";
    match dconn.prepare(fts) {
        Ok(mut stmt) => {
            match stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f32>(1)?))) {
                Ok(it) => {
                    let rows: Result<Vec<(String, f32)>, _> = it.collect();
                    println!("fts rows: {rows:?}");
                }
                Err(e) => println!("fts query_map FAILED: {e}"),
            }
        }
        Err(e) => println!("fts prepare FAILED (expected — no FTS index built): {e}"),
    }

    println!("\n=== Done. Summary ===");
    println!("(scroll up: each numbered step printed OK ✓ or FAILED with reason.)");
    Ok(())
}
