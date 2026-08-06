use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, FeedbackKind,
    GraphEdge, Scope, Visibility,
};
use mem::service::{capability_capsule_service::ServiceError, CapabilityCapsuleService};
use mem::storage::{
    current_timestamp, CapsuleStore, EmbeddingJobInsert, EmbeddingJobStore, FeedbackEvent, Store,
};

fn capsule(id: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "t".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Private,
        version: 1,
        summary: "hard-delete retry fixture".into(),
        content: "verbatim fixture content".into(),
        content_hash: "0".repeat(64),
        confidence: 0.5,
        decay_score: 0.0,
        source_agent: "test".into(),
        created_at: "00000000000000000000".into(),
        updated_at: "00000000000000000000".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn hard_delete_retry_finishes_remaining_satellites_before_removing_parent() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("store")).await.unwrap());
    let id = "partial-cascade-parent";
    store.insert_capability_capsule(capsule(id)).await.unwrap();

    // Reproduce a retry after an earlier attempt already completed the
    // feedback step but failed before deleting a later satellite. Keeping the
    // parent until every idempotent cascade step succeeds preserves the tenant
    // authorization boundary for this retry.
    store
        .try_enqueue_embedding_job(EmbeddingJobInsert {
            job_id: "remaining-job".into(),
            tenant: "t".into(),
            capability_capsule_id: id.into(),
            target_content_hash: "remaining-hash".into(),
            provider: "fake".into(),
            available_at: current_timestamp(),
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .get_embedding_job_status("remaining-job")
            .await
            .unwrap()
            .as_deref(),
        Some("pending")
    );

    let service = CapabilityCapsuleService::new(store.clone());
    service
        .delete_capability_capsule_hard("t", id)
        .await
        .unwrap();

    assert!(store
        .get_capability_capsule_for_tenant("t", id)
        .await
        .unwrap()
        .is_none());
    assert!(store
        .get_embedding_job_status("remaining-job")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn stale_satellite_writes_are_rejected_after_hard_delete() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path()).await.unwrap();
    let service = CapabilityCapsuleService::new(Arc::new(store.clone()));
    let memory = capsule("mem_deleted_parent");
    store
        .insert_capability_capsule(memory.clone())
        .await
        .unwrap();
    service
        .delete_capability_capsule_hard("t", &memory.capability_capsule_id)
        .await
        .unwrap();

    let feedback_error = store
        .apply_feedback(
            &memory,
            FeedbackEvent {
                feedback_id: "feedback_after_delete".into(),
                capability_capsule_id: memory.capability_capsule_id.clone(),
                feedback_kind: "useful".into(),
                created_at: current_timestamp(),
                note: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        feedback_error,
        mem::storage::StorageError::NotFound(_)
    ));

    let now = current_timestamp();
    let job_error = store
        .try_enqueue_embedding_job(EmbeddingJobInsert {
            job_id: "job_after_delete".into(),
            tenant: "t".into(),
            capability_capsule_id: memory.capability_capsule_id.clone(),
            target_content_hash: memory.content_hash.clone(),
            provider: "fake".into(),
            available_at: now.clone(),
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .unwrap_err();
    assert!(matches!(job_error, mem::storage::StorageError::NotFound(_)));
    assert_eq!(
        store
            .feedback_summary(&memory.capability_capsule_id)
            .await
            .unwrap()
            .total,
        0
    );
    assert!(store
        .list_embedding_jobs("t", None, Some(&memory.capability_capsule_id), 10)
        .await
        .unwrap()
        .is_empty());

    let graph_error = store
        .add_edge_direct(&GraphEdge {
            from_node_id: format!("capability_capsule:{}", memory.capability_capsule_id),
            to_node_id: "entity:late-write".into(),
            relation: "mentions".into(),
            valid_from: current_timestamp(),
            valid_to: None,
            confidence: None,
            extractor: None,
            strength: None,
            stability: None,
            last_activated: None,
            access_count: None,
        })
        .await
        .unwrap_err();
    assert!(graph_error.to_string().contains("deleted capsule"));
}

#[tokio::test]
async fn hard_delete_wrong_tenant_does_not_clean_the_owners_satellites() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("store")).await.unwrap());
    let id = "tenant-owned-capsule";
    let mut owned = capsule(id);
    owned.tenant = "owner".into();
    store
        .insert_capability_capsule(owned.clone())
        .await
        .unwrap();
    store
        .apply_feedback(
            &owned,
            FeedbackEvent {
                feedback_id: "owner-feedback".into(),
                capability_capsule_id: id.into(),
                feedback_kind: FeedbackKind::Useful.as_str().into(),
                created_at: current_timestamp(),
                note: None,
            },
        )
        .await
        .unwrap();

    let service = CapabilityCapsuleService::new(store.clone());
    let error = service
        .delete_capability_capsule_hard("other", id)
        .await
        .unwrap_err();

    assert!(matches!(error, ServiceError::NotFound));
    assert!(store
        .get_capability_capsule_for_tenant("owner", id)
        .await
        .unwrap()
        .is_some());
    assert_eq!(store.feedback_summary(id).await.unwrap().total, 1);
}
