use mem::storage::DuckDbRepository;
use tempfile::TempDir;

#[tokio::test]
async fn schema_creates_conversation_messages_and_jobs_tables() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let _repo = DuckDbRepository::open(&db).await.unwrap();

    let conn = duckdb::Connection::open(&db).unwrap();

    let cm: i64 = conn
        .query_row(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'conversation_messages'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cm, 1, "conversation_messages table should exist");

    let teq: i64 = conn
        .query_row(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'transcript_embedding_jobs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(teq, 1, "transcript_embedding_jobs table should exist");

    conn.execute(
        "INSERT INTO conversation_messages \
         (message_block_id, tenant, caller_agent, transcript_path, line_number, block_index, role, block_type, content, embed_eligible, created_at) \
         VALUES ('m1','t','a','/p',1,0,'user','text','hi',true,'2026-04-30T00:00:00Z')",
        [],
    )
    .unwrap();
    let dup = conn.execute(
        "INSERT INTO conversation_messages \
         (message_block_id, tenant, caller_agent, transcript_path, line_number, block_index, role, block_type, content, embed_eligible, created_at) \
         VALUES ('m2','t','a','/p',1,0,'user','text','hi',true,'2026-04-30T00:00:00Z')",
        [],
    );
    assert!(
        dup.is_err(),
        "duplicate (transcript_path,line_number,block_index) should be rejected"
    );
}
