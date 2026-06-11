//! Integration tests for the idle-archive worker (governance Step 2).
//!
//! The sweep archives `Active` capsules that have ALL of:
//!   - never been recalled since creation (`last_recalled_at IS NULL`,
//!     the sweep-proof signal added in Step 1),
//!   - aged past `age_days` (by `created_at`),
//!   - never positively reinforced (`confidence <= default_confidence`),
//!   - decayed past `decay_threshold`.
//!
//! Everything failing ANY clause is preserved. Archival reuses the same
//! `apply_feedback(Incorrect)` path as dedup, so the row is kept verbatim.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mem::{
    config::IdleArchiveSettings,
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    storage::Store,
    worker::idle_archive_worker,
};
use tempfile::tempdir;

const TENANT: &str = "local";
const MS_PER_DAY: u128 = 86_400_000;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn ms_string(ms: u128) -> String {
    format!("{ms:020}")
}

/// Active experience capsule with controllable age / confidence / decay.
fn capsule(id: &str, created_ms: u128, confidence: f32, decay: f32) -> CapabilityCapsuleRecord {
    let ts = ms_string(created_ms);
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: TENANT.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary-{id}"),
        content: format!("content body for {id}"),
        evidence: vec![],
        code_refs: vec![],
        project: Some("mem".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence,
        decay_score: decay,
        content_hash: format!("hash-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test-agent".into(),
        created_at: ts.clone(),
        updated_at: ts,
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    }
}

fn settings() -> IdleArchiveSettings {
    IdleArchiveSettings {
        enabled: true,
        interval_secs: 86_400,
        age_days: 30,
        decay_threshold: 0.5,
        default_confidence: 0.6,
        min_content_len: 40,
        scan_limit: 1_000,
    }
}

/// Seed the five-capsule fixture. Returns the store. Only `idle` should
/// ever be a candidate; the other four each fail exactly one clause.
async fn seed_fixture(store: &Store) {
    let now = now_ms();
    let old = now - 40 * MS_PER_DAY; // older than age_days=30
    let young = now - MS_PER_DAY; // younger than age_days

    // The lone candidate: old, never recalled, default confidence, high decay.
    store
        .insert_capability_capsule(capsule("idle", old, 0.6, 0.6))
        .await
        .unwrap();
    // Recalled at least once → must be spared (Step-1 signal in action).
    store
        .insert_capability_capsule(capsule("recalled", old, 0.6, 0.6))
        .await
        .unwrap();
    store
        .bump_last_used_at(TENANT, &["recalled".to_string()], &ms_string(now))
        .await
        .unwrap();
    // Too young.
    store
        .insert_capability_capsule(capsule("young", young, 0.6, 0.6))
        .await
        .unwrap();
    // Positively reinforced (confidence above default).
    store
        .insert_capability_capsule(capsule("reinforced", old, 0.8, 0.6))
        .await
        .unwrap();
    // Decay below threshold.
    store
        .insert_capability_capsule(capsule("fresh_decay", old, 0.6, 0.1))
        .await
        .unwrap();
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
async fn dry_run_previews_only_idle_and_writes_nothing() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("idle.lance")).await.unwrap());
    seed_fixture(&store).await;

    let preview = idle_archive_worker::sweep_once(&*store, &settings(), TENANT, true)
        .await
        .expect("dry sweep");
    assert_eq!(
        preview,
        vec!["idle".to_string()],
        "only the old/never-recalled/default-confidence/high-decay capsule qualifies"
    );

    // Dry run must not flip any status.
    for id in ["idle", "recalled", "young", "reinforced", "fresh_decay"] {
        assert_eq!(
            status_of(&store, id).await,
            CapabilityCapsuleStatus::Active,
            "dry_run must not archive {id}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn archives_idle_and_preserves_everything_else() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("idle.lance")).await.unwrap());
    seed_fixture(&store).await;

    let archived = idle_archive_worker::sweep_once(&*store, &settings(), TENANT, false)
        .await
        .expect("sweep");
    assert_eq!(archived, vec!["idle".to_string()]);

    assert_eq!(
        status_of(&store, "idle").await,
        CapabilityCapsuleStatus::Archived,
        "idle capsule must be archived"
    );
    // The row is kept verbatim (archived, not deleted).
    assert!(store
        .get_capability_capsule_for_tenant(TENANT, "idle")
        .await
        .unwrap()
        .is_some());
    for id in ["recalled", "young", "reinforced", "fresh_decay"] {
        assert_eq!(
            status_of(&store, id).await,
            CapabilityCapsuleStatus::Active,
            "{id} must be preserved"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn spares_substantive_idle_experience() {
    // The misfire-fix test: two capsules identical on EVERY idle axis
    // (old, never recalled, default confidence, high decay) — one is a
    // short bare line (structurally junk), the other a long structured
    // lesson. Only the junk one may be archived; the substantive one is
    // spared by clause 5.
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("idle.lance")).await.unwrap());
    let now = now_ms();
    let old = now - 40 * MS_PER_DAY;

    store
        .insert_capability_capsule(capsule("junk", old, 0.6, 0.6))
        .await
        .unwrap();
    let mut good = capsule("good", old, 0.6, 0.6);
    good.content = format!(
        "Symptom: the idle sweep mis-archived substantive memories.\n\
         Cause: recall + feedback signals were blank retroactively.\n{}",
        "Fix detail line. ".repeat(20)
    );
    store.insert_capability_capsule(good).await.unwrap();

    let preview = idle_archive_worker::sweep_once(&*store, &settings(), TENANT, true)
        .await
        .expect("dry sweep");
    assert_eq!(
        preview,
        vec!["junk".to_string()],
        "only the structurally low-value capsule is a candidate; the long lesson is spared"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_settings_archive_nothing() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("idle.lance")).await.unwrap());
    seed_fixture(&store).await;

    let mut off = settings();
    off.enabled = false;

    // Destructive path is gated on the master switch: a real run while
    // disabled archives NOTHING (safety — operator must opt in first).
    let archived = idle_archive_worker::sweep_once(&*store, &off, TENANT, false)
        .await
        .expect("sweep");
    assert!(
        archived.is_empty(),
        "a disabled real sweep must archive nothing, got {archived:?}"
    );
    assert_eq!(
        status_of(&store, "idle").await,
        CapabilityCapsuleStatus::Active
    );

    // But dry-run PREVIEW works regardless of the switch — the whole point
    // is to inspect what would be archived before flipping it on.
    let preview = idle_archive_worker::sweep_once(&*store, &off, TENANT, true)
        .await
        .expect("dry sweep while disabled");
    assert_eq!(
        preview,
        vec!["idle".to_string()],
        "dry-run preview must work even when the worker is disabled"
    );
    assert_eq!(
        status_of(&store, "idle").await,
        CapabilityCapsuleStatus::Active,
        "preview must still not write"
    );
}
