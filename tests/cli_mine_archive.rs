//! Integration tests for the dual-sink behavior of `mem mine`.
//!
//! These verify that a single `mem mine` invocation populates BOTH the
//! existing `memories` table (via the regex-extract pipeline) AND the new
//! `conversation_messages` table (via per-block POSTs to
//! `/transcripts/messages`). They also pin block-level idempotency: a second
//! invocation must not produce duplicate transcript rows or jobs.
//!
//! Server setup uses the `common::test_app_state` helper introduced in Task
//! 9 plus `mem::http::router()` to avoid loading a real embedding model.
//! `TranscriptService::provider` is `None` in this state, but ingest only
//! cares about the repo path — embedding-job rows are still enqueued.

use std::fs;

use mem::http;
use mem::service::MemoryService;
use mem::storage::DuckDbRepository;
use tempfile::{NamedTempFile, TempDir};

mod common;

/// Three-line transcript fixture. Lines 1 and 3 are user messages (one
/// `text` block and one `tool_result` block respectively); line 2 is the
/// assistant's reply with two blocks (`text` + `tool_use`). The text on
/// line 2 carries a `<mem-save>...` tag that the legacy extractor will
/// pick up — the dual-sink contract says the same scan must also archive
/// every block.
const TRANSCRIPT: &str = r##"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"please read README"}]},"sessionId":"sess-mine","timestamp":"2026-04-30T00:00:01Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>archive-pipeline-fact</mem-save>"},{"type":"tool_use","id":"tu-1","name":"Read","input":{"path":"README.md"}}]},"sessionId":"sess-mine","timestamp":"2026-04-30T00:00:02Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu-1","content":"# README contents"}]},"sessionId":"sess-mine","timestamp":"2026-04-30T00:00:03Z"}
"##;

/// Spin up a real axum server bound to a random port, returning the
/// base URL the CLI can post to and a handle to the open repo (so the
/// caller can close the server-side connection by dropping it).
async fn spawn_server(db_path: std::path::PathBuf) -> String {
    let repo = DuckDbRepository::open(&db_path).await.unwrap();
    repo.set_transcript_job_provider("embedanything");
    let state = common::test_app_state(repo.clone(), MemoryService::new(repo));
    let app = http::router().with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

#[tokio::test]
async fn mine_writes_to_both_memories_and_conversation_messages() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("mem.duckdb");
    let base_url = spawn_server(db_path.clone()).await;

    let transcript_file = NamedTempFile::new().unwrap();
    fs::write(transcript_file.path(), TRANSCRIPT).unwrap();

    let exit_code = mem::cli::mine::run(mem::cli::mine::MineArgs {
        transcript_path: transcript_file.path().to_path_buf(),
        tenant: "local".to_string(),
        agent: "claude-code".to_string(),
        base_url: base_url.clone(),
    })
    .await;
    assert_eq!(exit_code, 0, "mine should exit 0 on a clean transcript");

    // Open a parallel read-only DuckDB connection to assert. We avoid
    // touching the server's own connection (single-writer mutex).
    let conn = duckdb::Connection::open(&db_path).unwrap();

    let cm: i64 = conn
        .query_row("SELECT count(*) FROM conversation_messages", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        cm, 4,
        "expect one row per block: user-text + assistant-text + assistant-tool_use + user-tool_result"
    );

    let mem_count: i64 = conn
        .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    assert!(
        mem_count >= 1,
        "expect at least the <mem-save> extracted memory; got {mem_count}"
    );

    let teq: i64 = conn
        .query_row("SELECT count(*) FROM transcript_embedding_jobs", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        teq, 2,
        "embed-eligible blocks (text only here) should each enqueue exactly one job"
    );
}

#[tokio::test]
async fn mine_is_idempotent_at_block_level() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("mem.duckdb");
    let base_url = spawn_server(db_path.clone()).await;

    let transcript_file = NamedTempFile::new().unwrap();
    fs::write(transcript_file.path(), TRANSCRIPT).unwrap();

    let args = mem::cli::mine::MineArgs {
        transcript_path: transcript_file.path().to_path_buf(),
        tenant: "local".to_string(),
        agent: "claude-code".to_string(),
        base_url,
    };

    let first = mem::cli::mine::run(mem::cli::mine::MineArgs {
        transcript_path: args.transcript_path.clone(),
        tenant: args.tenant.clone(),
        agent: args.agent.clone(),
        base_url: args.base_url.clone(),
    })
    .await;
    assert_eq!(first, 0);

    let conn = duckdb::Connection::open(&db_path).unwrap();
    let cm_first: i64 = conn
        .query_row("SELECT count(*) FROM conversation_messages", [], |r| {
            r.get(0)
        })
        .unwrap();
    let job_first: i64 = conn
        .query_row("SELECT count(*) FROM transcript_embedding_jobs", [], |r| {
            r.get(0)
        })
        .unwrap();
    drop(conn);

    let second = mem::cli::mine::run(args).await;
    assert_eq!(second, 0, "second run should also succeed (200 OK on dup)");

    let conn = duckdb::Connection::open(&db_path).unwrap();
    let cm_second: i64 = conn
        .query_row("SELECT count(*) FROM conversation_messages", [], |r| {
            r.get(0)
        })
        .unwrap();
    let job_second: i64 = conn
        .query_row("SELECT count(*) FROM transcript_embedding_jobs", [], |r| {
            r.get(0)
        })
        .unwrap();

    assert_eq!(
        cm_first, cm_second,
        "second mine must not insert duplicate transcript rows"
    );
    assert_eq!(
        job_first, job_second,
        "second mine must not enqueue duplicate embedding jobs"
    );
}
