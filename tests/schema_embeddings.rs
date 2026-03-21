use mem::storage::DuckDbRepository;

fn table_column_names(conn: &duckdb::Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("pragma table_info('{table}')"))
        .expect("pragma prepare");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("pragma query");
    rows.map(|r| r.expect("row")).collect()
}

#[tokio::test]
async fn bootstrap_creates_embedding_tables_with_expected_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("embeddings_schema.duckdb");

    {
        let _repo = DuckDbRepository::open(&db).await.expect("open repo");
    }

    let conn = duckdb::Connection::open(&db).expect("reopen");

    let mut stmt = conn
        .prepare(
            "select table_name from information_schema.tables
             where table_catalog = current_database()
               and table_schema = 'main'
               and table_name in ('memory_embeddings', 'embedding_jobs')",
        )
        .expect("prepare");
    let names: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"memory_embeddings".to_string()));
    assert!(names.contains(&"embedding_jobs".to_string()));

    let mem_cols = table_column_names(&conn, "memory_embeddings");
    for expected in [
        "memory_id",
        "tenant",
        "embedding_model",
        "embedding_dim",
        "embedding",
        "content_hash",
        "source_updated_at",
        "created_at",
        "updated_at",
    ] {
        assert!(
            mem_cols.iter().any(|c| c == expected),
            "memory_embeddings missing column {expected}: {mem_cols:?}"
        );
    }

    let job_cols = table_column_names(&conn, "embedding_jobs");
    for expected in [
        "job_id",
        "tenant",
        "memory_id",
        "target_content_hash",
        "provider",
        "status",
        "attempt_count",
        "last_error",
        "available_at",
        "created_at",
        "updated_at",
    ] {
        assert!(
            job_cols.iter().any(|c| c == expected),
            "embedding_jobs missing column {expected}: {job_cols:?}"
        );
    }
}
