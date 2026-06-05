//! End-to-end coverage for `POST /admin/vacuum`. The storage-layer
//! mechanics (which tables are touched, the `RemovalStats` aggregation)
//! are covered by unit tests in `src/worker/vacuum_worker.rs`. This
//! file exercises the HTTP / service plumbing.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType,
    },
    http,
    storage::Store,
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

mod common;

fn fixture(id: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "local".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        content_hash: format!("{:0>64}", id),
        source_agent: "test".into(),
        created_at: "00000000000000000000".into(),
        updated_at: "00000000000000000000".into(),
        ..Default::default()
    }
}

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
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
        let json: Value =
            serde_json::from_str(&text).unwrap_or_else(|_| panic!("response not json: {text}"));
        (status, json)
    }
}

async fn seeded_app() -> TestApp {
    let dir = tempdir().unwrap();
    let path = dir.path().join("vac.duckdb");
    let store = Arc::new(Store::open(&path).await.unwrap());
    // Generate several version manifests so the vacuum has something
    // to reclaim. Each insert + accept_pending touches the
    // capability_capsules table.
    store.insert_capability_capsule(fixture("a")).await.unwrap();
    for _ in 0..15 {
        let _ = store
            .set_capsule_status("local", "a", CapabilityCapsuleStatus::Active)
            .await;
    }

    let state = common::test_app_state(
        store.clone(),
        mem::service::CapabilityCapsuleService::new(store.clone()),
    );
    TestApp {
        _temp_dir: dir,
        router: http::router().with_state(state),
    }
}

#[tokio::test]
async fn admin_vacuum_with_zero_cutoff_reclaims() {
    let app = seeded_app().await;
    let (status, body) = app
        .post_json("/admin/vacuum", json!({"older_than_days": 0}))
        .await;
    assert_eq!(status, 200, "body: {body}");
    let bytes_removed = body["bytes_removed"].as_u64().unwrap();
    let old_versions = body["old_versions_removed"].as_u64().unwrap();
    let tables_pruned = body["tables_pruned"].as_u64().unwrap();
    assert!(tables_pruned > 0, "must visit at least one table: {body}");
    assert!(
        bytes_removed > 0 || old_versions > 0,
        "older_than=0 should reclaim something: {body}",
    );
}

#[tokio::test]
async fn admin_vacuum_with_high_cutoff_is_noop() {
    let app = seeded_app().await;
    let (status, body) = app
        .post_json("/admin/vacuum", json!({"older_than_days": 999_999}))
        .await;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["bytes_removed"], 0);
    assert_eq!(body["old_versions_removed"], 0);
    assert!(body["tables_pruned"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn admin_vacuum_without_body_uses_config_default() {
    // Empty body / missing JSON should fall back to the configured
    // `older_than_days`. `Config::local()` ships with 7d, so this
    // will be a no-op on a fresh tempdir but should return 200.
    let app = seeded_app().await;
    let request = Request::builder()
        .method("POST")
        .uri("/admin/vacuum")
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["tables_pruned"].as_u64().unwrap() > 0);
}
