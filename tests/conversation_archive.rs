use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::storage::DuckDbRepository;
use tempfile::TempDir;

fn sample_message(suffix: &str, embed: bool, block_type: BlockType) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("mb-{suffix}"),
        session_id: Some("sess-1".to_string()),
        tenant: "local".to_string(),
        caller_agent: "claude-code".to_string(),
        transcript_path: "/tmp/transcript.jsonl".to_string(),
        line_number: 1,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type,
        content: format!("content-{suffix}"),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: embed,
        created_at: "2026-04-30T00:00:00Z".to_string(),
    }
}

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

#[tokio::test]
async fn create_conversation_message_inserts_row_and_optionally_enqueues_job() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    // Eligible: row + job
    let m1 = sample_message("eligible", true, BlockType::Text);
    repo.create_conversation_message(&m1).await.unwrap();

    let conn = duckdb::Connection::open(&db).unwrap();
    let cm_count: i64 = conn
        .query_row("SELECT count(*) FROM conversation_messages", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(cm_count, 1);

    let job_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM transcript_embedding_jobs WHERE message_block_id = 'mb-eligible'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(job_count, 1, "embed_eligible=true should enqueue a job");

    // Ineligible: row but no job
    let mut m2 = sample_message("ineligible", false, BlockType::ToolUse);
    m2.line_number = 2;
    repo.create_conversation_message(&m2).await.unwrap();

    let job_count_2: i64 = conn
        .query_row(
            "SELECT count(*) FROM transcript_embedding_jobs WHERE message_block_id = 'mb-ineligible'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(job_count_2, 0, "embed_eligible=false should not enqueue");
}

#[tokio::test]
async fn create_conversation_message_is_idempotent_on_unique_conflict() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let m = sample_message("first", true, BlockType::Text);
    repo.create_conversation_message(&m).await.unwrap();

    // Second call with same (transcript_path, line_number, block_index) but different
    // message_block_id -> no error, no second row, no second job.
    let mut m2 = m.clone();
    m2.message_block_id = "mb-different-id".to_string();
    repo.create_conversation_message(&m2).await.unwrap();

    let conn = duckdb::Connection::open(&db).unwrap();
    let cm_count: i64 = conn
        .query_row("SELECT count(*) FROM conversation_messages", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(cm_count, 1);

    let job_count: i64 = conn
        .query_row("SELECT count(*) FROM transcript_embedding_jobs", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(job_count, 1, "no duplicate job on idempotent insert");
}
