//! PoC: cross-table hybrid search via DuckDB SQL.
//!
//! Hypothesis (see chat thread): we can compute hybrid (BM25 + vector +
//! RRF) ranking purely in SQL, by joining the lance extension's
//! `lance_fts(capability_capsules)` with `lance_vector_search(
//! capability_capsule_embeddings)` on `capability_capsule_id` and computing
//! RRF in a CTE. If true, no schema co-location is needed.
//!
//! This PoC validates:
//!   1. Both lance_fts and lance_vector_search work against tables sized
//!      like our real ones (5 rows, 3-dim vectors), with FTS index built
//!      on capability_capsules.content.
//!   2. The two table functions return rows with `capability_capsule_id`
//!      so a FULL OUTER JOIN works.
//!   3. ROW_NUMBER() in DuckDB CTE produces stable ranks for RRF.
//!   4. Tenant filter pushdown in outer WHERE doesn't blow up the inner
//!      top-K (with mild oversample).
//!   5. Final RRF order matches expectation: items in both sources rank
//!      above items in only one source.
//!
//! Run with:
//!   cargo run --example hybrid_sql_poc

use std::sync::Arc;

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder, StringBuilder, UInt32Builder};
use arrow_array::{Array, RecordBatch};
use duckdb::Connection;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};

const VEC_DIM: i32 = 3;

/// Fixture row: (id, tenant, status, content, updated_at, version, vector).
type Row = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    u32,
    [f32; 3],
);

