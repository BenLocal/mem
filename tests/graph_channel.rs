//! G2 — graph as a third retrieval channel (oss-memory-diff §8 G2).
//!
//! Before G2, `rank_with_hybrid_and_graph` could only BOOST capsules
//! already fetched by the pool/hybrid channels; a graph-reachable
//! capsule missing from both (pool capped via `MEM_RECALL_POOL_LIMIT`,
//! or simply not a lexical/semantic match for the query) was
//! unreachable no matter how strong its edges. G2 hydrates
//! boosted-but-missing capsules by id so the graph genuinely GENERATES
//! candidates:
//!   - a `related_to` neighbor (H1 ingest link) of a top hit surfaces
//!     even when absent from the pool and the hybrid hits,
//!   - hydration respects the recall posture: Active + unexpired only,
//!   - `capsules: None` preserves the pre-G2 boost-only behavior.

use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, GraphEdge, Scope,
    Visibility,
};
use mem::domain::query::SearchCapabilityCapsuleRequest;
use mem::pipeline::retrieve::rank_with_hybrid_and_graph;
use mem::storage::{CapsuleStore, GraphStore, Store};
use tempfile::tempdir;

const TENANT: &str = "local";

fn capsule(id: &str, content: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: TENANT.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary of {id}"),
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        project: Some("mem".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.7,
        decay_score: 0.0,
        content_hash: format!("hash-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test".into(),
        created_at: "00000000000000000001".into(),
        updated_at: "00000000000000000001".into(),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    }
}

fn related_to(from: &str, to: &str) -> GraphEdge {
    GraphEdge {
        from_node_id: format!("capability_capsule:{from}"),
        to_node_id: format!("capability_capsule:{to}"),
        relation: "related_to".into(),
        valid_from: "00000000000000000002".into(),
        valid_to: None,
        confidence: Some(0.85),
        extractor: Some("ingest_link".into()),
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    }
}

fn query() -> SearchCapabilityCapsuleRequest {
    SearchCapabilityCapsuleRequest {
        query: "lance write path lesson".into(),
        intent: "debugging".into(),
        scope_filters: vec![],
        token_budget: 4096,
        caller_agent: "test".into(),
        expand_graph: true,
        tenant: Some(TENANT.into()),
        min_score: Some(0),
    }
}

/// The G2 acceptance: a graph neighbor absent from BOTH retrieval
/// channels surfaces through hydration — and stays absent without it.
#[tokio::test(flavor = "multi_thread")]
async fn graph_neighbor_missing_from_pool_surfaces_via_hydration() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("g2.lance")).await.unwrap());
    let seed = capsule("seed", "the lance write path lesson everyone recalls");
    let friend = capsule("friend", "companion note that only the graph reaches");
    let stranger = capsule("stranger", "unrelated bookkeeping trivia");
    for c in [&seed, &friend, &stranger] {
        store.insert_capability_capsule(c.clone()).await.unwrap();
    }
    store
        .add_edge_direct(&related_to("seed", "friend"))
        .await
        .unwrap();

    // Simulate a capped pool: `friend` is in NEITHER the pool nor the
    // hybrid hits — only the graph knows about it.
    let pool = vec![seed.clone(), stranger.clone()];
    let hybrid = vec![(seed.clone(), 0.9_f32)];

    let graph: &dyn GraphStore = store.as_ref();
    let capsules: &dyn CapsuleStore = store.as_ref();

    // Pre-G2 behavior (no hydrator): friend cannot appear.
    let without =
        rank_with_hybrid_and_graph(pool.clone(), hybrid.clone(), &query(), graph, None, None)
            .await
            .unwrap();
    assert!(
        !without.iter().any(|m| m.capability_capsule_id == "friend"),
        "without a hydrator the graph must stay boost-only"
    );

    // G2: hydrated, boosted, present.
    let with = rank_with_hybrid_and_graph(pool, hybrid, &query(), graph, None, Some(capsules))
        .await
        .unwrap();
    assert!(
        with.iter().any(|m| m.capability_capsule_id == "friend"),
        "graph channel must hydrate the boosted neighbor: got {:?}",
        with.iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect::<Vec<_>>()
    );
}

/// Hydration keeps the recall posture: an Archived neighbor and an
/// expired neighbor never surface, no matter their edges.
#[tokio::test(flavor = "multi_thread")]
async fn hydration_respects_active_and_expiry_posture() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("g2b.lance")).await.unwrap());
    let seed = capsule("seed", "the lance write path lesson everyone recalls");
    let mut archived = capsule("archived", "archived companion");
    archived.status = CapabilityCapsuleStatus::Archived;
    let mut expired = capsule("expired", "expired companion");
    expired.expires_at = Some("00000000000000000005".into()); // long past
    for c in [&seed, &archived, &expired] {
        store.insert_capability_capsule(c.clone()).await.unwrap();
    }
    store
        .add_edge_direct(&related_to("seed", "archived"))
        .await
        .unwrap();
    store
        .add_edge_direct(&related_to("seed", "expired"))
        .await
        .unwrap();

    let pool = vec![seed.clone()];
    let hybrid = vec![(seed.clone(), 0.9_f32)];
    let graph: &dyn GraphStore = store.as_ref();
    let capsules: &dyn CapsuleStore = store.as_ref();
    let got = rank_with_hybrid_and_graph(pool, hybrid, &query(), graph, None, Some(capsules))
        .await
        .unwrap();
    let ids: Vec<&str> = got
        .iter()
        .map(|m| m.capability_capsule_id.as_str())
        .collect();
    assert!(
        !ids.contains(&"archived") && !ids.contains(&"expired"),
        "hydration must not resurrect archived/expired rows: {ids:?}"
    );
}

