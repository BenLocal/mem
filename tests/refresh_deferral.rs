//! D2 — deferred-refresh test. Verifies that writes mark the
//! DuckDbQuery dirty but don't refresh eagerly; subsequent reads
//! still observe the write. The contract isn't "no refresh ever" —
//! it's "reads after writes always see the write," same as before.

use std::sync::Arc;

use mem::{
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    storage::{CapsuleSearchStore, Store},
};
use tempfile::tempdir;

fn capsule(id: &str, content: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "local".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary-{id}"),
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        project: Some("d2-test".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: format!("hash-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test".into(),
        created_at: "00000000000000000001".into(),
        updated_at: "00000000000000000001".into(),
        last_validated_at: None,
        last_used_at: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn write_then_read_observes_write_post_deferred_refresh() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("d2.lance")).await.unwrap());

    // Write 3 capsules — none should trigger an eager refresh
    // (commit_lance_write now just marks dirty).
    for i in 0..3 {
        store
            .insert_capability_capsule(capsule(&format!("cap_{i}"), &format!("content {i}")))
            .await
            .unwrap();
    }

    // First read after writes must see all 3 — ensure_fresh kicks in
    // inside the read path.
    let ids = CapsuleSearchStore::list_capability_capsule_ids_for_tenant(store.as_ref(), "local")
        .await
        .unwrap();
    assert_eq!(ids.len(), 3, "read after 3 writes must see all 3: {ids:?}");
    for i in 0..3 {
        let want = format!("cap_{i}");
        assert!(ids.contains(&want), "missing {want}: {ids:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn multiple_writes_then_one_read_still_consistent() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("d2.lance")).await.unwrap());

    // 10 writes back-to-back — pre-D2 each would refresh; post-D2 all
    // coalesce into one refresh on the next read.
    for i in 0..10 {
        store
            .insert_capability_capsule(capsule(&format!("c_{i:02}"), "x"))
            .await
            .unwrap();
    }
    let ids = CapsuleSearchStore::list_capability_capsule_ids_for_tenant(store.as_ref(), "local")
        .await
        .unwrap();
    assert_eq!(ids.len(), 10);
}

#[tokio::test(flavor = "multi_thread")]
async fn read_without_writes_is_fast_path() {
    // No write happens; dirty stays false; ensure_fresh short-circuits
    // without refreshing. Hard to assert "no refresh happened"
    // directly, but the read must still succeed.
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("d2.lance")).await.unwrap());
    let ids = CapsuleSearchStore::list_capability_capsule_ids_for_tenant(store.as_ref(), "local")
        .await
        .unwrap();
    assert!(ids.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn alternating_write_read_still_consistent() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("d2.lance")).await.unwrap());

    for i in 0..5 {
        store
            .insert_capability_capsule(capsule(&format!("alt_{i}"), &format!("c {i}")))
            .await
            .unwrap();
        // Read after every write — each read should see the previous
        // write (ensure_fresh fires once per read).
        let ids =
            CapsuleSearchStore::list_capability_capsule_ids_for_tenant(store.as_ref(), "local")
                .await
                .unwrap();
        assert_eq!(ids.len(), i + 1, "round {i}: got {ids:?}");
    }
}
