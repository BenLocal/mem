//! Integration tests for `POST /fact_check` (closes mempalace-diff-v3 #29).
//!
//! Three signals to cover:
//!   1. similar_names — Levenshtein ≤ 2 against existing canonical names.
//!   2. relationship_conflicts — direction-reversed identical predicate.
//!   3. kg_contradictions — (a) same (subject, predicate, *) active with
//!      different object; (b) restating a closed (subject, predicate, object).

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{domain::capability_capsule::GraphEdge, domain::EntityKind, http, storage::Store};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

mod common;

const TENANT: &str = "local";

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
    store: Arc<Store>,
}

async fn test_app() -> TestApp {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("mem.lance");
    let store = Arc::new(Store::open(&path).await.expect("Store::open"));
    store.set_transcript_job_provider("fake");
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

async fn post_json(app: &TestApp, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/fact_check")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = app.router.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value: Value = serde_json::from_slice(&body).expect("response body should be valid json");
    (status, value)
}

async fn seed_entity(app: &TestApp, canonical: &str, kind: EntityKind) -> String {
    let now = mem::storage::current_timestamp();
    app.store
        .resolve_or_create(TENANT, canonical, kind, &now)
        .await
        .expect("resolve_or_create")
}

#[tokio::test(flavor = "multi_thread")]
async fn typo_in_topic_surfaces_similar_name() {
    let app = test_app().await;
    let alice_id = seed_entity(&app, "Alice", EntityKind::Topic).await;

    let (status, body) = post_json(
        &app,
        json!({
            "tenant": TENANT,
            "topics": ["Alic"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");

    let similar = body
        .get("similar_names")
        .and_then(|v| v.as_array())
        .expect("similar_names array");
    assert_eq!(similar.len(), 1, "exactly one near-miss: {body}");
    let entry = &similar[0];
    assert_eq!(entry["in_input"], "Alic");
    let matches = entry["matches"].as_array().expect("matches array");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["canonical_name"], "Alice");
    assert_eq!(matches[0]["entity_id"], alice_id);
    assert_eq!(matches[0]["edit_distance"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_match_does_not_appear_in_similar_names() {
    let app = test_app().await;
    let _ = seed_entity(&app, "Phoenix", EntityKind::Project).await;

    let (status, body) = post_json(
        &app,
        json!({
            "tenant": TENANT,
            "topics": ["Phoenix"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let similar = body
        .get("similar_names")
        .and_then(|v| v.as_array())
        .expect("similar_names");
    assert!(similar.is_empty(), "exact alias hit must not fire: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn short_tokens_are_skipped_for_typo_scan() {
    let app = test_app().await;
    let _ = seed_entity(&app, "Bob", EntityKind::Topic).await; // 3 chars — under floor

    let (status, body) = post_json(
        &app,
        json!({
            "tenant": TENANT,
            "topics": ["Bib"], // also 3 chars, would be distance-1 from "Bob"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let similar = body
        .get("similar_names")
        .and_then(|v| v.as_array())
        .expect("similar_names");
    assert!(
        similar.is_empty(),
        "3-char tokens are below the length floor: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn content_tokenization_picks_up_misspellings() {
    let app = test_app().await;
    let _ = seed_entity(&app, "Phoenix", EntityKind::Project).await;

    let (status, body) = post_json(
        &app,
        json!({
            "tenant": TENANT,
            "content": "We shipped the Pheonix migration today.",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let similar = body
        .get("similar_names")
        .and_then(|v| v.as_array())
        .expect("similar_names");
    let names: Vec<&str> = similar
        .iter()
        .map(|s| s["in_input"].as_str().unwrap_or_default())
        .collect();
    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("pheonix")),
        "expected 'Pheonix' to surface as near-miss for 'Phoenix': {body}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn direction_reversed_edge_flags_relationship_conflict() {
    let app = test_app().await;
    let alice = seed_entity(&app, "Alice", EntityKind::Topic).await;
    let phoenix = seed_entity(&app, "Phoenix", EntityKind::Project).await;

    // KG asserts: project --managed_by--> alice
    let now = mem::storage::current_timestamp();
    let edge = GraphEdge {
        from_node_id: format!("entity:{phoenix}"),
        to_node_id: format!("entity:{alice}"),
        relation: "managed_by".into(),
        valid_from: now.clone(),
        valid_to: None,
        confidence: None,
        extractor: None,
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    };
    app.store
        .add_edge_direct(&edge)
        .await
        .expect("add_edge_direct");

    // Caller claims: alice --managed_by--> phoenix (reversed direction).
    let (status, body) = post_json(
        &app,
        json!({
            "tenant": TENANT,
            "relationships": [
                { "subject": "Alice", "predicate": "managed_by", "object": "Phoenix" }
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    let conflicts = body["relationship_conflicts"]
        .as_array()
        .expect("conflicts");
    assert_eq!(
        conflicts.len(),
        1,
        "expected one direction mismatch: {body}"
    );
    let c = &conflicts[0];
    assert_eq!(c["subject"], "Alice");
    assert_eq!(c["predicate"], "managed_by");
    assert_eq!(c["object"], "Phoenix");
    assert_eq!(c["existing_edge"]["relation"], "managed_by");
    assert_eq!(
        c["existing_edge"]["from_node_id"],
        format!("entity:{phoenix}")
    );
    assert_eq!(c["existing_edge"]["to_node_id"], format!("entity:{alice}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn value_change_on_same_predicate_flags_kg_contradiction() {
    let app = test_app().await;
    let phoenix = seed_entity(&app, "Phoenix", EntityKind::Project).await;
    // Alice is named in the claim — registry must know her so the
    // service resolves her alias; the entity_id itself isn't asserted
    // on, so no local binding is needed.
    let _ = seed_entity(&app, "Alice", EntityKind::Topic).await;
    let bob = seed_entity(&app, "Bobby", EntityKind::Topic).await; // 5 chars — long enough

    // KG: phoenix --owned_by--> bob (active)
    let now = mem::storage::current_timestamp();
    let edge = GraphEdge {
        from_node_id: format!("entity:{phoenix}"),
        to_node_id: format!("entity:{bob}"),
        relation: "owned_by".into(),
        valid_from: now.clone(),
        valid_to: None,
        confidence: None,
        extractor: None,
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    };
    app.store.add_edge_direct(&edge).await.unwrap();

    // Caller claims phoenix --owned_by--> alice (different object).
    let (status, body) = post_json(
        &app,
        json!({
            "tenant": TENANT,
            "relationships": [
                { "subject": "Phoenix", "predicate": "owned_by", "object": "Alice" }
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    let contradictions = body["kg_contradictions"]
        .as_array()
        .expect("contradictions");
    assert!(
        !contradictions.is_empty(),
        "value-changed claim should produce a contradiction: {body}",
    );
    let found = contradictions.iter().any(|c| {
        c["existing"]["to_node_id"] == format!("entity:{bob}")
            && c["existing"]["relation"] == "owned_by"
            && c["claim"].as_str().unwrap_or_default().contains("Alice")
    });
    assert!(
        found,
        "expected bob-as-current-owner contradiction in: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restating_closed_edge_flags_kg_contradiction() {
    let app = test_app().await;
    let phoenix = seed_entity(&app, "Phoenix", EntityKind::Project).await;
    let alice = seed_entity(&app, "Alice", EntityKind::Topic).await;

    // KG: phoenix --depended_on--> alice, then close it.
    let now = mem::storage::current_timestamp();
    let edge = GraphEdge {
        from_node_id: format!("entity:{phoenix}"),
        to_node_id: format!("entity:{alice}"),
        relation: "depended_on".into(),
        valid_from: now.clone(),
        valid_to: None,
        confidence: None,
        extractor: None,
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    };
    app.store.add_edge_direct(&edge).await.unwrap();
    let closed = app
        .store
        .invalidate_edge(
            &format!("entity:{phoenix}"),
            "depended_on",
            &format!("entity:{alice}"),
            &now,
        )
        .await
        .expect("invalidate_edge");
    assert_eq!(closed, 1, "should close exactly one edge");

    // Caller restates the previously-invalidated fact verbatim.
    let (status, body) = post_json(
        &app,
        json!({
            "tenant": TENANT,
            "relationships": [
                { "subject": "Phoenix", "predicate": "depended_on", "object": "Alice" }
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    let contradictions = body["kg_contradictions"]
        .as_array()
        .expect("contradictions");
    let restated = contradictions.iter().any(|c| {
        c["note"]
            .as_str()
            .unwrap_or_default()
            .contains("previously-invalidated")
            && c["existing"]["valid_to"] != Value::Null
    });
    assert!(
        restated,
        "expected restate-closed-edge contradiction in: {body}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_request_returns_empty_report() {
    let app = test_app().await;
    let (status, body) = post_json(&app, json!({ "tenant": TENANT })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["similar_names"].as_array().unwrap().len(), 0);
    assert_eq!(body["relationship_conflicts"].as_array().unwrap().len(), 0);
    assert_eq!(body["kg_contradictions"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn unresolved_entity_in_relationship_silently_skips() {
    let app = test_app().await;
    // No entities seeded.

    let (status, body) = post_json(
        &app,
        json!({
            "tenant": TENANT,
            "relationships": [
                { "subject": "Ghost", "predicate": "manages", "object": "Phantom" }
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["relationship_conflicts"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(body["kg_contradictions"].as_array().unwrap().is_empty());
}
