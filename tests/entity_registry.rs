//! Integration tests for the entity registry (closes ROADMAP #8). See spec
//! docs/superpowers/specs/2026-05-02-entity-registry-design.md.
//!
//! ### Composite-PK ON CONFLICT probe outcome (Task 1, 2026-05-02)
//! `INSERT … ON CONFLICT (tenant, alias_text) DO NOTHING` is **SUPPORTED** by
//! the bundled DuckDB version. Task 5's `add_alias` uses this idiom for the
//! "alias already exists, idempotent re-add" case.
//! Re-run the probe (`#[ignore]`'d below) on DuckDB upgrades.

#[test]
#[ignore]
fn composite_pk_on_conflict_probe() {
    // Run: cargo test --test entity_registry composite_pk_on_conflict_probe -- --ignored --nocapture
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("probe.duckdb");
    let conn = duckdb::Connection::open(&db).unwrap();
    conn.execute_batch(
        "create table t (tenant text not null, alias text not null, payload text not null, primary key (tenant, alias));"
    ).unwrap();
    conn.execute_batch("insert into t values ('local', 'rust', 'first');")
        .unwrap();

    let result = conn.execute_batch(
        "insert into t (tenant, alias, payload) values ('local', 'rust', 'second') on conflict (tenant, alias) do nothing;"
    );

    match result {
        Ok(_) => {
            // Verify the original row is preserved.
            let payload: String = conn
                .query_row(
                    "select payload from t where tenant='local' and alias='rust'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                payload, "first",
                "ON CONFLICT DO NOTHING should preserve original"
            );
            println!("Composite-PK ON CONFLICT DO NOTHING SUPPORTED — Task 5 add_alias can use it");
        }
        Err(e) => {
            println!("Composite-PK ON CONFLICT NOT SUPPORTED: {e}");
            println!("Task 5 add_alias must use SELECT-then-INSERT under a single mutex hold");
        }
    }
}

use mem::storage::DuckDbRepository;
use tempfile::TempDir;

#[tokio::test]
async fn schema_creates_entities_aliases_and_topics_column() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let _repo = DuckDbRepository::open(&db).await.unwrap();

    let conn = duckdb::Connection::open(&db).unwrap();

    let entities_count: i64 = conn
        .query_row(
            "select count(*) from information_schema.tables where table_name='entities'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(entities_count, 1, "entities table should exist");

    let aliases_count: i64 = conn
        .query_row(
            "select count(*) from information_schema.tables where table_name='entity_aliases'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(aliases_count, 1, "entity_aliases table should exist");

    // memories.topics column added by the ALTER in 008.
    let topics_col: i64 = conn
        .query_row(
            "select count(*) from information_schema.columns where table_name='memories' and column_name='topics'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(topics_col, 1, "memories.topics column should exist");

    // CHECK constraint on entities.kind: invalid kind rejected.
    let bad = conn.execute(
        "insert into entities (entity_id, tenant, canonical_name, kind, created_at) \
         values ('e1', 't', 'X', 'bogus', '00000000020260502000')",
        [],
    );
    assert!(bad.is_err(), "kind='bogus' should violate CHECK constraint");
}

#[tokio::test]
async fn schema_bootstrap_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let _repo1 = DuckDbRepository::open(&db).await.unwrap();
    drop(_repo1);
    let _repo2 = DuckDbRepository::open(&db).await.unwrap();
    // No panic: re-opening must not fail on duplicate ALTER.
}
