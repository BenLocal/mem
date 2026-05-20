//! Integration tests for the dedup worker (closes mempalace-diff-v3 #30).
//!
//! Three scenarios:
//!   1. Near-duplicate cosine cluster within one (source_agent, project, repo) → shorter members archived.
//!   2. Same near-duplicate pair but in *different* scopes → no archival (groups don't cross).
//!   3. `dry_run=true` returns the would-be archived ids without writing.

use std::sync::Arc;

use mem::{
    config::DedupSettings,
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    storage::Store,
    worker::dedup_worker,
};
use tempfile::tempdir;

const TENANT: &str = "local";
const DIM: usize = 8;

fn f32_to_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

fn capsule(
    id: &str,
    source_agent: &str,
    project: Option<&str>,
    repo: Option<&str>,
    content: &str,
) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: TENANT.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary-{id}"),
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        project: project.map(str::to_string),
        repo: repo.map(str::to_string),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.5,
        decay_score: 0.0,
        content_hash: format!("hash-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: source_agent.into(),
        created_at: "00000000000000000001".into(),
        updated_at: "00000000000000000001".into(),
        last_validated_at: None,
    }
}

async fn seed(store: &Store, c: &CapabilityCapsuleRecord, vector: &[f32]) {
    store
        .insert_capability_capsule(c.clone())
        .await
        .expect("insert");
    store
        .upsert_capability_capsule_embedding(
            &c.capability_capsule_id,
            &c.tenant,
            "fake",
            DIM as i64,
            &f32_to_blob(vector),
            &c.content_hash,
            &c.updated_at,
            "00000000000000000001",
        )
        .await
        .expect("upsert embedding");
}

fn settings(threshold: f32) -> DedupSettings {
    DedupSettings {
        enabled: true,
        interval_secs: 3_600,
        threshold,
        scan_limit: 1_000,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn archives_shorter_member_of_near_duplicate_cluster() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("dedup.lance")).await.unwrap());

    let vec_a = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let vec_close = vec![0.99_f32, 0.01, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

    // Two near-identical capsules in the same (agent, project, repo).
    // `long` has 30 chars of content, `short` has 15 — `short` should be archived.
    let long = capsule(
        "cap_long",
        "test-agent",
        Some("phoenix"),
        Some("mem"),
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", // 30 chars
    );
    let short = capsule(
        "cap_short",
        "test-agent",
        Some("phoenix"),
        Some("mem"),
        "BBBBBBBBBBBBBBB", // 15 chars
    );
    seed(&store, &long, &vec_a).await;
    seed(&store, &short, &vec_close).await;

    let archived = dedup_worker::sweep_once(&*store, &settings(0.9), TENANT, false)
        .await
        .expect("sweep");
    assert_eq!(archived, vec!["cap_short".to_string()]);

    // Verify status transitions.
    let short_after = store
        .get_capability_capsule_for_tenant(TENANT, "cap_short")
        .await
        .unwrap()
        .expect("short still present");
    assert_eq!(short_after.status, CapabilityCapsuleStatus::Archived);
    let long_after = store
        .get_capability_capsule_for_tenant(TENANT, "cap_long")
        .await
        .unwrap()
        .expect("long still present");
    assert_eq!(long_after.status, CapabilityCapsuleStatus::Active);
}

#[tokio::test(flavor = "multi_thread")]
async fn does_not_archive_across_different_scopes() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("dedup.lance")).await.unwrap());

    let vec_close_a = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let vec_close_b = vec![0.999_f32, 0.001, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

    // Same source_agent but different projects — must NOT cluster.
    let a = capsule(
        "cap_a",
        "test-agent",
        Some("alpha"),
        Some("mem"),
        "content one",
    );
    let b = capsule(
        "cap_b",
        "test-agent",
        Some("beta"),
        Some("mem"),
        "content two",
    );
    seed(&store, &a, &vec_close_a).await;
    seed(&store, &b, &vec_close_b).await;

    let archived = dedup_worker::sweep_once(&*store, &settings(0.9), TENANT, false)
        .await
        .expect("sweep");
    assert!(
        archived.is_empty(),
        "different scopes must not cluster, got {archived:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_returns_candidates_without_writing() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("dedup.lance")).await.unwrap());

    let v1 = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let v2 = vec![0.99_f32, 0.01, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let long = capsule(
        "cap_long",
        "test-agent",
        Some("phoenix"),
        Some("mem"),
        "longer content here LONG",
    );
    let short = capsule(
        "cap_short",
        "test-agent",
        Some("phoenix"),
        Some("mem"),
        "short",
    );
    seed(&store, &long, &v1).await;
    seed(&store, &short, &v2).await;

    let preview = dedup_worker::sweep_once(&*store, &settings(0.9), TENANT, true)
        .await
        .expect("dry sweep");
    assert_eq!(preview, vec!["cap_short".to_string()]);

    // Dry run must not flip status.
    let row = store
        .get_capability_capsule_for_tenant(TENANT, "cap_short")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, CapabilityCapsuleStatus::Active);
}

#[tokio::test(flavor = "multi_thread")]
async fn threshold_above_pair_cosine_archives_nothing() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("dedup.lance")).await.unwrap());

    let v1 = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    // Cosine(v1, v2) ≈ 0.707 — clearly under a 0.9 threshold.
    let v2 = vec![1.0_f32, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let a = capsule("cap_a", "agent", Some("phx"), Some("mem"), "AAAAAAAAAA");
    let b = capsule("cap_b", "agent", Some("phx"), Some("mem"), "BBBB");
    seed(&store, &a, &v1).await;
    seed(&store, &b, &v2).await;

    let archived = dedup_worker::sweep_once(&*store, &settings(0.9), TENANT, false)
        .await
        .expect("sweep");
    assert!(
        archived.is_empty(),
        "no pair above threshold should mean no archival: {archived:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn capsules_without_embeddings_are_skipped() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("dedup.lance")).await.unwrap());

    let a = capsule("cap_a", "agent", Some("phx"), Some("mem"), "AAAA");
    let b = capsule("cap_b", "agent", Some("phx"), Some("mem"), "BBBB");
    // Insert capsule rows but NEVER upsert embeddings — worker should
    // skip both (both rows have no vector to compare).
    store.insert_capability_capsule(a.clone()).await.unwrap();
    store.insert_capability_capsule(b.clone()).await.unwrap();

    let archived = dedup_worker::sweep_once(&*store, &settings(0.5), TENANT, false)
        .await
        .expect("sweep");
    assert!(
        archived.is_empty(),
        "no embeddings → nothing to cluster: {archived:?}",
    );
}
