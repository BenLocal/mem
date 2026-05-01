use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::storage::DuckDbRepository;
use tempfile::TempDir;

mod common;

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
    repo.set_transcript_job_provider("embedanything");

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
    repo.set_transcript_job_provider("embedanything");

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

#[tokio::test]
async fn get_by_session_returns_time_ordered_blocks() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    let mut m1 = sample_message("a", true, BlockType::Text);
    m1.created_at = "2026-04-30T00:00:02Z".to_string();
    m1.line_number = 1;

    let mut m2 = sample_message("b", true, BlockType::Text);
    m2.created_at = "2026-04-30T00:00:01Z".to_string();
    m2.line_number = 2;

    let mut m3 = sample_message("c", false, BlockType::ToolUse);
    m3.created_at = "2026-04-30T00:00:03Z".to_string();
    m3.line_number = 3;

    repo.create_conversation_message(&m1).await.unwrap();
    repo.create_conversation_message(&m2).await.unwrap();
    repo.create_conversation_message(&m3).await.unwrap();

    let out = repo
        .get_conversation_messages_by_session("local", "sess-1")
        .await
        .unwrap();

    assert_eq!(out.len(), 3);
    assert_eq!(out[0].message_block_id, "mb-b"); // earliest
    assert_eq!(out[1].message_block_id, "mb-a");
    assert_eq!(out[2].message_block_id, "mb-c"); // latest
}

#[tokio::test]
async fn fetch_conversation_messages_by_ids_preserves_input_order() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    for (i, suffix) in ["x", "y", "z"].iter().enumerate() {
        let mut m = sample_message(suffix, true, BlockType::Text);
        m.line_number = (i + 1) as u64;
        repo.create_conversation_message(&m).await.unwrap();
    }

    // Search returns ranked by score, so we ask the repo to fetch in a specific order.
    let ids = vec!["mb-z".to_string(), "mb-x".to_string(), "mb-y".to_string()];
    let out = repo
        .fetch_conversation_messages_by_ids("local", &ids)
        .await
        .unwrap();

    assert_eq!(out.len(), 3);
    assert_eq!(out[0].message_block_id, "mb-z");
    assert_eq!(out[1].message_block_id, "mb-x");
    assert_eq!(out[2].message_block_id, "mb-y");
}

#[tokio::test]
async fn transcript_embedding_job_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    let m = sample_message("life", true, BlockType::Text);
    repo.create_conversation_message(&m).await.unwrap();

    // Claim the next pending job.
    let now = "2026-04-30T00:00:00Z";
    let claimed = repo
        .claim_next_transcript_embedding_job(now, 5)
        .await
        .unwrap();
    let job = claimed.expect("should have one pending job");
    assert_eq!(job.message_block_id, "mb-life");
    assert_eq!(job.tenant, "local");
    assert_eq!(job.attempt_count, 0);

    // Second claim: nothing pending (the previous one is now 'processing').
    let none = repo
        .claim_next_transcript_embedding_job(now, 5)
        .await
        .unwrap();
    assert!(none.is_none());

    // Upsert embedding row.
    let blob = vec![0u8, 0, 128, 63, 0, 0, 0, 64]; // 1.0, 2.0 in LE f32 (sized for dim=2)
    repo.upsert_conversation_message_embedding(
        &job.message_block_id,
        &job.tenant,
        "fake-model",
        2,
        &blob,
        "fake-hash",
        &m.created_at,
        now,
    )
    .await
    .unwrap();

    // Complete the job.
    repo.complete_transcript_embedding_job(&job.job_id, now)
        .await
        .unwrap();

    let conn = duckdb::Connection::open(&db).unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM transcript_embedding_jobs WHERE job_id = ?",
            [&job.job_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "completed");
}

#[tokio::test]
async fn recent_conversation_messages_returns_newest_first_limited() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // Seed three messages with strictly increasing timestamps, distinct
    // (line_number, block_index) so the unique constraint accepts all three.
    let mut m1 = sample_message("1", true, BlockType::Text);
    m1.created_at = "2026-04-30T00:00:01Z".to_string();
    m1.line_number = 1;

    let mut m2 = sample_message("2", true, BlockType::Text);
    m2.created_at = "2026-04-30T00:00:02Z".to_string();
    m2.line_number = 2;

    let mut m3 = sample_message("3", true, BlockType::Text);
    m3.created_at = "2026-04-30T00:00:03Z".to_string();
    m3.line_number = 3;

    repo.create_conversation_message(&m1).await.unwrap();
    repo.create_conversation_message(&m2).await.unwrap();
    repo.create_conversation_message(&m3).await.unwrap();

    let out = repo.recent_conversation_messages("local", 2).await.unwrap();

    assert_eq!(out.len(), 2, "limit caps the result");
    assert_eq!(out[0].message_block_id, "mb-3", "newest first");
    assert_eq!(out[1].message_block_id, "mb-2");
}

