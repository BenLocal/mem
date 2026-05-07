use std::fs;
use std::sync::Arc;

use tempfile::{NamedTempFile, TempDir};

mod common;

#[tokio::test]
async fn test_mine_and_wake_up_flow() {
    // Use a tempdir + Fake provider instead of `Config::from_env()`, which
    // resolves to `$MEM_DB_PATH` or `~/.mem/mem.duckdb` and conflicts with a
    // running `mem serve` holding the DuckDB file lock. Also avoids the
    // `EmbedAnything` (Qwen3-1024) model load that `development_defaults`
    // wires in by default.
    let tmp = TempDir::new().unwrap();
    let mut embedding = mem::config::EmbeddingSettings::development_defaults();
    embedding.provider = mem::config::EmbeddingProviderKind::Fake;
    embedding.model = "fake".to_string();
    embedding.dim = 8;
    embedding.transcript_disabled = true;
    // Default 1000 ms poll interval makes the post-mine `sleep(200ms)` race
    // the first tick. Tighten so the worker drains the queue inside the
    // test's wait window.
    embedding.worker_poll_interval_ms = 50;
    let config = mem::config::Config {
        bind_addr: "127.0.0.1:0".to_string(),
        db_path: tmp.path().join("mem.duckdb"),
        embedding,
    };
    let app = mem::app::router_with_config(config.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let base_url = format!("http://{}", addr);

    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>Integration test memory</mem-save>"}]},"sessionId":"test","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let mine_args = mem::cli::mine::MineArgs {
        transcript_path: file.path().to_path_buf(),
        tenant: "local".to_string(),
        agent: "claude-code".to_string(),
        base_url: base_url.clone(),
    };

    let exit_code = mem::cli::mine::run(mine_args).await;
    assert_eq!(exit_code, 0);

    // Give embedding worker time to drain the queued job. With
    // `worker_poll_interval_ms = 50` and the Fake provider, several ticks
    // run inside this window. Increase if the test starts flaking.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let wake_args = mem::cli::wake_up::WakeUpArgs {
        tenant: "local".to_string(),
        token_budget: 800,
        base_url: base_url.clone(),
    };

    let output = mem::cli::wake_up::run(wake_args).await.unwrap();
    assert!(output.contains("Integration test memory"));
}

// ---------------------------------------------------------------------------
// Task 12 — End-to-end smoke for the transcript-archive pipeline.
//
// Exercises the full path from `mem mine` (CLI) through to
// `/transcripts/search` and `GET /transcripts?session_id=…` over a real axum
// HTTP server bound to a random port.
//
// Worker-handling strategy: **Option B** (manual `transcript_embedding_worker::tick`).
// We build `AppState` directly with a `FakeEmbeddingProvider` instead of going
// through `app::router_with_config`, which would spawn workers AND load the
// EmbedAnything model (slow + flaky in CI). The full data path —
// ingest → DB row → embedding job → worker tick → embedding row + HNSW
// upsert → search query embed → HNSW lookup → fetch-by-ids → response — runs
// end-to-end. The "worker spawns and drains in the background" mechanism is
// already covered by `tests/transcript_embedding_worker.rs`.
//
// Manual smoke checklist (per spec §Verification Checklist; do NOT run as
// part of this test, kept here for the operator's pre-merge ritual):
//
//   1. cargo run -- serve
//   2. mem mine --transcript-path ~/.claude/projects/<proj>/<session>.jsonl
//   3. Inspect rows:
//        duckdb $MEM_DB_PATH "SELECT count(*) FROM conversation_messages"
//        duckdb $MEM_DB_PATH "SELECT count(*) FROM transcript_embedding_jobs WHERE status='completed'"
//   4. curl -s "http://127.0.0.1:3000/transcripts?session_id=<sid>&tenant=local" | jq
//   5. curl -s -X POST http://127.0.0.1:3000/transcripts/search \
//        -H 'content-type: application/json' \
//        -d '{"query":"<some text from the session>","tenant":"local","limit":5}' | jq
// ---------------------------------------------------------------------------

const TRANSCRIPT_FIXTURE: &str = r##"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"please read README in this Rust project"}]},"sessionId":"S1","timestamp":"2026-04-30T00:00:01Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>archive-pipeline-fact</mem-save>"},{"type":"tool_use","id":"tu-1","name":"Read","input":{"path":"README.md"}}]},"sessionId":"S1","timestamp":"2026-04-30T00:00:02Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu-1","content":"# README contents"}]},"sessionId":"S1","timestamp":"2026-04-30T00:00:03Z"}
"##;

fn write_transcript_fixture(file: &NamedTempFile) {
    fs::write(file.path(), TRANSCRIPT_FIXTURE).unwrap();
}

