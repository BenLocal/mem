//! End-to-end test for the feedback-pass cursor (duplicate re-credit
//! fix, 2026-07-13).
//!
//! `mem feedback-from-transcript` keeps its own cursor under the
//! pseudo-path `<transcript_path>#feedback` in the mine-cursor store, so
//! consecutive Stop-hook passes over one growing transcript credit each
//! consumed capsule ONCE instead of re-POSTing `applies_here` every 15
//! exchanges (measured 2026-07-10: 491 sends over 66 distinct capsules).
//!
//! This spins a real axum server on an ephemeral port because
//! `run_with_counts` speaks reqwest — `tower::oneshot` can't serve it.

use std::io::Write;

use mem::cli::common::RemoteArgs;
use mem::cli::feedback::{self, FeedbackFromTranscriptArgs};
use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
};
use mem::service::CapabilityCapsuleService;
use mem::storage::MineCursorStore;

mod common;

const CAPSULE_ID: &str = "mem_01900000-0000-7000-8000-00000000cafe";

fn sample_capsule() -> CapabilityCapsuleRecord {
    let ts = "2026-07-13T00:00:00.000Z".to_string();
    CapabilityCapsuleRecord {
        capability_capsule_id: CAPSULE_ID.into(),
        tenant: "local".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: "summary".into(),
        content: "DuckDB single-writer MVCC concurrency lock contention".into(),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.5,
        decay_score: 0.0,
        content_hash: format!("{:0>64}", "cafe"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test".into(),
        created_at: ts.clone(),
        updated_at: ts,
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn second_feedback_pass_sends_nothing_and_cursor_persists() {
    let (_dir, store) = common::test_store().await;
    let state = common::test_app_state(store.clone(), CapabilityCapsuleService::new(store.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mem::http::router().with_state(state))
            .await
            .unwrap();
    });
    let base_url = format!("http://{addr}");

    store
        .insert_capability_capsule(sample_capsule())
        .await
        .unwrap();

    // Transcript: recall banner + a consuming assistant block.
    let mut transcript = tempfile::NamedTempFile::new().unwrap();
    let banner = format!(
        "🧠 mem auto-recall — memories relevant to this prompt\n\
         - DuckDB single-writer MVCC concurrency lock contention `[{CAPSULE_ID}]`"
    );
    writeln!(
        transcript,
        "{}",
        serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "hook_additional_context", "content": [banner]},
        })
    )
    .unwrap();
    writeln!(
        transcript,
        "{}",
        serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{
                "type": "text",
                "text": "Right — DuckDB is single-writer, so MVCC concurrency and \
                         lock contention bite when two writers share one file.",
            }]},
        })
    )
    .unwrap();

    let args = |base_url: &str| FeedbackFromTranscriptArgs {
        transcript_path: transcript.path().to_path_buf(),
        remote: RemoteArgs {
            tenant: "local".into(),
            base_url: base_url.into(),
        },
        kind: "applies_here".into(),
        all: false,
    };

    // First pass credits and sends once.
    let first = feedback::run_with_counts(args(&base_url)).await.unwrap();
    assert_eq!(first.sent, 1, "first pass must send the consumed capsule");
    assert_eq!(first.failed, 0);

    // Second pass over the unchanged transcript: evidence is at or before
    // the cursor, so nothing is re-sent.
    let second = feedback::run_with_counts(args(&base_url)).await.unwrap();
    assert_eq!(
        second.sent, 0,
        "second pass must not re-credit already-credited evidence"
    );
    assert_eq!(second.deduped, 1, "the skip must be visible in counts");

    // The cursor persisted under the feedback pseudo-path and covers the
    // whole transcript (2 lines).
    let cursor_key = format!("{}#feedback", transcript.path().display());
    let cursor = store
        .get_mine_cursor(&cursor_key)
        .await
        .unwrap()
        .expect("feedback cursor must persist after a clean pass");
    assert_eq!(cursor.last_line_number, 2);
}
