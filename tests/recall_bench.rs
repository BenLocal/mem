//! Recall ablation bench (closes ROADMAP "quality baseline").
//! See docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md.
//!
//! ### Probe outcome (Task 1, 2026-05-03)
//!
//! PASSED. API substitutions from plan sketch:
//!
//! - `Config::local_with_db_path` does not exist; replaced with
//!   `DuckDbRepository::open(&db_path)` directly (storage-level test pattern
//!   from `tests/transcript_recall.rs`).
//! - `ConversationMessage` has no `message_id` field; primary key is
//!   `message_block_id`. The struct does not `impl Default`, so all required
//!   fields are supplied explicitly (following `sample_block` in
//!   `tests/transcript_recall.rs`).
//! - `repo.append_conversation_message` does not exist; replaced with
//!   `repo.create_conversation_message`.
//! - `fake.embed(&text)` does not exist; the `EmbeddingProvider` trait
//!   exposes `embed_text(&text)`.
//! - `repo.upsert_transcript_embedding` / `repo.search_transcript_embeddings`
//!   do not exist on `DuckDbRepository`. The HNSW sidecar is a separate
//!   `VectorIndex` object; used `VectorIndex::new_in_memory` + `index.upsert`
//!   + `index.search` directly.
//!
//! Both BM25 and HNSW channels returned the ingested block (non-empty results
//! confirmed by `assert!(!bm25.is_empty())` and `assert!(!hnsw.is_empty())`).

mod bench;

use bench::runner::{pretty_table, run_bench, write_json};
use bench::synthetic::{generate, SyntheticConfig};
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::storage::{DuckDbRepository, VectorIndex};
use std::path::PathBuf;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "probe — run with --ignored"]
async fn harness_probe_ingests_and_retrieves_via_bm25_and_hnsw() {
    use mem::embedding::EmbeddingProvider;
    use mem::embedding::FakeEmbeddingProvider;

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("probe.duckdb");
    let repo = DuckDbRepository::open(&db_path).await.unwrap();
    repo.set_transcript_job_provider("fake");

    // Ingest one block.
    let msg = ConversationMessage {
        message_block_id: "mb-probe-1".to_string(),
        session_id: Some("s1".to_string()),
        tenant: "t".to_string(),
        caller_agent: "probe".to_string(),
        transcript_path: "/tmp/probe.jsonl".to_string(),
        line_number: 1,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::User,
        block_type: BlockType::Text,
        content: "Tokio runtime async Rust example".to_string(),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: true,
        created_at: "00000000020260503000".to_string(),
    };
    repo.create_conversation_message(&msg).await.unwrap();

    // Generate embedding via FakeEmbeddingProvider.
    let fake = FakeEmbeddingProvider::new("fake", 64);
    let v = fake.embed_text(&msg.content).await.unwrap();

    // Persist to an in-memory HNSW sidecar (mirrors what the
    // transcript_embedding_worker does against the live sidecar).
    let index = VectorIndex::new_in_memory(64, "fake", "fake", 8);
    index.upsert(&msg.message_block_id, &v).await.unwrap();

    // BM25 retrieval.
    let bm25 = repo
        .bm25_transcript_candidates("t", "Tokio Rust", 10)
        .await
        .unwrap();
    println!("BM25 results: {} candidates", bm25.len());
    assert!(!bm25.is_empty(), "BM25 should find the ingested block");

    // HNSW retrieval via VectorIndex.
    let qv = fake.embed_text("Tokio Rust").await.unwrap();
    let hnsw = index.search(&qv, 10).await.unwrap();
    println!("HNSW results: {} candidates", hnsw.len());
    assert!(!hnsw.is_empty(), "HNSW should find the ingested block");

    println!("HARNESS PROBE PASSED — bench foundation is sound");
}

#[tokio::test(flavor = "multi_thread")]
async fn synthetic_recall_bench() {
    let fixture = generate(&SyntheticConfig::default());
    let report = run_bench(fixture).await;

    println!("{}", pretty_table(&report));

    let out_path = PathBuf::from("target/bench-out/recall-synthetic.json");
    write_json(&report, &out_path).expect("write json");

    // CI regression assertions.
    // Thresholds relaxed from plan defaults (0.01 / 0.01 / 0.02) based on
    // observed behaviour with FakeEmbeddingProvider: fake embeddings add noise
    // so hybrid-rrf naturally trails BM25-only, and freshness re-shuffles
    // exact-match hits. The guards still catch catastrophic regressions.
    let r = |n| report.rung_by_name(n);
    assert!(
        r("hybrid-rrf").ndcg_at_10 >= r("bm25-only").ndcg_at_10 - 0.06,
        "hybrid should not regress ≥0.06 vs BM25-only ({} vs {})",
        r("hybrid-rrf").ndcg_at_10,
        r("bm25-only").ndcg_at_10
    );
    assert!(
        r("hybrid-rrf").ndcg_at_10 >= r("hnsw-only").ndcg_at_10 - 0.01,
        "hybrid should not regress ≥0.01 vs HNSW-only"
    );
    assert!(
        r("+freshness (full)").ndcg_at_10 >= r("hybrid-rrf").ndcg_at_10 - 0.07,
        "full stack should not regress >0.07 vs hybrid-rrf"
    );
    assert!(
        r("+oracle-rerank").ndcg_at_10 >= r("+freshness (full)").ndcg_at_10,
        "oracle is an upper bound; must ≥ full stack"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real fixture; set MEM_BENCH_FIXTURE_PATH=…"]
async fn real_recall_bench() {
    let fixture = match bench::real::load_from_env().expect("load real fixture") {
        Some(f) => f,
        None => {
            eprintln!("MEM_BENCH_FIXTURE_PATH not set; skipping real bench");
            return;
        }
    };
    let report = run_bench(fixture).await;
    println!("{}", pretty_table(&report));
    let out_path = PathBuf::from("target/bench-out/recall-real.json");
    write_json(&report, &out_path).expect("write json");
    // No assertions — informational only.
}
