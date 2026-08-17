//! RED contract tests for the Skill proposal lifecycle gate.
//!
//! A Workflow produced by the Skill proposal compiler is not an ordinary
//! pending capsule: both automated promotion and generic review acceptance
//! must leave it pending until the dedicated bundle acceptance path exists.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use mem::{
    domain::capability_capsule::{
        ActivationPolicy, CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType,
        Scope, Visibility,
    },
    http,
    service::CapabilityCapsuleService,
    storage::Store,
};
use serde_json::json;
use tempfile::TempDir;
use tower::util::ServiceExt;

mod common;

const TENANT: &str = "local";
const PROPOSAL_ID: &str = "skill-proposal-lifecycle-red";

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
    store: Arc<Store>,
}

async fn test_app() -> TestApp {
    let (temp_dir, store) = common::test_store().await;
    let mut state =
        common::test_app_state(store.clone(), CapabilityCapsuleService::new(store.clone()));
    // Deliberately make Workflow eligible at the configuration layer. The
    // per-record activation policy must remain the final safety boundary.
    state.config.auto_promote.types = vec![CapabilityCapsuleType::Workflow];

    TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        store,
    }
}

fn skill_proposal() -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: PROPOSAL_ID.to_owned(),
        tenant: TENANT.to_owned(),
        capability_capsule_type: CapabilityCapsuleType::Workflow,
        status: CapabilityCapsuleStatus::PendingConfirmation,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: "Pending Skill bundle review".to_owned(),
        content: "Run the reviewed Skill workflow".to_owned(),
        project: Some("mem".to_owned()),
        repo: Some("mem".to_owned()),
        confidence: 0.6,
        decay_score: 0.0,
        content_hash: "1".repeat(64),
        source_agent: "skill-proposal-compiler".to_owned(),
        // Old enough to be selected by an age-based auto-promote scan.
        created_at: "00000000000000000001".to_owned(),
        updated_at: "00000000000000000001".to_owned(),
        ..Default::default()
    }
}

async fn seed_skill_proposal(app: &TestApp) {
    assert_eq!(
        skill_proposal().activation_policy(),
        ActivationPolicy::SkillBundleRequired,
    );
    app.store
        .insert_capability_capsule(skill_proposal())
        .await
        .expect("seed Skill proposal");
}

fn authenticated_post(path: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", common::TEST_ADMIN_TOKEN),
        )
        .body(body)
        .expect("request build")
}

async fn stored_proposal(app: &TestApp) -> CapabilityCapsuleRecord {
    app.store
        .get_capability_capsule_for_tenant(TENANT, PROPOSAL_ID)
        .await
        .expect("proposal read")
        .expect("proposal exists")
}

#[tokio::test]
async fn unauthenticated_malformed_accept_is_401_before_json_parsing() {
    let app = test_app().await;
    let request = Request::builder()
        .method("POST")
        .uri("/reviews/pending/accept")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{malformed-json"))
        .expect("request build");

    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("request runs");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn skill_bundle_required_workflow_cannot_be_auto_promoted() {
    let app = test_app().await;
    seed_skill_proposal(&app).await;
    let request = authenticated_post(
        "/reviews/auto_promote",
        Body::from(json!({"tenant": TENANT, "dry_run": false}).to_string()),
    );

    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("request runs");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        stored_proposal(&app).await.status,
        CapabilityCapsuleStatus::PendingConfirmation,
    );
}

#[tokio::test]
async fn generic_accept_rejects_skill_bundle_required_and_keeps_it_pending() {
    let app = test_app().await;
    seed_skill_proposal(&app).await;
    let request = authenticated_post(
        "/reviews/pending/accept",
        Body::from(
            json!({
                "tenant": TENANT,
                "capability_capsule_id": PROPOSAL_ID,
            })
            .to_string(),
        ),
    );

    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("request runs");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        stored_proposal(&app).await.status,
        CapabilityCapsuleStatus::PendingConfirmation,
    );
}

#[tokio::test]
async fn generic_reject_also_requires_the_skill_governance_path() {
    let app = test_app().await;
    seed_skill_proposal(&app).await;
    let request = authenticated_post(
        "/reviews/pending/reject",
        Body::from(
            json!({
                "tenant": TENANT,
                "capability_capsule_id": PROPOSAL_ID,
            })
            .to_string(),
        ),
    );
    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("request runs");
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        stored_proposal(&app).await.status,
        CapabilityCapsuleStatus::PendingConfirmation,
    );
}
