//! Phase 2 validation: same capsule CRUD / lifecycle scenarios run
//! against both backends (`Store` over Lance and
//! `InMemoryCapsuleStore`). If any case behaves differently across
//! the two, the trait surface defined in `src/storage/capsule_store.rs`
//! has an under-specified contract — the failing case becomes the
//! validation signal that pushes us back to revisit §3.3 of
//! `docs/backend-coupling.md` before Phase 3.
//!
//! Test fanout works by parameterising each scenario over a
//! `Arc<dyn CapsuleStore>`. A small macro emits one `#[tokio::test]`
//! per `(scenario, backend)` pair so the test names show up
//! independently in `cargo test` output and either backend can fail
//! in isolation.

use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, FeedbackKind, Scope,
    Visibility,
};
use mem::storage::{current_timestamp, CapsuleStore, FeedbackEvent, InMemoryCapsuleStore, Store};
use tempfile::TempDir;

fn fixture(id: &str, status: CapabilityCapsuleStatus) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "t".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status,
        scope: Scope::Repo,
        visibility: Visibility::Private,
        version: 1,
        summary: format!("summary-{id}"),
        content: format!("content-{id}"),
        content_hash: format!("{:0>64}", id),
        confidence: 0.5,
        decay_score: 0.0,
        source_agent: "test".into(),
        created_at: "00000000000000000000".into(),
        updated_at: "00000000000000000000".into(),
        ..Default::default()
    }
}

async fn open_lance_backend() -> (TempDir, Arc<dyn CapsuleStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("store")).await.unwrap();
    let backend: Arc<dyn CapsuleStore> = Arc::new(store);
    (dir, backend)
}

fn open_in_memory_backend() -> Arc<dyn CapsuleStore> {
    Arc::new(InMemoryCapsuleStore::new())
}

// ── Scenarios ────────────────────────────────────────────────────────

async fn insert_and_get_round_trip(backend: Arc<dyn CapsuleStore>) {
    let row = fixture("a", CapabilityCapsuleStatus::Active);
    backend
        .insert_capability_capsule(row.clone())
        .await
        .unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("t", "a")
        .await
        .unwrap();
    assert!(got.is_some(), "tenant-scoped get should find the row");
    let got = got.unwrap();
    assert_eq!(got.capability_capsule_id, "a");
    assert_eq!(got.tenant, "t");
    assert_eq!(got.status, CapabilityCapsuleStatus::Active);
}

async fn get_for_other_tenant_returns_none(backend: Arc<dyn CapsuleStore>) {
    let row = fixture("a", CapabilityCapsuleStatus::Active);
    backend.insert_capability_capsule(row).await.unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("other-tenant", "a")
        .await
        .unwrap();
    assert!(got.is_none(), "wrong tenant must not see the row");
}

async fn accept_pending_transitions_status(backend: Arc<dyn CapsuleStore>) {
    backend
        .insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::PendingConfirmation))
        .await
        .unwrap();
    let updated = backend.accept_pending("t", "a").await.unwrap();
    assert_eq!(updated.status, CapabilityCapsuleStatus::Active);
    let refreshed = backend
        .get_capability_capsule_for_tenant("t", "a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.status, CapabilityCapsuleStatus::Active);
}

async fn reject_pending_transitions_status(backend: Arc<dyn CapsuleStore>) {
    backend
        .insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::PendingConfirmation))
        .await
        .unwrap();
    let updated = backend.reject_pending("t", "a").await.unwrap();
    assert_eq!(updated.status, CapabilityCapsuleStatus::Rejected);
}

