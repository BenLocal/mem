use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
};
use mem::domain::episode::EpisodeRecord;
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::embedding::FakeEmbeddingProvider;
use mem::storage::{SessionStore, Store};
use tempfile::TempDir;

async fn temp_lance() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeEmbeddingProvider::new("fake", 64));
    let store = Store::open_with_provider(dir.path().join("store"), provider)
        .await
        .unwrap();
    (dir, store)
}

fn sample_capsule(id: &str, tenant: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: tenant.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Global,
        visibility: Visibility::Private,
        version: 1,
        summary: format!("summary-{id}"),
        content: format!("content-{id}"),
        content_hash: format!("hash-{id}"),
        confidence: 0.5,
        decay_score: 0.0,
        source_agent: "test".into(),
        created_at: "00000000000000000000".into(),
        updated_at: "00000000000000000000".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn syncs_capsules_lance_to_lance() {
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;

    src.insert_capability_capsules(&[sample_capsule("c1", "local"), sample_capsule("c2", "local")])
        .await
        .unwrap();

    let report = mem::cli::sync::copy_capsules_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 2);

    let ids = dst
        .list_capability_capsule_ids_for_tenant("local")
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"c1".to_string()));

    // Verbatim-content round-trip: fetching c1 from dst preserves content_hash.
    let rows = mem::storage::CapsuleStore::fetch_capability_capsules_by_ids(&dst, "local", &["c1"])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content_hash, "hash-c1");

    // Re-run is idempotent.
    let again = mem::cli::sync::copy_capsules_for_test(&src, &dst, "local", 100).await;
    assert_eq!(again.copied, 0);
    assert_eq!(again.skipped, 2);
}

fn conversation_message_fixture(session_id: &str, tenant: &str) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("mb-{session_id}-1"),
        session_id: Some(session_id.to_string()),
        tenant: tenant.to_string(),
        caller_agent: "claude-code".to_string(),
        transcript_path: format!("/tmp/transcript-{session_id}.jsonl"),
        line_number: 1,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type: BlockType::Text,
        content: format!("hello from {session_id}"),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: false,
        created_at: "00000001778000000000".to_string(),
        meta_json: None,
    }
}

#[tokio::test]
async fn syncs_transcripts_lance_to_lance() {
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;
    // Build one ConversationMessage via the tests/clickhouse_backend.rs literal pattern.
    let msg = conversation_message_fixture("sess1", "local");
    src.create_conversation_messages(&[msg]).await.unwrap();

    let report = mem::cli::sync::copy_transcripts_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 1);
    let got = dst
        .get_conversation_messages_by_session("local", "sess1")
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
}

fn episode_fixture(id: &str, tenant: &str) -> EpisodeRecord {
    EpisodeRecord {
        episode_id: id.to_string(),
        tenant: tenant.to_string(),
        goal: format!("goal-{id}"),
        steps: vec![format!("step-{id}")],
        outcome: "success".to_string(),
        evidence: vec![],
        scope: Scope::Workspace,
        visibility: Visibility::Private,
        project: None,
        repo: None,
        module: None,
        tags: vec![],
        source_agent: "test".to_string(),
        idempotency_key: None,
        created_at: "00000000000000000000".to_string(),
        updated_at: "00000000000000000000".to_string(),
        workflow_candidate: None,
    }
}

#[tokio::test]
async fn syncs_episodes_lance_to_lance() {
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;
    src.insert_episode(episode_fixture("e1", "local"))
        .await
        .unwrap();

    let report = mem::cli::sync::copy_episodes_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 1);
    assert_eq!(
        dst.list_successful_episodes_for_tenant("local")
            .await
            .unwrap()
            .len(),
        1
    );

    // Idempotent re-run.
    let again = mem::cli::sync::copy_episodes_for_test(&src, &dst, "local", 100).await;
    assert_eq!(again.copied, 0);
    assert_eq!(again.skipped, 1);
}