// ---------------------------------------------------------------------------
// HTTP integration tests for the /transcripts/* surface (Task 9).
//
// These build the router directly via `common::test_app_state` instead of
// going through `app::router_with_config`, to avoid spinning up a real
// embedding provider for every test. The `TranscriptService` constructed by
// the helper has `provider = None`, which means semantic queries return zero
// hits; the empty-query / time-based fallback still works (used by the
// search filter test below).
// ---------------------------------------------------------------------------

mod http_routes {
    use super::*;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use mem::http;
    use mem::service::MemoryService;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    async fn build_router() -> (axum::Router, TempDir, DuckDbRepository) {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("mem.duckdb");
        let repo = DuckDbRepository::open(&db).await.unwrap();
        repo.set_transcript_job_provider("embedanything");
        let state = super::common::test_app_state(repo.clone(), MemoryService::new(repo.clone()));
        let router = http::router().with_state(state);
        (router, dir, repo)
    }

    #[tokio::test]
    async fn post_transcripts_messages_creates_a_row() {
        let (app, _dir, _repo) = build_router().await;

        let body = json!({
            "session_id": "sess-1",
            "tenant": "local",
            "caller_agent": "claude-code",
            "transcript_path": "/tmp/t.jsonl",
            "line_number": 1,
            "block_index": 0,
            "role": "assistant",
            "block_type": "text",
            "content": "hello",
            "embed_eligible": true,
            "created_at": "2026-04-30T00:00:00Z"
        });

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/transcripts/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["message_block_id"].is_string());
        let id = v["message_block_id"].as_str().unwrap();
        assert!(!id.is_empty(), "service should mint a non-empty id");
    }

    #[tokio::test]
    async fn get_transcripts_by_session_returns_blocks() {
        let (app, _dir, _repo) = build_router().await;

        // Seed two blocks via POST.
        for i in 0..2u64 {
            let body = json!({
                "session_id": "sess-X",
                "tenant": "local",
                "caller_agent": "claude-code",
                "transcript_path": "/tmp/t.jsonl",
                "line_number": i + 1,
                "block_index": 0,
                "role": "user",
                "block_type": "text",
                "content": format!("msg-{i}"),
                "embed_eligible": false,
                "created_at": format!("2026-04-30T00:00:0{i}Z")
            });
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/transcripts/messages")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // Fetch.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/transcripts?session_id=sess-X&tenant=local")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let messages = v["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "msg-0");
        assert_eq!(messages[1]["content"], "msg-1");
    }

    #[tokio::test]
    async fn post_transcripts_search_filters_by_role_and_block_type() {
        let (app, _dir, _repo) = build_router().await;

        // Seed: 1 user/text, 1 assistant/text, 1 assistant/tool_use.
        // Distinct (line_number) so the unique constraint accepts all three.
        let seeds = [
            ("user", "text", "user-says-hello", true, 1u64),
            ("assistant", "text", "assistant-answers", true, 2u64),
            (
                "assistant",
                "tool_use",
                r#"{"path":"README.md"}"#,
                false,
                3u64,
            ),
        ];
        for (role, block_type, content, eligible, line) in seeds {
            let body = json!({
                "session_id": "sess-Y",
                "tenant": "local",
                "caller_agent": "claude-code",
                "transcript_path": "/tmp/t.jsonl",
                "line_number": line,
                "block_index": 0,
                "role": role,
                "block_type": block_type,
                "content": content,
                "embed_eligible": eligible,
                "created_at": format!("2026-04-30T00:00:0{line}Z"),
            });
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/transcripts/messages")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // Empty query → recent-time fallback (provider is None in tests).
        // Filter by role=user → expect 1 hit.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/transcripts/search")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "query": "",
                            "tenant": "local",
                            "role": "user",
                            "limit": 10,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let windows = v["windows"].as_array().expect("windows array");
        assert_eq!(windows.len(), 1, "role=user filters to single window");
        let primaries: Vec<&str> = windows[0]["primary_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(primaries.len(), 1);
        let primary_block = windows[0]["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["is_primary"].as_bool() == Some(true))
            .expect("primary block in window");
        assert_eq!(primary_block["role"], "user");
        assert_eq!(primary_block["content"], "user-says-hello");

        // Filter by block_type=tool_use with empty query → expect 0 hits.
        // The empty-query browse path (recent_conversation_messages) filters
        // to embed_eligible = true rows so candidate primaries are symmetric
        // with the BM25/HNSW/anchor channels; tool_use blocks are ineligible
        // and therefore can never appear as primaries via empty-query browse.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/transcripts/search")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "query": "",
                            "tenant": "local",
                            "block_type": "tool_use",
                            "limit": 10,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let windows = v["windows"].as_array().expect("windows array");
        assert_eq!(
            windows.len(),
            0,
            "block_type=tool_use yields no primaries via empty-query browse \
             (tool_use is embed-ineligible)"
        );
    }
}
