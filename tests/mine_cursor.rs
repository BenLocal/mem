//! Integration tests for v3 #32 mine cursor.
//!
//! Storage round-trip + HTTP endpoints (GET / POST /mine/cursors).
//! The CLI fast-skip behavior is covered by the unit test in
//! `cli/mine.rs` indirectly (the `run_with_counts` happy path is
//! exercised by the existing `tests/cli_mine.rs` suite — cursor
//! reads / writes are best-effort and never fail the mine).

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    http,
    storage::{MineCursorStore, Store},
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

mod common;

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
    store: Arc<Store>,
}

async fn test_app() -> TestApp {
    let dir = tempdir().expect("tempdir");
    let store = Arc::new(Store::open(&dir.path().join("mc.lance")).await.unwrap());
    let state = common::test_app_state(
        store.clone(),
        mem::service::CapabilityCapsuleService::new(store.clone()),
    );
    TestApp {
        _temp_dir: dir,
        router: http::router().with_state(state),
        store,
    }
}

async fn get_cursor(app: &TestApp, path: &str) -> (StatusCode, Value) {
    // Manual minimal URL encoding (just `/` → `%2F`) — keeps test
    // free of an extra urlencoding crate dependency.
    let encoded = path.replace('/', "%2F");
    let uri = format!("/mine/cursors?transcript_path={encoded}");
    let req = Request::builder()
        .method("GET")
        .uri(&uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    (status, v)
}

async fn post_cursor(app: &TestApp, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/mine/cursors")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    (status, v)
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_missing_returns_404() {
    let app = test_app().await;
    let (status, body) = get_cursor(&app, "/no/such/file.jsonl").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_round_trip() {
    let app = test_app().await;
    let path = "/root/.claude/projects/foo/abc.jsonl";

    // First POST creates the cursor at line 100.
    let (s1, b1) = post_cursor(
        &app,
        json!({"transcript_path": path, "last_line_number": 100}),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["last_line_number"], 100);
    assert!(b1["updated_at"].as_str().is_some());

    // GET returns the row.
    let (s2, b2) = get_cursor(&app, path).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b2["transcript_path"], path);
    assert_eq!(b2["last_line_number"], 100);
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_upsert_overwrites_existing() {
    let app = test_app().await;
    let path = "/tmp/test.jsonl";

    post_cursor(
        &app,
        json!({"transcript_path": path, "last_line_number": 50}),
    )
    .await;
    let (_, b1) = get_cursor(&app, path).await;
    assert_eq!(b1["last_line_number"], 50);

    // Upsert with higher number.
    post_cursor(
        &app,
        json!({"transcript_path": path, "last_line_number": 250}),
    )
    .await;
    let (_, b2) = get_cursor(&app, path).await;
    assert_eq!(b2["last_line_number"], 250);

    // Verify only ONE row at storage layer (upsert = delete-then-insert,
    // not append).
    let stored = app.store.get_mine_cursor(path).await.unwrap().unwrap();
    assert_eq!(stored.last_line_number, 250);
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_rejects_negative_line_number() {
    let app = test_app().await;
    let req = Request::builder()
        .method("POST")
        .uri("/mine/cursors")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"transcript_path": "/tmp/x.jsonl", "last_line_number": -1}).to_string(),
        ))
        .unwrap();
    let resp = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_distinct_paths_dont_collide() {
    let app = test_app().await;
    post_cursor(
        &app,
        json!({"transcript_path": "/a.jsonl", "last_line_number": 10}),
    )
    .await;
    post_cursor(
        &app,
        json!({"transcript_path": "/b.jsonl", "last_line_number": 99}),
    )
    .await;

    let (_, ba) = get_cursor(&app, "/a.jsonl").await;
    let (_, bb) = get_cursor(&app, "/b.jsonl").await;
    assert_eq!(ba["last_line_number"], 10);
    assert_eq!(bb["last_line_number"], 99);
}
