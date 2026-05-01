//! Integration tests for the transcript-recall path (BM25 + HNSW + RRF +
//! session/recency + window hydration). See spec
//! docs/superpowers/specs/2026-05-01-transcript-recall-design.md.
//!
//! ### FTS predicate-index probe outcome (Task 2, 2026-05-01)
//! `pragma create_fts_index(..., where := '...')` is **NOT SUPPORTED** by
//! the bundled DuckDB version (error: `Parser Error: syntax error at or
//! near "where"` — the `where := '...'` named parameter is not recognized
//! on `pragma create_fts_index`). Task 3's `ensure_transcript_fts_index_fresh`
//! builds a full-table index over `conversation_messages`;
//! `bm25_transcript_candidates` SELECT adds `AND embed_eligible = true`
//! to filter at query time. Re-run `fts_predicate_probe` (`#[ignore]`'d
//! below) on DuckDB upgrades.

#[test]
#[ignore]
fn fts_predicate_probe() {
    // One-shot probe — `cargo test --test transcript_recall fts_predicate_probe -- --ignored --nocapture`.
    // Determines whether the bundled DuckDB FTS extension supports
    // `pragma create_fts_index(... where := '...')` for partial-index
    // creation. Outcome documented in source as a comment block; Task 3's
    // BM25 SQL chooses its branch accordingly.

    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("probe.duckdb");
    let conn = duckdb::Connection::open(&db).unwrap();
    conn.execute_batch("install fts; load fts;").unwrap();
    conn.execute_batch(
        "create table t (id text primary key, content text, eligible boolean);
         insert into t values ('a', 'hello world', true), ('b', 'goodbye world', false);",
    )
    .unwrap();

    let result = conn.execute_batch(
        "pragma create_fts_index('t', 'id', 'content', where := 'eligible = true');",
    );

    match result {
        Ok(_) => {
            // Verify the predicate actually pruned the index — search for a
            // term that appears in BOTH rows; only the eligible row should
            // surface.
            let count: i64 = conn
                .query_row(
                    "select count(*) from t where fts_main_t.match_bm25(id, 'world') is not null",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "only eligible row should be in index");
            println!("FTS predicate index SUPPORTED — Task 3 should use `where := 'embed_eligible = true'`");
        }
        Err(e) => {
            println!("FTS predicate index NOT SUPPORTED: {e}");
            println!("Task 3 should build full-table index and add `AND embed_eligible = true` to BM25 SQL");
            // Don't fail the probe — it's informational. The non-error message
            // is the deliverable.
        }
    }
}

use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::storage::DuckDbRepository;
use tempfile::TempDir;

mod common;

fn sample_block(
    suffix: &str,
    content: &str,
    block_type: BlockType,
    embed: bool,
) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("mb-{suffix}"),
        session_id: Some("S1".to_string()),
        tenant: "local".to_string(),
        caller_agent: "claude-code".to_string(),
        transcript_path: "/tmp/t.jsonl".to_string(),
        line_number: 1,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type,
        content: content.to_string(),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: embed,
        created_at: "00000000020260430000".to_string(),
    }
}

#[tokio::test]
async fn bm25_finds_lexical_match_in_text_block() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    let mut a = sample_block("a", "the user asked about Python", BlockType::Text, true);
    a.line_number = 1;
    let mut b = sample_block(
        "b",
        "we discussed the Rust project layout",
        BlockType::Text,
        true,
    );
    b.line_number = 2;
    let mut c = sample_block("c", "JavaScript notes follow", BlockType::Text, true);
    c.line_number = 3;
    repo.create_conversation_message(&a).await.unwrap();
    repo.create_conversation_message(&b).await.unwrap();
    repo.create_conversation_message(&c).await.unwrap();

    let hits = repo
        .bm25_transcript_candidates("local", "Rust", 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "exactly one block should match 'Rust'");
    assert_eq!(hits[0].message_block_id, "mb-b");
}

#[tokio::test]
async fn bm25_excludes_tool_blocks() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // tool_use block whose JSON content has a unique keyword. embed_eligible=false
    // means it must NOT surface in BM25 (the SELECT filters by embed_eligible=true).
    let mut tool = sample_block(
        "tool",
        r#"{"file_path":"rare-keyword.toml"}"#,
        BlockType::ToolUse,
        false,
    );
    tool.line_number = 1;
    repo.create_conversation_message(&tool).await.unwrap();

    let hits = repo
        .bm25_transcript_candidates("local", "rare-keyword", 5)
        .await
        .unwrap();
    assert!(hits.is_empty(), "tool blocks must not surface in BM25");
}

