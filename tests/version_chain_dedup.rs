//! Version-chain dedup at retrieve (closes strategy-readiness §4.4 #3).
//!
//! `supersede` creates a new active row with
//! `supersedes_capability_capsule_id` pointing at the prior version,
//! but leaves the prior row Active too (verbatim philosophy: never
//! mutate). Without dedup, every search returned BOTH versions.
//!
//! The SQL fix lives in `duckdb_query::search_candidates` +
//! `hybrid_candidates`: a `NOT EXISTS` correlated subquery drops any
//! capsule that's been superseded by another *active* row in the same
//! tenant. These tests exercise the resulting behavior end-to-end.

use std::sync::Arc;

use mem::{
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    storage::Store,
};
use tempfile::tempdir;

const TENANT: &str = "local";

fn capsule(id: &str, content: &str, supersedes: Option<&str>) -> CapabilityCapsuleRecord {
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
        project: Some("dedup-test".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: format!("hash-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: supersedes.map(str::to_string),
        source_agent: "test".into(),
        // Newer rows get bigger timestamps; the SQL sort uses these
        // for tiebreak ordering.
        created_at: format!("0000000000000000{:04}", id.len()),
        updated_at: format!("0000000000000000{:04}", id.len()),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn single_supersede_drops_old_version_from_search_candidates() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("vc.lance")).await.unwrap());

    // A is the original; B supersedes A; C is unrelated.
    store
        .insert_capability_capsule(capsule("cap_a", "alpha alpha alpha", None))
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule(
            "cap_b_supersedes_a",
            "alpha alpha alpha v2",
            Some("cap_a"),
        ))
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule("cap_c_unrelated", "gamma gamma gamma", None))
        .await
        .unwrap();

    let pool = store.search_candidates(TENANT).await.unwrap();
    let ids: Vec<&str> = pool
        .iter()
        .map(|m| m.capability_capsule_id.as_str())
        .collect();

    assert!(
        !ids.contains(&"cap_a"),
        "old version should be suppressed by NOT EXISTS clause: got {ids:?}",
    );
    assert!(ids.contains(&"cap_b_supersedes_a"), "got {ids:?}");
    assert!(ids.contains(&"cap_c_unrelated"), "got {ids:?}");
    assert_eq!(pool.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn chained_supersede_keeps_only_the_newest() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("vc.lance")).await.unwrap());

    // Chain: A → B → C, all active.
    store
        .insert_capability_capsule(capsule("cap_a", "alpha alpha alpha", None))
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule(
            "cap_b_supersedes_a",
            "alpha alpha alpha v2",
            Some("cap_a"),
        ))
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule(
            "cap_c_supersedes_b",
            "alpha alpha alpha v3",
            Some("cap_b_supersedes_a"),
        ))
        .await
        .unwrap();

    let pool = store.search_candidates(TENANT).await.unwrap();
    let ids: Vec<&str> = pool
        .iter()
        .map(|m| m.capability_capsule_id.as_str())
        .collect();

    assert_eq!(
        ids,
        vec!["cap_c_supersedes_b"],
        "only the tail of the chain should surface: got {ids:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_superseder_does_not_suppress_old_version() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("vc.lance")).await.unwrap());

    // A is the original; B was meant to supersede A but got Archived
    // (caller rejected the proposed update). A should still surface —
    // an archived superseder doesn't count for dedup, otherwise we'd
    // lose data when callers roll back a proposed update.
    store
        .insert_capability_capsule(capsule("cap_a", "alpha original", None))
        .await
        .unwrap();
    let mut b = capsule("cap_b_archived", "alpha v2 rejected", Some("cap_a"));
    b.status = CapabilityCapsuleStatus::Archived;
    store.insert_capability_capsule(b).await.unwrap();

    let pool = store.search_candidates(TENANT).await.unwrap();
    let ids: Vec<&str> = pool
        .iter()
        .map(|m| m.capability_capsule_id.as_str())
        .collect();

    // cap_b is archived → filtered out by status NOT IN clause.
    // cap_a is NOT suppressed (the only candidate superseder is
    // archived, which the active-only correlated subquery ignores).
    assert_eq!(ids, vec!["cap_a"], "got {ids:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn supersede_across_tenants_does_not_suppress() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("vc.lance")).await.unwrap());

    // The supersede flow is single-tenant by design, but the SQL
    // correlation explicitly requires `s.tenant = m.tenant` to be
    // safe against cross-tenant pollution. Construct a degenerate case
    // (B in tenant t2 claims to supersede A in tenant t1) and verify
    // A in t1 still surfaces.
    let mut a = capsule("cap_a", "alpha", None);
    a.tenant = "t1".into();
    let mut b_t2 = capsule("cap_b_t2", "alpha v2", Some("cap_a"));
    b_t2.tenant = "t2".into();

    store.insert_capability_capsule(a).await.unwrap();
    store.insert_capability_capsule(b_t2).await.unwrap();

    let t1_pool = store.search_candidates("t1").await.unwrap();
    let t1_ids: Vec<&str> = t1_pool
        .iter()
        .map(|m| m.capability_capsule_id.as_str())
        .collect();
    assert!(
        t1_ids.contains(&"cap_a"),
        "cross-tenant superseder must not suppress: got {t1_ids:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_capability_capsule_ids_still_returns_superseded_rows() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("vc.lance")).await.unwrap());

    store
        .insert_capability_capsule(capsule("cap_a", "alpha", None))
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule("cap_b_supersedes_a", "alpha v2", Some("cap_a")))
        .await
        .unwrap();

    // Browsing path (admin / audit) — superseded rows MUST still be
    // visible so operators can see the full version chain.
    let ids = store
        .list_capability_capsule_ids_for_tenant(TENANT)
        .await
        .unwrap();
    assert!(
        ids.contains(&"cap_a".to_string()),
        "list_capability_capsule_ids must NOT apply version-chain dedup (admin path): got {ids:?}",
    );
    assert!(
        ids.contains(&"cap_b_supersedes_a".to_string()),
        "got {ids:?}"
    );
}
