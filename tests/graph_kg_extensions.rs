//! Integration tests for K4 (kg_query_predicate) + K5 (graph_neighbor
//! fuzzy suggestions). Both extensions ride on existing tables / pure
//! reads; tests focus on the response shapes + corner cases.

use std::sync::Arc;

use mem::{
    domain::capability_capsule::GraphEdge,
    domain::EntityKind,
    service::{CapabilityCapsuleService, NeighborSuggestion},
    storage::{current_timestamp, Store},
};
use tempfile::tempdir;

const TENANT: &str = "local";

async fn open_store() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("kg.lance")).await.unwrap());
    (dir, store)
}

async fn add_edge(store: &Store, from: &str, to: &str, relation: &str, valid_from: &str) {
    let edge = GraphEdge {
        from_node_id: from.into(),
        to_node_id: to.into(),
        relation: relation.into(),
        valid_from: valid_from.into(),
        valid_to: None,
        confidence: None,
        extractor: None,
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    };
    store.add_edge_direct(&edge).await.unwrap();
}

// ────────────────────────── K4: query_predicate ──────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k4_query_predicate_returns_every_edge_with_the_relation() {
    let (_dir, store) = open_store().await;
    let svc = CapabilityCapsuleService::new(store.clone());

    add_edge(
        &store,
        "entity:a",
        "entity:b",
        "manages",
        "00000000000000000010",
    )
    .await;
    add_edge(
        &store,
        "entity:c",
        "entity:d",
        "manages",
        "00000000000000000020",
    )
    .await;
    add_edge(
        &store,
        "entity:e",
        "entity:f",
        "depends_on",
        "00000000000000000030",
    )
    .await;

    let edges = svc.graph_query_predicate("manages", None).await.unwrap();
    assert_eq!(
        edges.len(),
        2,
        "expected both `manages` edges, got {edges:?}"
    );
    let pairs: Vec<(&str, &str)> = edges
        .iter()
        .map(|e| (e.from_node_id.as_str(), e.to_node_id.as_str()))
        .collect();
    assert!(pairs.contains(&("entity:a", "entity:b")));
    assert!(pairs.contains(&("entity:c", "entity:d")));
}