#[tokio::test]
async fn context_window_returns_neighbors_in_same_session() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // Seed 5 text blocks, increasing line_number AND created_at.
    for i in 0..5 {
        let mut m = sample_block(
            &format!("blk-{i}"),
            &format!("content {i}"),
            BlockType::Text,
            true,
        );
        m.line_number = (i + 1) as u64;
        m.created_at = format!("000000000{:011}", i + 1); // strictly increasing
        repo.create_conversation_message(&m).await.unwrap();
    }

    let win = repo
        .context_window_for_block("local", "mb-blk-2", 1, 1, false)
        .await
        .unwrap();
    assert_eq!(win.before.len(), 1);
    assert_eq!(win.before[0].message_block_id, "mb-blk-1");
    assert_eq!(win.primary.message_block_id, "mb-blk-2");
    assert_eq!(win.after.len(), 1);
    assert_eq!(win.after[0].message_block_id, "mb-blk-3");
}

#[tokio::test]
async fn context_window_excludes_tool_blocks_by_default() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // text, tool_use, text, tool_result, text — primary at index 2 (the middle text).
    let kinds = [
        (BlockType::Text, true),
        (BlockType::ToolUse, false),
        (BlockType::Text, true),
        (BlockType::ToolResult, false),
        (BlockType::Text, true),
    ];
    for (i, (bt, eligible)) in kinds.iter().enumerate() {
        let mut m = sample_block(&format!("k{i}"), &format!("c{i}"), *bt, *eligible);
        m.line_number = (i + 1) as u64;
        m.created_at = format!("000000000{:011}", i + 1);
        repo.create_conversation_message(&m).await.unwrap();
    }

    // include_tool_blocks=false → before/after skip the tool blocks.
    let win = repo
        .context_window_for_block("local", "mb-k2", 2, 2, false)
        .await
        .unwrap();
    let before_ids: Vec<&str> = win
        .before
        .iter()
        .map(|m| m.message_block_id.as_str())
        .collect();
    let after_ids: Vec<&str> = win
        .after
        .iter()
        .map(|m| m.message_block_id.as_str())
        .collect();
    assert_eq!(before_ids, vec!["mb-k0"]); // mb-k1 (tool_use) skipped
    assert_eq!(after_ids, vec!["mb-k4"]); // mb-k3 (tool_result) skipped

    // include_tool_blocks=true → all 4 neighbors returned.
    let win = repo
        .context_window_for_block("local", "mb-k2", 2, 2, true)
        .await
        .unwrap();
    let before_ids: Vec<&str> = win
        .before
        .iter()
        .map(|m| m.message_block_id.as_str())
        .collect();
    let after_ids: Vec<&str> = win
        .after
        .iter()
        .map(|m| m.message_block_id.as_str())
        .collect();
    assert_eq!(before_ids, vec!["mb-k0", "mb-k1"]);
    assert_eq!(after_ids, vec!["mb-k3", "mb-k4"]);
}

#[tokio::test]
async fn context_window_does_not_cross_session_boundary() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    // Two sessions interleaved temporally; window for session A's block must
    // not include session B blocks even though B's block sits between A's.
    let mut a1 = sample_block("a1", "session A first", BlockType::Text, true);
    a1.session_id = Some("A".to_string());
    a1.line_number = 1;
    a1.created_at = "00000000010000000001".to_string();

    let mut b1 = sample_block("b1", "session B first", BlockType::Text, true);
    b1.session_id = Some("B".to_string());
    b1.line_number = 1;
    b1.created_at = "00000000010000000002".to_string();

    let mut a2 = sample_block("a2", "session A second", BlockType::Text, true);
    a2.session_id = Some("A".to_string());
    a2.line_number = 2;
    a2.created_at = "00000000010000000003".to_string();

    repo.create_conversation_message(&a1).await.unwrap();
    repo.create_conversation_message(&b1).await.unwrap();
    repo.create_conversation_message(&a2).await.unwrap();

    let win = repo
        .context_window_for_block("local", "mb-a1", 1, 1, false)
        .await
        .unwrap();
    assert_eq!(win.before.len(), 0);
    assert_eq!(win.after.len(), 1);
    assert_eq!(win.after[0].message_block_id, "mb-a2");
}

#[tokio::test]
async fn context_window_returns_not_found_for_missing_id() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.set_transcript_job_provider("embedanything");

    let err = repo
        .context_window_for_block("local", "mb-does-not-exist", 1, 1, false)
        .await
        .expect_err("should error on missing primary");
    let msg = err.to_string();
    // Variant must NOT include the requested id (avoid leaking through HTTP).
    assert!(
        !msg.contains("mb-does-not-exist"),
        "error message must not leak the requested id: got {msg}"
    );
    // Match shape (loose, in case error message wording shifts):
    assert!(
        msg.to_lowercase().contains("not found"),
        "expected 'not found' in error: got {msg}"
    );
}

