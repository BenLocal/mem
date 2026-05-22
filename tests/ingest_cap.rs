//! Tests for §4.3 #3 per-session ingest cap (MEM_MAX_INGEST_PER_SESSION).
//!
//! Two layers exercised:
//!   - `IngestSettings::from_env_vars` parsing (boundaries: missing,
//!     zero-as-disabled, valid, garbage)
//!   - `CapabilityCapsuleService::ingest` enforcement under cap

use std::sync::Arc;

use mem::{
    config::IngestSettings,
    domain::capability_capsule::{
        CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
    },
    service::CapabilityCapsuleService,
    storage::Store,
};
use tempfile::tempdir;

// ────────────────────────── env parsing ──────────────────────────

fn env_lookup<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k: &str| {
        map.iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| (*v).to_string())
    }
}

#[test]
fn ingest_settings_default_is_unbounded() {
    let s = IngestSettings::from_env_vars(|_| None).unwrap();
    assert_eq!(s.max_per_session, None);
}

#[test]
fn ingest_settings_zero_means_no_cap() {
    // `0` is a footgun guard — operators don't typically want "block
    // all ingest"; reading 0 as None saves a typo from breaking
    // ingest entirely.
    let s =
        IngestSettings::from_env_vars(env_lookup(&[("MEM_MAX_INGEST_PER_SESSION", "0")])).unwrap();
    assert_eq!(s.max_per_session, None);
}

#[test]
fn ingest_settings_positive_set() {
    let s =
        IngestSettings::from_env_vars(env_lookup(&[("MEM_MAX_INGEST_PER_SESSION", "42")])).unwrap();
    assert_eq!(s.max_per_session, Some(42));
}

#[test]
fn ingest_settings_garbage_rejected() {
    let err =
        IngestSettings::from_env_vars(env_lookup(&[("MEM_MAX_INGEST_PER_SESSION", "twelve")]))
            .expect_err("non-numeric env value should reject");
    // Use Debug to inspect the variant — the error message includes
    // the bad input ("twelve") so we don't need to match exact format.
    let msg = format!("{err:?}");
    assert!(
        msg.contains("twelve"),
        "error should carry the bad input verbatim: {msg}",
    );
}

// ────────────────────────── service enforcement ──────────────────────────

async fn make_service(
    max_per_session: Option<usize>,
) -> (tempfile::TempDir, CapabilityCapsuleService) {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("ic.lance")).await.unwrap());
    let svc = CapabilityCapsuleService::new(store)
        .with_ingest_settings(IngestSettings { max_per_session });
    (dir, svc)
}

fn request(content: &str, idempotency_key: Option<&str>) -> IngestCapabilityCapsuleRequest {
    IngestCapabilityCapsuleRequest {
        tenant: "local".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        content: content.into(),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Project,
        visibility: Visibility::Shared,
        project: Some("phoenix".into()),
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test-agent".into(),
        idempotency_key: idempotency_key.map(str::to_string),
        write_mode: WriteMode::Auto,
        supersedes_capability_capsule_id: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cap_none_allows_unlimited_ingests() {
    let (_dir, svc) = make_service(None).await;
    // 5 writes — would exceed any reasonable cap; should all succeed.
    for i in 0..5 {
        let content = format!("test content {i}");
        svc.ingest(request(&content, None))
            .await
            .unwrap_or_else(|e| panic!("ingest {i} should succeed: {e:?}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cap_blocks_after_threshold() {
    let (_dir, svc) = make_service(Some(3)).await;
    // First 3 succeed.
    for i in 0..3 {
        let content = format!("under-cap content {i}");
        svc.ingest(request(&content, None))
            .await
            .unwrap_or_else(|e| panic!("ingest {i} should succeed: {e:?}"));
    }
    // 4th must reject with the cap message.
    let err = svc
        .ingest(request("over-cap content", None))
        .await
        .expect_err("4th ingest should hit cap");
    let msg = format!("{err}");
    assert!(
        msg.contains("per-session ingest cap reached"),
        "error must explain: {msg}",
    );
    assert!(msg.contains("MEM_MAX_INGEST_PER_SESSION"), "{msg}");
}

#[tokio::test(flavor = "multi_thread")]
async fn idempotency_short_circuit_does_not_consume_cap_slot() {
    // The `find_by_idempotency_or_hash` early return runs BEFORE the
    // cap check — so re-ingesting an already-existing capsule should
    // return the existing row without using a slot. Verify: cap=2,
    // ingest a unique row (consumes 1), then re-ingest the SAME row
    // 5 times (each short-circuits), then a second unique row should
    // still succeed.
    let (_dir, svc) = make_service(Some(2)).await;
    svc.ingest(request("first unique", Some("idemp-1")))
        .await
        .expect("first ingest");
    // 5 re-ingests of the same idempotency key — all short-circuit.
    for _ in 0..5 {
        svc.ingest(request("first unique", Some("idemp-1")))
            .await
            .expect("idempotent re-ingest");
    }
    // Cap still has room for 1 more.
    svc.ingest(request("second unique", Some("idemp-2")))
        .await
        .expect("second unique");
    // Third unique should hit the cap.
    svc.ingest(request("third unique", Some("idemp-3")))
        .await
        .expect_err("third unique should hit cap=2");
}
