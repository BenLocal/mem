//! End-to-end coverage for the auto-promote sweep — both the storage
//! query (`Store::auto_promote_candidates`) and the HTTP endpoint
//! (`POST /reviews/auto_promote`).
//!
//! Unit-level coverage of the sweep logic lives in
//! `src/worker/auto_promote_worker.rs::tests` (in-process,
//! tempdir-isolated stores). This file complements it with the
//! HTTP-routed path so we exercise the service / Axum wiring too.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    http,
    storage::Store,
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

mod common;

fn ms_string(ms: u128) -> String {
    format!("{ms:020}")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn sample(
    id: &str,
    capability_capsule_type: CapabilityCapsuleType,
    status: CapabilityCapsuleStatus,
    updated_at_ms: u128,
    decay_score: f32,
) -> CapabilityCapsuleRecord {
    let ts = ms_string(updated_at_ms);
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "local".into(),
        capability_capsule_type,
        status,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary-{id}"),
        content: format!("content-{id}"),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.5,
        decay_score,
        content_hash: format!("{:0>64}", id),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test".into(),
        created_at: ts.clone(),
        updated_at: ts,
        last_validated_at: None,
    }
}

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
    repo: Arc<Store>,
}

impl TestApp {
    async fn post_json(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request build");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("request runs");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body read");
        let text = String::from_utf8(bytes.to_vec()).expect("body utf8");
        let body_json: Value =
            serde_json::from_str(&text).unwrap_or_else(|_| panic!("response not json: {text}"));
        (status, body_json)
    }
}

async fn seeded_app() -> TestApp {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auto-promote.duckdb");
    let store = Arc::new(Store::open(&path).await.unwrap());
    let n = now_ms();
    let ten_days_ago = n - 10 * 86_400_000;
    let one_day_ago = n - 86_400_000;

    // Eligible: pending experience, idle 10 days, low decay.
    store
        .insert_capability_capsule(sample(
            "eligible",
            CapabilityCapsuleType::Experience,
            CapabilityCapsuleStatus::PendingConfirmation,
            ten_days_ago,
            0.0,
        ))
        .await
        .unwrap();
    // Too young.
    store
        .insert_capability_capsule(sample(
            "young",
            CapabilityCapsuleType::Experience,
            CapabilityCapsuleStatus::PendingConfirmation,
            one_day_ago,
            0.0,
        ))
        .await
        .unwrap();
    // Wrong type (Preference excluded by default).
    store
        .insert_capability_capsule(sample(
            "pref",
            CapabilityCapsuleType::Preference,
            CapabilityCapsuleStatus::PendingConfirmation,
            ten_days_ago,
            0.0,
        ))
        .await
        .unwrap();
    // Decay too high.
    store
        .insert_capability_capsule(sample(
            "decayed",
            CapabilityCapsuleType::Experience,
            CapabilityCapsuleStatus::PendingConfirmation,
            ten_days_ago,
            0.6,
        ))
        .await
        .unwrap();
    // Already active — not a candidate.
    store
        .insert_capability_capsule(sample(
            "active",
            CapabilityCapsuleType::Experience,
            CapabilityCapsuleStatus::Active,
            ten_days_ago,
            0.0,
        ))
        .await
        .unwrap();

    let state = common::test_app_state(
        store.clone(),
        mem::service::CapabilityCapsuleService::new(store.clone()),
    );
    TestApp {
        _temp_dir: dir,
        router: http::router().with_state(state),
        repo: store,
    }
}

#[tokio::test]
async fn dry_run_returns_eligible_ids_and_writes_nothing() {
    let app = seeded_app().await;

    let (status, body) = app
        .post_json(
            "/reviews/auto_promote",
            json!({"tenant": "local", "dry_run": true}),
        )
        .await;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["dry_run"], true);
    let ids: Vec<String> = body["capability_capsule_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["eligible".to_string()]);

    // Status untouched.
    let row = app
        .repo
        .get_capability_capsule_for_tenant("local", "eligible")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, CapabilityCapsuleStatus::PendingConfirmation);
}

#[tokio::test]
async fn live_run_promotes_eligible_and_skips_others() {
    let app = seeded_app().await;

    let (status, body) = app
        .post_json(
            "/reviews/auto_promote",
            json!({"tenant": "local", "dry_run": false}),
        )
        .await;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["dry_run"], false);
    let ids: Vec<String> = body["capability_capsule_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["eligible".to_string()]);

    let eligible = app
        .repo
        .get_capability_capsule_for_tenant("local", "eligible")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(eligible.status, CapabilityCapsuleStatus::Active);

    // Other rows must remain untouched.
    for id in ["young", "pref", "decayed"] {
        let r = app
            .repo
            .get_capability_capsule_for_tenant("local", id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            r.status,
            CapabilityCapsuleStatus::PendingConfirmation,
            "row {id} unexpectedly transitioned",
        );
    }
}
