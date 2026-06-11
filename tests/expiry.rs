//! Integration tests for hard expiry (`expires_at`) auto-forget.
//!
//! The decay worker's hourly tick (`apply_time_decay`) now archives any
//! Active capsule whose `expires_at` deadline has passed; a `None` expiry
//! (the default) is never touched. Retrieve-time skipping is unit-tested in
//! `src/pipeline/retrieve.rs::tests::rank_skips_expired_candidates`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mem::{
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    storage::Store,
};
use tempfile::tempdir;

const TENANT: &str = "local";

fn ms(n: u128) -> String {
    format!("{n:020}")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn capsule(id: &str, expires_at: Option<String>) -> CapabilityCapsuleRecord {
    let ts = ms(now_ms());
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: TENANT.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Global,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("s-{id}"),
        content: format!("content for {id}"),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.6,
        decay_score: 0.0,
        content_hash: format!("h-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "t".into(),
        created_at: ts.clone(),
        updated_at: ts,
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at,
    }
}

async fn status_of(store: &Store, id: &str) -> CapabilityCapsuleStatus {
    store
        .get_capability_capsule_for_tenant(TENANT, id)
        .await
        .unwrap()
        .expect("present")
        .status
}

#[tokio::test(flavor = "multi_thread")]
async fn decay_tick_archives_expired_and_spares_others() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("exp.lance")).await.unwrap());
    let now = now_ms();

    store
        .insert_capability_capsule(capsule("past", Some(ms(now - 60_000)))) // expired 1 min ago
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule("future", Some(ms(now + 86_400_000)))) // tomorrow
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule("never", None))
        .await
        .unwrap();

    // One decay-worker tick — which now also archives past-expiry rows.
    let now_str = ms(now);
    store
        .apply_time_decay(0.01, now as f64, 86_400_000.0, &now_str)
        .await
        .unwrap();

    assert_eq!(
        status_of(&store, "past").await,
        CapabilityCapsuleStatus::Archived,
        "a past-expiry capsule must be archived"
    );
    assert_eq!(
        status_of(&store, "future").await,
        CapabilityCapsuleStatus::Active,
        "a future-expiry capsule stays active"
    );
    assert_eq!(
        status_of(&store, "never").await,
        CapabilityCapsuleStatus::Active,
        "a capsule with no expiry stays active (default-safe)"
    );
    // Archived verbatim, not deleted — still gettable by id.
    assert!(store
        .get_capability_capsule_for_tenant(TENANT, "past")
        .await
        .unwrap()
        .is_some());
}
