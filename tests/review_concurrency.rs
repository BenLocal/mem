use std::sync::Arc;

use mem::{
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    service::{capability_capsule_service::ServiceError, CapabilityCapsuleService},
    storage::{EvolutionCandidate, EvolutionCandidateStore, StorageError, Store},
    worker::evolution_worker::EVOLUTION_SOURCE_AGENT,
};

fn pending(id: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.to_owned(),
        tenant: "local".to_owned(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::PendingConfirmation,
        scope: Scope::Repo,
        visibility: Visibility::Private,
        version: 1,
        summary: format!("summary-{id}"),
        content: format!("content-{id}"),
        content_hash: format!("hash-{id}"),
        source_agent: "review-test".to_owned(),
        created_at: "00000000000000000001".to_owned(),
        updated_at: "00000000000000000001".to_owned(),
        ..Default::default()
    }
}

#[tokio::test]
async fn concurrent_verdict_settles_evolution_candidate_once_to_winner() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(dir.path().join("evolution-review.lance"))
            .await
            .unwrap(),
    );
    let mut capsule = pending("evolution-race");
    capsule.source_agent = EVOLUTION_SOURCE_AGENT.to_owned();
    store.insert_capability_capsule(capsule).await.unwrap();
    store
        .upsert_evolution_candidate(EvolutionCandidate {
            candidate_id: "candidate-race".to_owned(),
            tenant: "local".to_owned(),
            op_kind: "generalize".to_owned(),
            member_ids: vec!["source".to_owned()],
            params: "{}".to_owned(),
            evidence: 1.0,
            consecutive_cycles: 1,
            status: "executed".to_owned(),
            first_proposed_at: "00000000000000000001".to_owned(),
            last_signal_at: "00000000000000000001".to_owned(),
            executed_at: Some("00000000000000000001".to_owned()),
            result_capsule_ids: vec!["evolution-race".to_owned()],
        })
        .await
        .unwrap();
    let service = CapabilityCapsuleService::new(store.clone());

    let (accepted, rejected) = tokio::join!(
        service.accept_pending("local", "evolution-race"),
        service.reject_pending("local", "evolution-race"),
    );
    assert_eq!(
        usize::from(accepted.is_ok()) + usize::from(rejected.is_ok()),
        1,
        "only the committed verdict may run evolution post-processing"
    );

    let capsule = store
        .get_capability_capsule_for_tenant("local", "evolution-race")
        .await
        .unwrap()
        .unwrap();
    let expected_candidate_status = match capsule.status {
        CapabilityCapsuleStatus::Active => "accepted",
        CapabilityCapsuleStatus::Rejected => "rejected",
        other => panic!("unexpected review terminal status: {other:?}"),
    };
    let settled = store
        .list_evolution_candidates("local", Some(expected_candidate_status))
        .await
        .unwrap();
    assert_eq!(settled.len(), 1);
    assert!(store
        .list_evolution_candidates("local", Some("executed"))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn late_competing_verdict_returns_review_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("review.lance")).await.unwrap());
    store
        .insert_capability_capsule(pending("race"))
        .await
        .unwrap();
    let service = CapabilityCapsuleService::new(store);

    service.accept_pending("local", "race").await.unwrap();
    let loser = service.reject_pending("local", "race").await.unwrap_err();

    assert!(
        matches!(
            loser,
            ServiceError::Storage(StorageError::Conflict("review conflict"))
        ),
        "late competing verdict must remain distinguishable from a missing capsule: {loser}"
    );
}
