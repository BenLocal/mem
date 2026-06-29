//! G5 — structured user/project profile (`build_profile` /
//! `POST /capability_capsules/profile`). Verifies the read-side aggregation:
//! only Active `Preference` + `Workflow` capsules in scope, scoped by project,
//! excluding other types and out-of-scope rows.

use std::sync::Arc;

use mem::{
    domain::capability_capsule::{
        CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
    },
    service::CapabilityCapsuleService,
    storage::Store,
};
use tempfile::tempdir;

const TENANT: &str = "local";

async fn make_service() -> (tempfile::TempDir, CapabilityCapsuleService) {
    let dir = tempdir().unwrap();
    let store = Arc::new(
        Store::open(&dir.path().join("profile.lance"))
            .await
            .unwrap(),
    );
    (dir, CapabilityCapsuleService::new(store))
}

fn req(ct: CapabilityCapsuleType, content: &str, project: &str) -> IngestCapabilityCapsuleRequest {
    IngestCapabilityCapsuleRequest {
        tenant: TENANT.into(),
        capability_capsule_type: ct,
        content: content.into(),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Project,
        visibility: Visibility::Shared,
        project: Some(project.into()),
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test-agent".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
        supersedes_capability_capsule_id: None,
        expires_at: None,
    }
}

/// Preference / Workflow ingest lands as PendingConfirmation; accept it so it
/// becomes Active (the status the profile surfaces).
async fn ingest_active(svc: &CapabilityCapsuleService, r: IngestCapabilityCapsuleRequest) {
    let resp = svc.ingest(r).await.expect("ingest");
    // Accept is a no-op error for already-active rows (Implementation/Auto);
    // only call it for the guidance types that land pending.
    let _ = svc
        .accept_pending(TENANT, &resp.capability_capsule_id)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_aggregates_active_preferences_and_workflows_in_scope() {
    let (_dir, svc) = make_service().await;

    ingest_active(
        &svc,
        req(
            CapabilityCapsuleType::Preference,
            "always run cargo fmt and clippy before committing in this project",
            "phoenix",
        ),
    )
    .await;
    ingest_active(
        &svc,
        req(
            CapabilityCapsuleType::Workflow,
            "deploy: stop service, swap binary, start, verify health endpoint",
            "phoenix",
        ),
    )
    .await;
    // An Implementation capsule (Active via Auto) must NOT appear in the profile.
    ingest_active(
        &svc,
        req(
            CapabilityCapsuleType::Implementation,
            "the retrieve scorer sums an additive integer lifecycle stack",
            "phoenix",
        ),
    )
    .await;
    // A Preference for a DIFFERENT project must be scoped out.
    ingest_active(
        &svc,
        req(
            CapabilityCapsuleType::Preference,
            "use four-space indentation in the other project",
            "other",
        ),
    )
    .await;

    let profile = svc
        .build_profile(TENANT, Some("phoenix"), None, 100)
        .await
        .expect("build_profile");

    assert_eq!(
        profile.preference_count,
        1,
        "exactly the one phoenix preference (other-project pref scoped out): {:?}",
        profile
            .preferences
            .iter()
            .map(|p| &p.content)
            .collect::<Vec<_>>()
    );
    assert_eq!(profile.workflow_count, 1, "the one phoenix workflow");
    assert!(profile.preferences[0].content.contains("cargo fmt"));
    assert!(profile.workflows[0].content.contains("deploy"));
    // No Implementation capsule leaked into either guidance bucket.
    assert!(profile
        .preferences
        .iter()
        .chain(profile.workflows.iter())
        .all(|c| c.capability_capsule_type != CapabilityCapsuleType::Implementation));
    assert_eq!(profile.project.as_deref(), Some("phoenix"));
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_empty_scope_is_zero_not_error() {
    let (_dir, svc) = make_service().await;
    let profile = svc
        .build_profile(TENANT, Some("nonexistent"), None, 100)
        .await
        .expect("empty profile must succeed, not error");
    assert_eq!(profile.preference_count, 0);
    assert_eq!(profile.workflow_count, 0);
    assert!(profile.preferences.is_empty() && profile.workflows.is_empty());
}
