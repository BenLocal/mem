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
