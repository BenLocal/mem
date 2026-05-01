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