async fn list_pending_review_filters_status(backend: Arc<dyn CapsuleStore>) {
    backend
        .insert_capability_capsule(fixture("p1", CapabilityCapsuleStatus::PendingConfirmation))
        .await
        .unwrap();
    backend
        .insert_capability_capsule(fixture("p2", CapabilityCapsuleStatus::PendingConfirmation))
        .await
        .unwrap();
    backend
        .insert_capability_capsule(fixture("a1", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    let pending = backend.list_pending_review("t").await.unwrap();
    assert_eq!(pending.len(), 2, "should see only the 2 pending rows");
    let mut ids: Vec<&str> = pending
        .iter()
        .map(|r| r.capability_capsule_id.as_str())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["p1", "p2"]);
}

async fn find_by_idempotency_dedups_on_key(backend: Arc<dyn CapsuleStore>) {
    let mut row = fixture("a", CapabilityCapsuleStatus::Active);
    row.idempotency_key = Some("idem-1".into());
    backend.insert_capability_capsule(row).await.unwrap();
    let dup = backend
        .find_by_idempotency_or_hash("t", &Some("idem-1".to_string()), "no-match-hash")
        .await
        .unwrap();
    assert!(dup.is_some(), "idempotency key should match");
    assert_eq!(dup.unwrap().capability_capsule_id, "a");
}

async fn find_by_idempotency_dedups_on_hash(backend: Arc<dyn CapsuleStore>) {
    backend
        .insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    let hash = format!("{:0>64}", "a");
    let dup = backend
        .find_by_idempotency_or_hash("t", &None, &hash)
        .await
        .unwrap();
    assert!(dup.is_some(), "content hash should match");
    assert_eq!(dup.unwrap().capability_capsule_id, "a");
}

async fn apply_feedback_useful_raises_confidence(backend: Arc<dyn CapsuleStore>) {
    let original = fixture("a", CapabilityCapsuleStatus::Active);
    let baseline_confidence = original.confidence;
    backend
        .insert_capability_capsule(original.clone())
        .await
        .unwrap();
    let event = FeedbackEvent {
        feedback_id: "fb_1".into(),
        capability_capsule_id: "a".into(),
        feedback_kind: FeedbackKind::Useful.as_str().to_string(),
        created_at: current_timestamp(),
        note: None,
    };
    let updated = backend.apply_feedback(&original, event).await.unwrap();
    assert!(
        updated.confidence > baseline_confidence,
        "useful feedback must raise confidence; baseline={baseline_confidence}, after={}",
        updated.confidence
    );
}

async fn apply_feedback_incorrect_archives(backend: Arc<dyn CapsuleStore>) {
    let original = fixture("a", CapabilityCapsuleStatus::Active);
    backend
        .insert_capability_capsule(original.clone())
        .await
        .unwrap();
    let event = FeedbackEvent {
        feedback_id: "fb_1".into(),
        capability_capsule_id: "a".into(),
        feedback_kind: FeedbackKind::Incorrect.as_str().to_string(),
        created_at: current_timestamp(),
        note: None,
    };
    let updated = backend.apply_feedback(&original, event).await.unwrap();
    assert_eq!(updated.status, CapabilityCapsuleStatus::Archived);
}

async fn delete_hard_removes_row(backend: Arc<dyn CapsuleStore>) {
    backend
        .insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    backend
        .delete_capability_capsule_hard("t", "a")
        .await
        .unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("t", "a")
        .await
        .unwrap();
    assert!(got.is_none(), "row should be gone after hard delete");
}

async fn fetch_by_ids_returns_only_requested(backend: Arc<dyn CapsuleStore>) {
    backend
        .insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    backend
        .insert_capability_capsule(fixture("b", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    backend
        .insert_capability_capsule(fixture("c", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    let got = backend
        .fetch_capability_capsules_by_ids("t", &["a", "c"])
        .await
        .unwrap();
    let mut ids: Vec<&str> = got
        .iter()
        .map(|r| r.capability_capsule_id.as_str())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["a", "c"]);
}

async fn fetch_by_ids_empty_short_circuits(backend: Arc<dyn CapsuleStore>) {
    backend
        .insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    let got = backend
        .fetch_capability_capsules_by_ids("t", &[])
        .await
        .unwrap();
    assert!(got.is_empty());
}

async fn replace_pending_with_successor_chains(backend: Arc<dyn CapsuleStore>) {
    backend
        .insert_capability_capsule(fixture("v1", CapabilityCapsuleStatus::PendingConfirmation))
        .await
        .unwrap();
    let mut successor = fixture("v2", CapabilityCapsuleStatus::Active);
    successor.version = 2;
    successor.supersedes_capability_capsule_id = Some("v1".into());
    backend
        .replace_pending_with_successor("t", "v1", successor)
        .await
        .unwrap();
    let v1 = backend
        .get_capability_capsule_for_tenant("t", "v1")
        .await
        .unwrap();
    let v2 = backend
        .get_capability_capsule_for_tenant("t", "v2")
        .await
        .unwrap();
    // Backend-specific: Lance archives v1 to `Rejected`; in-memory
    // matches that. Both must end up not-active. v2 must be the
    // new active row carrying the supersedes link.
    assert!(v1.is_some());
    assert_ne!(v1.unwrap().status, CapabilityCapsuleStatus::Active);
    let v2 = v2.expect("successor must be readable");
    assert_eq!(v2.status, CapabilityCapsuleStatus::Active);
    assert_eq!(v2.supersedes_capability_capsule_id.as_deref(), Some("v1"),);
}

// ── Test fanout: one #[tokio::test] per (scenario, backend) pair ────

macro_rules! parity {
    ($name:ident) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn lance_backend() {
                let (_dir, backend) = open_lance_backend().await;
                super::$name(backend).await;
            }

            #[tokio::test]
            async fn in_memory_backend() {
                let backend = open_in_memory_backend();
                super::$name(backend).await;
            }
        }
    };
}

parity!(insert_and_get_round_trip);
parity!(get_for_other_tenant_returns_none);
parity!(accept_pending_transitions_status);
parity!(reject_pending_transitions_status);
parity!(list_pending_review_filters_status);
parity!(find_by_idempotency_dedups_on_key);
parity!(find_by_idempotency_dedups_on_hash);
parity!(apply_feedback_useful_raises_confidence);
parity!(apply_feedback_incorrect_archives);
parity!(delete_hard_removes_row);
parity!(fetch_by_ids_returns_only_requested);
parity!(fetch_by_ids_empty_short_circuits);
parity!(replace_pending_with_successor_chains);