/// Hybrid result row: (id, content, rrf_score, rank_lex?, rank_sem?, cos?).
type HybridRow = (String, String, f64, Option<i64>, Option<i64>, Option<f64>);

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let lance_dir = tmp
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("tempdir path not utf-8"))?;
    println!("lance dir: {lance_dir}\n");

    // ── 1. Build tables: capability_capsules + capability_capsule_embeddings
    println!("=== 1. Build Lance tables (capsules + embeddings) ===");
    let lance_conn = lancedb::connect(lance_dir).execute().await?;

    let capsules_schema = Arc::new(Schema::new(vec![
        Field::new("capability_capsule_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
        Field::new("version", DataType::UInt32, false),
    ]));

    let embeddings_schema = Arc::new(Schema::new(vec![
        Field::new("capability_capsule_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                VEC_DIM,
            ),
            false,
        ),
    ]));

    // Fixture rows. Vectors hand-normalized so cos_sim = 1 - L2²/2 holds.
    // Query embedding will be roughly [0.71, 0.71, 0] — close to c3.
    // Query text "ANN HNSW retrieval" — should hit c2, c3, weakly c1.
    let rows: Vec<Row> = vec![
        // c1: text mentions storage; vector orthogonal to query
        (
            "c1",
            "local",
            "active",
            "DuckDB stores canonical capsule records and indexes",
            "20250501000001",
            1,
            [1.0, 0.0, 0.0],
        ),
        // c2: BM25 hit (HNSW); vector still mostly off
        (
            "c2",
            "local",
            "active",
            "Lance datasets support ANN via HNSW",
            "20250501000002",
            1,
            [0.0, 1.0, 0.0],
        ),
        // c3: BM25 + vector both strong — should rank top
        (
            "c3",
            "local",
            "active",
            "ANN retrieval with HNSW and inverted lists",
            "20250501000003",
            1,
            [0.707, 0.707, 0.0],
        ),
        // c4: BM25 weak (Tantivy / BM25 mention); vector orthogonal
        (
            "c4",
            "local",
            "active",
            "Tantivy provides BM25 lexical search",
            "20250501000004",
            1,
            [0.0, 0.0, 1.0],
        ),
        // c5: cross-tenant — must NEVER appear in results
        (
            "c5",
            "other",
            "active",
            "Cross-tenant noise that mentions ANN HNSW",
            "20250501000005",
            1,
            [0.5, 0.5, 0.5],
        ),
    ];

    // Build capsules batch
    let mut id_b = StringBuilder::new();
    let mut tenant_b = StringBuilder::new();
    let mut status_b = StringBuilder::new();
    let mut content_b = StringBuilder::new();
    let mut updated_b = StringBuilder::new();
    let mut version_b = UInt32Builder::new();
    for (id, tenant, status, content, updated, version, _vec) in &rows {
        id_b.append_value(id);
        tenant_b.append_value(tenant);
        status_b.append_value(status);
        content_b.append_value(content);
        updated_b.append_value(updated);
        version_b.append_value(*version);
    }
    let capsules_batch = RecordBatch::try_new(
        capsules_schema.clone(),
        vec![
            Arc::new(id_b.finish()) as Arc<dyn Array>,
            Arc::new(tenant_b.finish()),
            Arc::new(status_b.finish()),
            Arc::new(content_b.finish()),
            Arc::new(updated_b.finish()),
            Arc::new(version_b.finish()),
        ],
    )?;
    let capsules = lance_conn
        .create_table("capability_capsules", capsules_batch)
        .execute()
        .await?;
    println!("created capability_capsules ({} rows)", rows.len());

    // Build embeddings batch
    let mut eid_b = StringBuilder::new();
    let mut etenant_b = StringBuilder::new();
    let mut emb_b = FixedSizeListBuilder::with_capacity(Float32Builder::new(), VEC_DIM, rows.len());
    for (id, tenant, _status, _content, _updated, _version, vec) in &rows {
        eid_b.append_value(id);
        etenant_b.append_value(tenant);
        for x in vec {
            emb_b.values().append_value(*x);
        }
        emb_b.append(true);
    }
    let embeddings_batch = RecordBatch::try_new(
        embeddings_schema,
        vec![
            Arc::new(eid_b.finish()) as Arc<dyn Array>,
            Arc::new(etenant_b.finish()),
            Arc::new(emb_b.finish()),
        ],
    )?;
    let _embeddings = lance_conn
        .create_table("capability_capsule_embeddings", embeddings_batch)
        .execute()
        .await?;
    println!(
        "created capability_capsule_embeddings ({} rows)",
        rows.len()
    );

    // ── 2. Build FTS index on capsules.content
    println!("\n=== 2. Build FTS index on capability_capsules.content ===");
    capsules
        .create_index(
            &["content"],
            lancedb::index::Index::FTS(lancedb::index::scalar::FtsIndexBuilder::default()),
        )
        .execute()
        .await?;
    println!("FTS index built");

    // ── 3. INSTALL lance / LOAD lance / ATTACH in DuckDB
    println!("\n=== 3. DuckDB: INSTALL lance; LOAD lance; ATTACH ===");
    let dconn = Connection::open_in_memory()?;
    dconn.execute_batch("INSTALL lance; LOAD lance;")?;
    let attach = format!("ATTACH '{lance_dir}' AS ns (TYPE LANCE);");
    dconn.execute_batch(&attach)?;
    println!("attached");

    // Sanity: SELECT through DuckDB sees both tables
    let cnt: i64 = dconn.query_row(
        "SELECT count(*) FROM ns.main.capability_capsules",
        [],
        |r| r.get(0),
    )?;
    println!("capsules visible: {cnt}");
    let ecnt: i64 = dconn.query_row(
        "SELECT count(*) FROM ns.main.capability_capsule_embeddings",
        [],
        |r| r.get(0),
    )?;
    println!("embeddings visible: {ecnt}");

    // ── 4. Probe each side independently first
    println!("\n=== 4a. lance_fts probe (query='ANN HNSW retrieval') ===");
    let fts_q = "SELECT capability_capsule_id, _score \
                 FROM lance_fts('ns.main.capability_capsules', 'content', \
                                 'ANN HNSW retrieval', k => 8) \
                 WHERE tenant = 'local' AND status = 'active' \
                 ORDER BY _score DESC";
    match dconn.prepare(fts_q) {
        Ok(mut stmt) => {
            let rows: Vec<(String, f32)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (id, score) in &rows {
                println!("  fts: {id}  score={score:.4}");
            }
            if !rows.iter().any(|(id, _)| id == "c5") {
                println!("  ✓ c5 (other tenant) excluded");
            } else {
                println!("  ✗ c5 leaked — tenant filter not applied!");
            }
        }
        Err(e) => println!("  FTS prepare failed: {e}"),
    }

    println!("\n=== 4b. lance_vector_search probe (q≈[0.71, 0.71, 0]) ===");
    let vec_q = "SELECT e.capability_capsule_id, e._distance \
                 FROM lance_vector_search( \
                        'ns.main.capability_capsule_embeddings', 'embedding', \
                        [0.707, 0.707, 0.0]::FLOAT[], k => 8 \
                      ) AS e \
                 WHERE e.tenant = 'local' \
                 ORDER BY e._distance ASC";
    match dconn.prepare(vec_q) {
        Ok(mut stmt) => {
            let rows: Vec<(String, f32)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (id, dist) in &rows {
                let cos_sim = 1.0 - dist / 2.0;
                println!("  vec: {id}  L2²={dist:.4}  cos_sim={cos_sim:.4}");
            }
            if !rows.iter().any(|(id, _)| id == "c5") {
                println!("  ✓ c5 (other tenant) excluded");
            } else {
                println!("  ✗ c5 leaked — tenant filter not applied!");
            }
        }
        Err(e) => println!("  vector_search prepare failed: {e}"),
    }

    // ── 5. The full hybrid SQL
    println!("\n=== 5. Hybrid: lance_fts + lance_vector_search + RRF (k=60) ===");
    let hybrid_sql = "
        WITH
        fts AS (
            SELECT
                capability_capsule_id,
                _score AS bm25_score,
                ROW_NUMBER() OVER (ORDER BY _score DESC, capability_capsule_id ASC) AS rank_lex
            FROM lance_fts('ns.main.capability_capsules', 'content',
                           'ANN HNSW retrieval', k => 16)
            WHERE tenant = 'local' AND status = 'active'
        ),
        vec AS (
            SELECT
                e.capability_capsule_id,
                e._distance AS l2_squared,
                ROW_NUMBER() OVER (ORDER BY e._distance ASC, e.capability_capsule_id ASC) AS rank_sem
            FROM lance_vector_search(
                    'ns.main.capability_capsule_embeddings', 'embedding',
                    [0.707, 0.707, 0.0]::FLOAT[], k => 16
                 ) AS e
            WHERE e.tenant = 'local'
        ),
        fused AS (
            SELECT
                COALESCE(fts.capability_capsule_id, vec.capability_capsule_id) AS capability_capsule_id,
                  COALESCE(1.0 / (60.0 + fts.rank_lex), 0.0)
                + COALESCE(1.0 / (60.0 + vec.rank_sem), 0.0) AS rrf_score,
                fts.rank_lex,
                vec.rank_sem,
                vec.l2_squared
            FROM fts FULL OUTER JOIN vec USING (capability_capsule_id)
        )
        SELECT
            m.capability_capsule_id,
            m.content,
            f.rrf_score,
            f.rank_lex,
            f.rank_sem,
            CASE WHEN f.l2_squared IS NULL THEN NULL
                 ELSE 1.0 - f.l2_squared / 2.0
            END AS cos_sim
        FROM fused f
        JOIN ns.main.capability_capsules m USING (capability_capsule_id)
        WHERE m.tenant = 'local' AND m.status = 'active'
        ORDER BY f.rrf_score DESC, m.updated_at DESC, m.capability_capsule_id ASC
        LIMIT 10
    ";

    match dconn.prepare(hybrid_sql) {
        Ok(mut stmt) => {
            let rows: Vec<HybridRow> = stmt
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
                .collect::<Result<_, _>>()?;
            for (id, content, rrf, rl, rs, cos) in &rows {
                let rl_s = rl.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
                let rs_s = rs.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
                let cos_s = cos.map(|v| format!("{v:.4}")).unwrap_or_else(|| "—".into());
                println!(
                    "  {id}  rrf={rrf:.5}  rank_lex={rl_s}  rank_sem={rs_s}  cos={cos_s}\n      content: {content}",
                );
            }

            // Acceptance assertions
            println!();
            assert_eq!(
                rows[0].0, "c3",
                "expected c3 (both lex+sem hit) to rank top, got {}",
                rows[0].0
            );
            println!("  ✓ c3 ranked top (best of both sources)");

            let leaked = rows.iter().any(|(id, ..)| id == "c5");
            assert!(!leaked, "c5 (other tenant) leaked into results");
            println!("  ✓ c5 (cross-tenant) excluded");

            let any_single_source = rows
                .iter()
                .any(|(_, _, _, rl, rs, _)| rl.is_none() || rs.is_none());
            if any_single_source {
                println!("  ✓ FULL OUTER JOIN preserved single-source items");
            } else {
                println!(
                    "  (note: every result hit both sources — no single-source case in fixture)"
                );
            }
        }
        Err(e) => println!("  hybrid prepare failed: {e}"),
    }

    // ── 6. Edge case: empty FTS query (vec-only path)
    println!("\n=== 6a. Edge case: text query matches no FTS docs (vec-only fallback) ===");
    let no_fts_sql = "
        WITH
        fts AS (
            SELECT
                capability_capsule_id,
                _score AS bm25_score,
                ROW_NUMBER() OVER (ORDER BY _score DESC, capability_capsule_id ASC) AS rank_lex
            FROM lance_fts('ns.main.capability_capsules', 'content',
                           'zzznonexistentwordzzz', k => 16)
            WHERE tenant = 'local'
        ),
        vec AS (
            SELECT
                e.capability_capsule_id,
                e._distance AS l2_squared,
                ROW_NUMBER() OVER (ORDER BY e._distance ASC, e.capability_capsule_id ASC) AS rank_sem
            FROM lance_vector_search(
                    'ns.main.capability_capsule_embeddings', 'embedding',
                    [0.707, 0.707, 0.0]::FLOAT[], k => 16
                 ) AS e
            WHERE e.tenant = 'local'
        ),
        fused AS (
            SELECT
                COALESCE(fts.capability_capsule_id, vec.capability_capsule_id) AS capability_capsule_id,
                  COALESCE(1.0 / (60.0 + fts.rank_lex), 0.0)
                + COALESCE(1.0 / (60.0 + vec.rank_sem), 0.0) AS rrf_score,
                fts.rank_lex,
                vec.rank_sem
            FROM fts FULL OUTER JOIN vec USING (capability_capsule_id)
        )
        SELECT m.capability_capsule_id, f.rrf_score, f.rank_lex, f.rank_sem
        FROM fused f
        JOIN ns.main.capability_capsules m USING (capability_capsule_id)
        WHERE m.tenant = 'local'
        ORDER BY f.rrf_score DESC, m.capability_capsule_id ASC
        LIMIT 5
    ";
    match dconn.prepare(no_fts_sql) {
        Ok(mut stmt) => {
            let rows: Vec<(String, f64, Option<i64>, Option<i64>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<Result<_, _>>()?;
            for (id, rrf, rl, rs) in &rows {
                let rl_s = rl.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
                let rs_s = rs.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
                println!("  {id}  rrf={rrf:.5}  rank_lex={rl_s}  rank_sem={rs_s}");
            }
            let all_lex_null = rows.iter().all(|(_, _, rl, _)| rl.is_none());
            if all_lex_null {
                println!("  ✓ FTS empty → all rank_lex NULL → ranked by vector alone");
            } else {
                println!("  (note: FTS still matched something; tweak the test query)");
            }
        }
        Err(e) => println!("  empty-fts probe failed: {e}"),
    }

    println!("\n=== Done ===");
    println!("If all ✓ printed, the cross-table SQL hybrid is feasible without schema changes.");
    Ok(())
}
