use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, GraphEdge, Scope,
    Visibility,
};
use mem::domain::entity::EntityKind;
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

#[tokio::test]
async fn syncs_entities_lance_to_lance() {
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;

    src.resolve_or_create(
        "local",
        "InvoiceService",
        EntityKind::Module,
        "20260625T000000000",
    )
    .await
    .unwrap();

    let report = mem::cli::sync::copy_entities_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 1);
    let got = dst.list_entities("local", None, None, 100).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].canonical_name, "InvoiceService");

    // Idempotent re-run (skip by canonical_name).
    let again = mem::cli::sync::copy_entities_for_test(&src, &dst, "local", 100).await;
    assert_eq!(again.copied, 0);
    assert_eq!(again.skipped, 1);
}

#[tokio::test]
async fn syncs_active_edges_lance_to_lance() {
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;
    // A capsule must exist so the walk enumerates its node id "mem:c1".
    src.insert_capability_capsules(&[sample_capsule("c1", "local")])
        .await
        .unwrap();

    // Seed one active edge rooted at the capsule node.
    let edge = GraphEdge {
        from_node_id: "mem:c1".into(),
        to_node_id: "entity:abc".into(),
        relation: "mentions".into(),
        valid_from: "20260625T000000000000".into(),
        valid_to: None,
        confidence: Some(1.0),
        extractor: Some("test".into()),
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    };
    src.sync_memory_edges(&[edge], "20260625T000000000000")
        .await
        .unwrap();

    let report = mem::cli::sync::copy_edges_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 1);
    assert_eq!(dst.neighbors("mem:c1").await.unwrap().len(), 1);

    // Idempotent re-run: the active edge already exists on the target.
    let again = mem::cli::sync::copy_edges_for_test(&src, &dst, "local", 100).await;
    assert_eq!(again.copied, 0);
    assert_eq!(again.skipped, 1);
}

#[tokio::test]
async fn full_roundtrip_lance_to_lance() {
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;
    src.insert_capability_capsules(&[sample_capsule("c1", "local")])
        .await
        .unwrap();

    let caps = mem::cli::sync::copy_capsules_for_test(&src, &dst, "local", 100).await;
    let ents = mem::cli::sync::copy_entities_for_test(&src, &dst, "local", 100).await;
    assert_eq!(caps.copied, 1);
    assert_eq!(caps.failed + ents.failed, 0);
}

#[tokio::test]
async fn syncs_capsules_lance_to_clickhouse() {
    let Ok(url) = std::env::var("MEM_TEST_CLICKHOUSE_URL") else {
        eprintln!("MEM_TEST_CLICKHOUSE_URL unset — skipping lance→clickhouse");
        return;
    };
    let (_sd, src) = temp_lance().await;
    src.insert_capability_capsules(&[sample_capsule("c1", "local")])
        .await
        .unwrap();
    let ch = mem::storage::ClickHouseBackend::connect(&url)
        .await
        .unwrap();
    ch.apply_migrations().await.unwrap();
    let report = mem::cli::sync::copy_capsules_for_test(
        &src,
        &ch as &dyn mem::storage::Backend,
        "local",
        100,
    )
    .await;
    assert_eq!(report.failed, 0);
    assert!(report.copied >= 1);
}

#[tokio::test]
async fn syncs_capsules_lance_to_postgres() {
    let Ok(url) = std::env::var("MEM_TEST_POSTGRES_URL") else {
        eprintln!("MEM_TEST_POSTGRES_URL unset — skipping lance→postgres");
        return;
    };
    let (_sd, src) = temp_lance().await;
    src.insert_capability_capsules(&[sample_capsule("c1", "local")])
        .await
        .unwrap();
    let pg = mem::storage::PostgresCapsuleStore::connect(&url)
        .await
        .unwrap();
    let report = mem::cli::sync::copy_capsules_for_test(
        &src,
        &pg as &dyn mem::storage::Backend,
        "local",
        100,
    )
    .await;
    assert_eq!(report.failed, 0);
    assert!(report.copied >= 1);
}

/// Orchestration layer (`run_domains`): `--domains` subset selection only runs
/// the chosen domains, the grand tally sums across them, and `--dry-run`
/// counts without writing. Drives two fake-provider temp Lance stores directly,
/// bypassing `Config::from_env` / backend opening.
#[tokio::test]
async fn orchestration_honors_domain_subset_and_dry_run() {
    let (_sd, src) = temp_lance().await;
    src.insert_capability_capsules(&[sample_capsule("c1", "local")])
        .await
        .unwrap();
    src.resolve_or_create("local", "Svc", EntityKind::Module, "20260625T000000000")
        .await
        .unwrap();

    // Subset = only Capsules → the Entities domain must NOT run.
    let (_td, dst) = temp_lance().await;
    let grand = mem::cli::sync::run_domains(
        &src,
        &dst,
        &["local".to_string()],
        &[mem::cli::sync::Domain::Capsules],
        100,
        false, // dry_run
        false, // verbose
        "fake",
    )
    .await;
    assert_eq!(grand.copied, 1, "only the capsule should be copied");
    assert_eq!(grand.failed, 0);
    assert_eq!(
        dst.list_capability_capsule_ids_for_tenant("local")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        dst.list_entities("local", None, None, 100)
            .await
            .unwrap()
            .len(),
        0,
        "entities domain was excluded by the subset"
    );

    // Dry-run: tallies what it would copy but writes nothing.
    let (_td2, dst2) = temp_lance().await;
    let dry = mem::cli::sync::run_domains(
        &src,
        &dst2,
        &["local".to_string()],
        &[mem::cli::sync::Domain::Capsules],
        100,
        true, // dry_run
        false,
        "fake",
    )
    .await;
    assert_eq!(dry.copied, 1);
    assert_eq!(
        dst2.list_capability_capsule_ids_for_tenant("local")
            .await
            .unwrap()
            .len(),
        0,
        "dry-run must not write to the target"
    );
}