/// Audit 2026-07-03 #4: hydration keeps the POOL's posture, not just
/// Active+unexpired — Diary capsules are excluded from the pool AND the
/// hybrid channels, so the graph channel must not resurrect them into
/// normal recall through an edge.
#[tokio::test(flavor = "multi_thread")]
async fn hydration_excludes_diary_capsules() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("g2c.lance")).await.unwrap());
    let seed = capsule("seed", "the lance write path lesson everyone recalls");
    let mut journal = capsule("journal", "private diary entry about the day");
    journal.capability_capsule_type = CapabilityCapsuleType::Diary;
    for c in [&seed, &journal] {
        store.insert_capability_capsule(c.clone()).await.unwrap();
    }
    store
        .add_edge_direct(&related_to("seed", "journal"))
        .await
        .unwrap();

    let pool = vec![seed.clone()];
    let hybrid = vec![(seed.clone(), 0.9_f32)];
    let graph: &dyn GraphStore = store.as_ref();
    let capsules: &dyn CapsuleStore = store.as_ref();
    let got = rank_with_hybrid_and_graph(pool, hybrid, &query(), graph, None, Some(capsules))
        .await
        .unwrap();
    assert!(
        !got.iter().any(|m| m.capability_capsule_id == "journal"),
        "diary content must never leak into recall via hydration"
    );
}

/// Audit 2026-07-03 #3: the pool drops versions superseded by an Active
/// successor (version-chain dedup); ingest writes an active
/// `supersedes` edge new→old, so WITHOUT the same filter the graph
/// channel deterministically resurrects every replaced fact whose
/// successor ranks top-5.
#[tokio::test(flavor = "multi_thread")]
async fn hydration_does_not_resurrect_superseded_versions() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("g2d.lance")).await.unwrap());
    let old = capsule("old", "the stale fact everyone should forget");
    let mut new = capsule("new", "the corrected lance write path lesson");
    new.supersedes_capability_capsule_id = Some("old".into());
    for c in [&old, &new] {
        store.insert_capability_capsule(c.clone()).await.unwrap();
    }
    // The lineage edge ingest mints on supersede — confidence-less.
    store
        .add_edge_direct(&GraphEdge {
            from_node_id: "capability_capsule:new".into(),
            to_node_id: "capability_capsule:old".into(),
            relation: "supersedes".into(),
            valid_from: "00000000000000000002".into(),
            valid_to: None,
            confidence: None,
            extractor: None,
            strength: None,
            stability: None,
            last_activated: None,
            access_count: None,
        })
        .await
        .unwrap();

    // The pool already deduped `old` away; only the graph knows it.
    let pool = vec![new.clone()];
    let hybrid = vec![(new.clone(), 0.9_f32)];
    let graph: &dyn GraphStore = store.as_ref();
    let capsules: &dyn CapsuleStore = store.as_ref();
    let got = rank_with_hybrid_and_graph(pool, hybrid, &query(), graph, None, Some(capsules))
        .await
        .unwrap();
    assert!(
        !got.iter().any(|m| m.capability_capsule_id == "old"),
        "hydration must not resurrect a version superseded by an Active successor"
    );
}

/// Audit 2026-07-03 #5: the service resolves an omitted tenant to
/// "local" for the pool and hybrid channels — hydration must apply the
/// SAME default instead of silently disabling the graph channel.
#[tokio::test(flavor = "multi_thread")]
async fn hydration_applies_the_local_tenant_default() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("g2e.lance")).await.unwrap());
    let seed = capsule("seed", "the lance write path lesson everyone recalls");
    let friend = capsule("friend", "companion note that only the graph reaches");
    for c in [&seed, &friend] {
        store.insert_capability_capsule(c.clone()).await.unwrap();
    }
    store
        .add_edge_direct(&related_to("seed", "friend"))
        .await
        .unwrap();

    let mut q = query();
    q.tenant = None; // caller relies on the documented "local" default

    let pool = vec![seed.clone()];
    let hybrid = vec![(seed.clone(), 0.9_f32)];
    let graph: &dyn GraphStore = store.as_ref();
    let capsules: &dyn CapsuleStore = store.as_ref();
    let got = rank_with_hybrid_and_graph(pool, hybrid, &q, graph, None, Some(capsules))
        .await
        .unwrap();
    assert!(
        got.iter().any(|m| m.capability_capsule_id == "friend"),
        "omitted tenant must default to local, not disable hydration: got {:?}",
        got.iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect::<Vec<_>>()
    );
}
