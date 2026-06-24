use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
};
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::service::CapabilityCapsuleService;
use mem::storage::Store;
use tempfile::tempdir;

#[tokio::test]
async fn fetch_by_ids_filters_tenant_and_status() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("fetch.lance");
    let store = Arc::new(Store::open(&db).await.unwrap());
    let svc = CapabilityCapsuleService::new(store.clone());

    let make = |tenant: &str, content: &str| IngestCapabilityCapsuleRequest {
        tenant: tenant.into(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        content: content.into(),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: Some("mem".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
        supersedes_capability_capsule_id: None,
        expires_at: None,
    };
    let a = svc.ingest(make("ten-a", "alpha")).await.unwrap();
    let b = svc.ingest(make("ten-b", "beta")).await.unwrap();
    let c = svc.ingest(make("ten-a", "gamma")).await.unwrap();

    let ids = vec![
        a.capability_capsule_id.as_str(),
        b.capability_capsule_id.as_str(),
        c.capability_capsule_id.as_str(),
    ];
    let rows = store
        .fetch_capability_capsules_by_ids("ten-a", &ids)
        .await
        .unwrap();

    let returned: std::collections::HashSet<_> = rows
        .iter()
        .map(|m| m.capability_capsule_id.as_str())
        .collect();
    assert!(returned.contains(a.capability_capsule_id.as_str()));
    assert!(returned.contains(c.capability_capsule_id.as_str()));
    assert!(!returned.contains(b.capability_capsule_id.as_str())); // wrong tenant
}

#[allow(clippy::too_many_arguments)]
fn tmsg(id: &str, tenant: &str, line: u64, content: &str, stamp: &str) -> ConversationMessage {
    ConversationMessage {
        message_block_id: id.into(),
        session_id: Some("S1".into()),
        tenant: tenant.into(),
        caller_agent: "claude-code".into(),
        transcript_path: "/tmp/fetch_by_ids.jsonl".into(),
        line_number: line,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type: BlockType::Text,
        content: content.into(),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: true,
        created_at: stamp.into(),
        meta_json: None,
    }
}

/// Both fetch-by-ids buckets must work on the lance-native read path —
/// the compose hydration path is pure lance. Seeds a couple of capsules +
/// transcript messages and asserts both fetch-by-ids methods return the
/// right records (correct set + order + tenant isolation).
#[tokio::test]
async fn fetch_by_ids_lance_engine_capsules_and_transcripts() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("fetch_lance.lance");
    let store = Arc::new(Store::open(&db).await.unwrap());
    store.set_transcript_job_provider("fake-test");
    let svc = CapabilityCapsuleService::new(store.clone());

    let make = |tenant: &str, content: &str| IngestCapabilityCapsuleRequest {
        tenant: tenant.into(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        content: content.into(),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: Some("mem".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
        supersedes_capability_capsule_id: None,
        expires_at: None,
    };
    let a = svc.ingest(make("ten-a", "alpha capsule")).await.unwrap();
    let b = svc.ingest(make("ten-b", "beta capsule")).await.unwrap();
    let c = svc.ingest(make("ten-a", "gamma capsule")).await.unwrap();

    // ── Capsule fetch-by-ids on the Lance engine ──
    let ids = vec![
        a.capability_capsule_id.as_str(),
        b.capability_capsule_id.as_str(),
        c.capability_capsule_id.as_str(),
    ];
    let rows = store
        .fetch_capability_capsules_by_ids("ten-a", &ids)
        .await
        .unwrap();
    let returned: std::collections::HashSet<_> = rows
        .iter()
        .map(|m| m.capability_capsule_id.as_str())
        .collect();
    assert_eq!(
        returned.len(),
        2,
        "exactly the two ten-a capsules: {rows:?}"
    );
    assert!(returned.contains(a.capability_capsule_id.as_str()));
    assert!(returned.contains(c.capability_capsule_id.as_str()));
    assert!(
        !returned.contains(b.capability_capsule_id.as_str()),
        "ten-b capsule must not leak into ten-a fetch"
    );

    // Empty ids short-circuits to empty.
    let empty = store
        .fetch_capability_capsules_by_ids("ten-a", &[])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // ── Transcript fetch-by-ids on the Lance engine ──
    let m1 = tmsg("blk_1", "ten-a", 1, "first block", "00000000000000000010");
    let m2 = tmsg("blk_2", "ten-a", 2, "second block", "00000000000000000020");
    let m3 = tmsg("blk_3", "ten-a", 3, "third block", "00000000000000000030");
    let m_other = tmsg("blk_x", "ten-b", 1, "other tenant", "00000000000000000040");
    for m in [&m1, &m2, &m3, &m_other] {
        store.create_conversation_message(m).await.unwrap();
    }

    // Input order preserved (blk_3, blk_1), wrong-tenant + missing dropped.
    let by_ids = store
        .fetch_conversation_messages_by_ids(
            "ten-a",
            &[
                "blk_3".to_string(),
                "blk_1".to_string(),
                "blk_x".to_string(), // wrong tenant → dropped
                "missing".to_string(),
            ],
        )
        .await
        .unwrap();
    let got_ids: Vec<&str> = by_ids.iter().map(|m| m.message_block_id.as_str()).collect();
    assert_eq!(
        got_ids,
        vec!["blk_3", "blk_1"],
        "input-id order preserved; wrong-tenant + missing dropped"
    );

    // Empty ids short-circuits to empty.
    let t_empty = store
        .fetch_conversation_messages_by_ids("ten-a", &[])
        .await
        .unwrap();
    assert!(t_empty.is_empty());
}