#[tokio::test]
async fn end_to_end_mine_then_search_then_get() {
    use mem::config::{EmbeddingProviderKind, EmbeddingSettings};
    use mem::embedding::{EmbeddingProvider, FakeEmbeddingProvider};
    use mem::service::{EntityService, MemoryService, TranscriptService};
    use mem::storage::{DuckDbRepository, VectorIndex, VectorIndexFingerprint};
    use mem::worker::transcript_embedding_worker;

    // --- Construct AppState manually (no worker spawn, no model load).
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("mem.duckdb");

    let repo = DuckDbRepository::open(&db_path).await.unwrap();
    // Match the `provider` recorded on each enqueued transcript embedding job
    // so the worker tick's `job.provider != settings.job_provider_id()` sanity
    // check passes (we configure `EmbeddingSettings.provider = Fake` below,
    // which yields `job_provider_id() == "fake"`).
    repo.set_transcript_job_provider("fake");

    let dim = 8;
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbeddingProvider::new("fake", dim));
    let fp = VectorIndexFingerprint {
        provider: "fake".to_string(),
        model: provider.model().to_string(),
        dim,
    };
    let transcript_index = Arc::new(
        VectorIndex::open_or_rebuild_transcripts(&repo, &db_path, &fp)
            .await
            .unwrap(),
    );

    let transcript_service = TranscriptService::new(
        repo.clone(),
        transcript_index.clone(),
        Some(provider.clone()),
    );
    let state = mem::app::AppState {
        memory_service: MemoryService::new(repo.clone()),
        config: mem::config::Config::local(),
        transcript_index: transcript_index.clone(),
        transcript_service,
        entity_service: EntityService::new(repo.clone()),
    };
    let app = mem::http::router().with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // --- Step 1: run `mem mine` against the fixture.
    let transcript_file = NamedTempFile::new().unwrap();
    write_transcript_fixture(&transcript_file);
    let exit_code = mem::cli::mine::run(mem::cli::mine::MineArgs {
        transcript_path: transcript_file.path().to_path_buf(),
        tenant: "local".to_string(),
        agent: "claude-code".to_string(),
        base_url: format!("http://{}", addr),
    })
    .await;
    assert_eq!(exit_code, 0, "mine should succeed against the fixture");

    // --- Step 2: drain the transcript embedding queue manually. The fixture
    //     has 2 embed-eligible blocks (the two `text` ones); each tick claims
    //     a single job, so we tick until the queue is empty. Bound the loop
    //     to a small constant — runaway means a bug in the worker.
    let mut settings = EmbeddingSettings::development_defaults();
    settings.provider = EmbeddingProviderKind::Fake;
    settings.model = provider.model().to_string();
    settings.dim = dim;
    settings.max_retries = 2;
    settings.vector_index_flush_every = 1;
    for _ in 0..10 {
        transcript_embedding_worker::tick(&repo, provider.as_ref(), &settings, &transcript_index)
            .await
            .unwrap();
        let conn = duckdb::Connection::open(&db_path).unwrap();
        let pending: i64 = conn
            .query_row(
                "SELECT count(*) FROM transcript_embedding_jobs \
                 WHERE status IN ('pending','processing')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if pending == 0 {
            break;
        }
    }

    // --- Step 3: GET by session_id returns time-ordered blocks (>=4 = the
    //     four blocks in the fixture: 1 user/text + 1 assistant/text +
    //     1 assistant/tool_use + 1 user/tool_result).
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "http://{}/transcripts?session_id=S1&tenant=local",
            addr
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "GET /transcripts must 2xx");
    let v: serde_json::Value = resp.json().await.unwrap();
    let messages = v["messages"].as_array().expect("messages array");
    assert!(
        messages.len() >= 4,
        "expected >= 4 blocks, got {}",
        messages.len()
    );
    // Time-ordered: created_at strings are ISO-8601 so lexicographic order
    // matches chronological order.
    let timestamps: Vec<&str> = messages
        .iter()
        .map(|m| m["created_at"].as_str().unwrap())
        .collect();
    assert!(
        timestamps.windows(2).all(|w| w[0] <= w[1]),
        "messages must be time-ordered: {timestamps:?}"
    );

    // --- Step 4: POST search with a real query — at least one hit.
    //     The fake embedder gives deterministic per-input vectors, and the
    //     transcript HNSW sidecar holds the two embed-eligible text blocks
    //     ("please read README in this Rust project" and the <mem-save>
    //     line); ANN search returns nearest neighbours regardless of query
    //     similarity, so any non-empty query against a non-empty index
    //     produces ≥1 hit.
    let body = serde_json::json!({
        "query": "Rust project",
        "tenant": "local",
        "limit": 5,
    });
    let resp = client
        .post(format!("http://{}/transcripts/search", addr))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "POST /transcripts/search must 2xx"
    );
    let v: serde_json::Value = resp.json().await.unwrap();
    let windows = v["windows"].as_array().expect("windows array");
    assert!(!windows.is_empty(), "expected at least one window");
    let primaries = windows[0]["primary_ids"].as_array().unwrap();
    assert!(
        !primaries.is_empty(),
        "top window must have at least one primary"
    );
}