#[tokio::test(flavor = "multi_thread")]
async fn k4_query_predicate_unknown_returns_empty() {
    let (_dir, store) = open_store().await;
    let svc = CapabilityCapsuleService::new(store.clone());
    add_edge(
        &store,
        "entity:a",
        "entity:b",
        "manages",
        "00000000000000000010",
    )
    .await;

    let edges = svc.graph_query_predicate("never_used", None).await.unwrap();
    assert!(edges.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn k4_query_predicate_as_of_filters_to_active_at_timestamp() {
    let (_dir, store) = open_store().await;
    let svc = CapabilityCapsuleService::new(store.clone());

    // edge_a: valid 10–20, edge_b: valid from 30 onwards
    add_edge(
        &store,
        "entity:a",
        "entity:b",
        "owns",
        "00000000000000000010",
    )
    .await;
    store
        .invalidate_edge("entity:a", "owns", "entity:b", "00000000000000000020")
        .await
        .unwrap();
    add_edge(
        &store,
        "entity:c",
        "entity:d",
        "owns",
        "00000000000000000030",
    )
    .await;

    // as_of=15 → only edge_a should match (active at 15)
    let at_15 = svc
        .graph_query_predicate("owns", Some("00000000000000000015"))
        .await
        .unwrap();
    let ids_15: Vec<_> = at_15
        .iter()
        .map(|e| (e.from_node_id.as_str(), e.to_node_id.as_str()))
        .collect();
    assert_eq!(ids_15, vec![("entity:a", "entity:b")]);

    // as_of=25 → neither (edge_a closed at 20, edge_b starts at 30)
    let at_25 = svc
        .graph_query_predicate("owns", Some("00000000000000000025"))
        .await
        .unwrap();
    assert!(at_25.is_empty());

    // as_of=35 → only edge_b
    let at_35 = svc
        .graph_query_predicate("owns", Some("00000000000000000035"))
        .await
        .unwrap();
    let ids_35: Vec<_> = at_35
        .iter()
        .map(|e| (e.from_node_id.as_str(), e.to_node_id.as_str()))
        .collect();
    assert_eq!(ids_35, vec![("entity:c", "entity:d")]);

    // as_of=None → both edges (active + closed)
    let all = svc.graph_query_predicate("owns", None).await.unwrap();
    assert_eq!(all.len(), 2);
}

// ────────────────────────── K5: fuzzy suggestions ──────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k5_typo_in_entity_node_id_surfaces_canonical_match() {
    let (_dir, store) = open_store().await;
    let svc = CapabilityCapsuleService::new(store.clone());

    // Seed entities; caller passes a near-miss node_id suffix.
    let now = current_timestamp();
    store
        .resolve_or_create(TENANT, "Phoenix", EntityKind::Project, &now)
        .await
        .unwrap();
    store
        .resolve_or_create(TENANT, "Atlas", EntityKind::Project, &now)
        .await
        .unwrap();

    // Caller types `entity:Pheonix` (typo). Should suggest Phoenix.
    let suggestions = svc
        .graph_neighbor_suggestions(TENANT, "entity:Pheonix", 5)
        .await
        .unwrap();
    let names: Vec<&str> = suggestions
        .iter()
        .map(|s| s.canonical_name.as_str())
        .collect();
    assert!(
        names.contains(&"Phoenix"),
        "expected Phoenix in suggestions, got {names:?}",
    );
    // Atlas is too far (Lev > 3), shouldn't appear.
    assert!(!names.contains(&"Atlas"));
}

#[tokio::test(flavor = "multi_thread")]
async fn k5_exact_match_returns_no_suggestions() {
    let (_dir, store) = open_store().await;
    let svc = CapabilityCapsuleService::new(store.clone());
    let now = current_timestamp();
    store
        .resolve_or_create(TENANT, "Phoenix", EntityKind::Project, &now)
        .await
        .unwrap();

    // Exact alias (suffix matches canonical exactly after normalize)
    // produces no suggestions — the helper deliberately drops dist=0.
    let suggestions = svc
        .graph_neighbor_suggestions(TENANT, "entity:Phoenix", 5)
        .await
        .unwrap();
    assert!(
        suggestions.is_empty(),
        "exact match should not surface as a suggestion: {suggestions:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn k5_no_close_matches_returns_empty() {
    let (_dir, store) = open_store().await;
    let svc = CapabilityCapsuleService::new(store.clone());
    let now = current_timestamp();
    store
        .resolve_or_create(TENANT, "Phoenix", EntityKind::Project, &now)
        .await
        .unwrap();

    // Lev distance > 3 — no suggestion.
    let suggestions = svc
        .graph_neighbor_suggestions(TENANT, "entity:XYZQR12345", 5)
        .await
        .unwrap();
    assert!(suggestions.is_empty(), "got {suggestions:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn k5_limit_caps_result_count() {
    let (_dir, store) = open_store().await;
    let svc = CapabilityCapsuleService::new(store.clone());
    let now = current_timestamp();
    // Seed 4 entities that are all 2 edits from "alic" → all should
    // match Lev ≤ 3, but limit=2 caps the response.
    for name in ["Alice", "Alica", "Alicia", "Alex"] {
        store
            .resolve_or_create(TENANT, name, EntityKind::Topic, &now)
            .await
            .unwrap();
    }
    let suggestions = svc
        .graph_neighbor_suggestions(TENANT, "entity:alic", 2)
        .await
        .unwrap();
    assert_eq!(
        suggestions.len(),
        2,
        "limit must cap output, got {suggestions:?}"
    );
    // Sorted by edit_distance ascending — closer matches first.
    let dists: Vec<usize> = suggestions.iter().map(|s| s.edit_distance).collect();
    assert!(
        dists.windows(2).all(|w| w[0] <= w[1]),
        "suggestions must be sorted by edit_distance asc: {dists:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn k5_suggestion_shape_includes_node_id_ready_to_retry() {
    let (_dir, store) = open_store().await;
    let svc = CapabilityCapsuleService::new(store.clone());
    let now = current_timestamp();
    let entity_id = store
        .resolve_or_create(TENANT, "Phoenix", EntityKind::Project, &now)
        .await
        .unwrap();

    let suggestions: Vec<NeighborSuggestion> = svc
        .graph_neighbor_suggestions(TENANT, "entity:Pheonix", 5)
        .await
        .unwrap();
    let phoenix = suggestions
        .iter()
        .find(|s| s.canonical_name == "Phoenix")
        .expect("Phoenix in suggestions");
    assert_eq!(
        phoenix.suggested_node_id,
        format!("entity:{entity_id}"),
        "suggested_node_id should be the canonical entity:<uuid> the caller can re-query",
    );
}