// ──────── Integration test scaffolding ────────

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

async fn build_recall_app(db_dir: &TempDir) -> axum::Router {
    use mem::config::Config;
    use mem::service::MemoryService;
    let mut cfg = Config::local();
    cfg.db_path = db_dir.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&cfg.db_path).await.unwrap();
    repo.set_transcript_job_provider("embedanything");
    let memory_service = MemoryService::new(repo.clone());
    let state = common::test_app_state(repo, memory_service);
    mem::http::router().with_state(state)
}

#[allow(clippy::too_many_arguments)]
async fn ingest_via_http(
    app: &axum::Router,
    session: &str,
    line: u64,
    role: &str,
    block_type: &str,
    content: &str,
    embed: bool,
    created: &str,
) {
    let body = json!({
        "session_id": session,
        "tenant": "local",
        "caller_agent": "claude-code",
        "transcript_path": "/tmp/t.jsonl",
        "line_number": line,
        "block_index": 0,
        "role": role,
        "block_type": block_type,
        "content": content,
        "embed_eligible": embed,
        "created_at": created,
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

async fn search_http(app: &axum::Router, body: serde_json::Value) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcripts/search")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ──────── Integration tests ────────

#[tokio::test]
async fn bm25_only_candidate_appears_in_results() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    ingest_via_http(
        &app,
        "S",
        1,
        "assistant",
        "text",
        "rust project layout",
        true,
        "2026-04-30T00:00:00Z",
    )
    .await;
    ingest_via_http(
        &app,
        "S",
        2,
        "assistant",
        "text",
        "unrelated material",
        true,
        "2026-04-30T00:00:01Z",
    )
    .await;

    let v = search_http(
        &app,
        json!({ "query": "rust", "tenant": "local", "limit": 5 }),
    )
    .await;
    let windows = v["windows"].as_array().unwrap();
    assert!(
        !windows.is_empty(),
        "BM25 alone should surface the rust block"
    );
    let primary_block_id = windows[0]["primary_ids"][0].as_str().unwrap();
    assert!(
        !primary_block_id.is_empty(),
        "primary should reference a real id"
    );
    // The matching primary's content should be the rust block.
    let primary_block = windows[0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["is_primary"].as_bool() == Some(true))
        .expect("primary block in window");
    assert_eq!(primary_block["content"], "rust project layout");
}

#[tokio::test]
async fn anchor_session_boost_lifts_matching_session_to_top() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    // Two sessions, each with one weakly relevant block.
    ingest_via_http(
        &app,
        "A",
        1,
        "assistant",
        "text",
        "scattered keyword once",
        true,
        "2026-04-30T00:00:00Z",
    )
    .await;
    ingest_via_http(
        &app,
        "B",
        2,
        "assistant",
        "text",
        "scattered keyword once",
        true,
        "2026-04-30T00:00:01Z",
    )
    .await;

    let v = search_http(
        &app,
        json!({
            "query": "scattered",
            "tenant": "local",
            "limit": 5,
            "anchor_session_id": "A"
        }),
    )
    .await;
    let windows = v["windows"].as_array().unwrap();
    assert!(!windows.is_empty());
    assert_eq!(
        windows[0]["session_id"].as_str(),
        Some("A"),
        "anchor session must rank first"
    );
}

#[tokio::test]
async fn context_window_includes_neighboring_text_blocks() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    // Five text blocks. Only the middle one carries the unique keyword so
    // BM25 has a single primary candidate at position 2 (line 3); ±2 then
    // hydrates into a window of exactly 5 blocks (primary + 2 before + 2 after).
    let contents = [
        "filler-alpha",
        "filler-beta",
        "uniqueneighborkeyword",
        "filler-gamma",
        "filler-delta",
    ];
    for (i, content) in contents.iter().enumerate() {
        ingest_via_http(
            &app,
            "S",
            (i + 1) as u64,
            "assistant",
            "text",
            content,
            true,
            &format!("2026-04-30T00:00:0{i}Z"),
        )
        .await;
    }

    // Search for the middle block's keyword; expect a window of size 5.
    let v = search_http(
        &app,
        json!({
            "query": "uniqueneighborkeyword",
            "tenant": "local",
            "limit": 1,
            "context_window": 2
        }),
    )
    .await;
    let windows = v["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    let blocks = windows[0]["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 5, "primary + 2 before + 2 after = 5");
    let primary_count = blocks
        .iter()
        .filter(|b| b["is_primary"] == json!(true))
        .count();
    assert_eq!(primary_count, 1);
}

#[tokio::test]
async fn context_window_excludes_tool_blocks_by_default_via_http() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    // Only the middle text block carries the search keyword, so BM25 has
    // exactly one candidate. With default tool exclusion, the ±2 window
    // around it should skip past the tool_use / tool_result neighbors.
    let kinds = [
        ("text", true, "filler-alpha"),
        ("tool_use", false, "filler-beta"),
        ("text", true, "uniquedefaultkeyword"),
        ("tool_result", false, "filler-gamma"),
        ("text", true, "filler-delta"),
    ];
    for (i, (bt, eligible, content)) in kinds.iter().enumerate() {
        ingest_via_http(
            &app,
            "S",
            (i + 1) as u64,
            "assistant",
            bt,
            content,
            *eligible,
            &format!("2026-04-30T00:00:0{i}Z"),
        )
        .await;
    }

    // Search hits the middle text block (index 2). Default context_window=2 + tool exclusion.
    let v = search_http(
        &app,
        json!({
            "query": "uniquedefaultkeyword",
            "tenant": "local",
            "limit": 1
        }),
    )
    .await;
    let windows = v["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    let block_types: Vec<&str> = windows[0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["block_type"].as_str().unwrap())
        .collect();
    assert!(
        block_types.iter().all(|bt| *bt == "text"),
        "context excludes tool blocks; got {block_types:?}"
    );
}

#[tokio::test]
async fn context_window_includes_tool_blocks_when_opted_in() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    // Five blocks: only the middle text block carries the search keyword,
    // so BM25 has exactly one candidate and the primary is line 3 (index 2).
    // The others surround it with one tool_use before and one tool_result
    // after so the ±2 window picks up both when tools are opted in.
    let kinds = [
        ("text", true, "filler-alpha"),
        ("tool_use", false, "filler-beta"),
        ("text", true, "uniqueoptinkeyword"),
        ("tool_result", false, "filler-gamma"),
        ("text", true, "filler-delta"),
    ];
    for (i, (bt, eligible, content)) in kinds.iter().enumerate() {
        ingest_via_http(
            &app,
            "S",
            (i + 1) as u64,
            "assistant",
            bt,
            content,
            *eligible,
            &format!("2026-04-30T00:00:0{i}Z"),
        )
        .await;
    }

    let v = search_http(
        &app,
        json!({
            "query": "uniqueoptinkeyword",
            "tenant": "local",
            "limit": 1,
            "context_window": 2,
            "include_tool_blocks_in_context": true
        }),
    )
    .await;
    let windows = v["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    let block_types: std::collections::HashSet<&str> = windows[0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["block_type"].as_str().unwrap())
        .collect();
    assert!(
        block_types.contains("tool_use"),
        "tool_use must appear when opted in; got {block_types:?}"
    );
    assert!(block_types.contains("tool_result"));
}

#[tokio::test]
async fn windows_merge_when_primaries_share_session_and_overlap() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    // Three adjacent text blocks, all with the same query keyword.
    // With context_window=1, primary 1's after overlaps primary 3's before.
    for i in 0..3 {
        ingest_via_http(
            &app,
            "S",
            (i + 1) as u64,
            "assistant",
            "text",
            "shared-merge-keyword",
            true,
            &format!("2026-04-30T00:00:0{i}Z"),
        )
        .await;
    }

    let v = search_http(
        &app,
        json!({
            "query": "shared-merge-keyword",
            "tenant": "local",
            "limit": 5,
            "context_window": 1
        }),
    )
    .await;
    let windows = v["windows"].as_array().unwrap();
    assert_eq!(
        windows.len(),
        1,
        "all three primaries should merge into one window; got {} windows",
        windows.len()
    );
    let primary_ids = windows[0]["primary_ids"].as_array().unwrap();
    assert_eq!(primary_ids.len(), 3);
}

#[tokio::test]
async fn empty_query_returns_recent_time_windows() {
    let dir = TempDir::new().unwrap();
    let app = build_recall_app(&dir).await;

    for i in 0..3 {
        ingest_via_http(
            &app,
            "S",
            (i + 1) as u64,
            "assistant",
            "text",
            &format!("recent-c{i}"),
            true,
            &format!("2026-04-30T00:00:0{i}Z"),
        )
        .await;
    }

    let v = search_http(&app, json!({ "query": "", "tenant": "local", "limit": 5 })).await;
    let windows = v["windows"].as_array().unwrap();
    assert!(
        !windows.is_empty(),
        "empty query should still return windows from recent_*"
    );
}
