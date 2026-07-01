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
    service::{
        capability_capsule_service::{BatchIngestItem, ServiceError},
        CapabilityCapsuleService,
    },
    storage::{StorageError, Store},
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
    let svc = CapabilityCapsuleService::new(store).with_ingest_settings(IngestSettings {
        max_per_session,
        ..IngestSettings::development_defaults()
    });
    (dir, svc)
}

/// Service with the Step-3 source quality gate enabled (default
/// `min_content_len`), used by the gate-wiring tests below.
async fn make_gated_service() -> (tempfile::TempDir, CapabilityCapsuleService) {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("ig.lance")).await.unwrap());
    let svc = CapabilityCapsuleService::new(store).with_ingest_settings(IngestSettings {
        max_per_session: None,
        quality_gate_enabled: true,
        min_content_len: 40,
    });
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
        expires_at: None,
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
    // The cap is a rate limit, not a malformed request: must be the
    // RateLimited variant so `error.rs` maps it to HTTP 429 (not 400).
    assert!(
        matches!(err, ServiceError::Storage(StorageError::RateLimited(_))),
        "cap rejection must be RateLimited (→ HTTP 429), got: {err:?}"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn cap_enforced_on_batch_path() {
    // The batch path (`/capability_capsules/batch`, MCP `batch_ingest`, and the
    // route `mem mine` uses) must honor the per-session cap too — otherwise
    // MEM_MAX_INGEST_PER_SESSION is silently defeated by the dominant bulk route.
    let (_dir, svc) = make_service(Some(2)).await;
    let batch = vec![
        request("batch a", None),
        request("batch b", None),
        request("batch c", None),
        request("batch d", None),
    ];
    let results = svc.ingest_batch(batch).await.expect("batch call");
    assert_eq!(results.len(), 4, "output preserves input order 1:1");

    let ok = results
        .iter()
        .filter(|r| matches!(r, BatchIngestItem::Ok { .. }))
        .count();
    let errs: Vec<&String> = results
        .iter()
        .filter_map(|r| match r {
            BatchIngestItem::Err { error } => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(ok, 2, "cap=2 → exactly 2 items may land");
    assert_eq!(
        errs.len(),
        2,
        "the 2 over-cap items must be rejected per-item"
    );
    for e in errs {
        assert!(
            e.contains("per-session ingest cap reached"),
            "over-cap rejection must be the cap error (→ 429), got: {e}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_idempotent_items_do_not_consume_cap_slots() {
    // Idempotent re-ingests short-circuit before the cap check on the batch
    // path too (mirrors the single-item guarantee): the existing-row probe
    // returns early, so a slot is never reserved for a dup.
    let (_dir, svc) = make_service(Some(2)).await;
    svc.ingest(request("u1", Some("idemp-1")))
        .await
        .expect("first single ingest consumes slot 1/2");

    // Batch: re-ingest u1 (existing → no slot), u2 (new → slot 2/2),
    // u3 (new → over cap).
    let results = svc
        .ingest_batch(vec![
            request("u1", Some("idemp-1")),
            request("u2", Some("idemp-2")),
            request("u3", Some("idemp-3")),
        ])
        .await
        .expect("batch call");
    assert!(
        matches!(results[0], BatchIngestItem::Ok { .. }),
        "idempotent re-ingest returns the existing row: {:?}",
        results[0]
    );
    assert!(
        matches!(results[1], BatchIngestItem::Ok { .. }),
        "the one remaining slot admits u2: {:?}",
        results[1]
    );
    assert!(
        matches!(results[2], BatchIngestItem::Err { .. }),
        "u3 must hit the cap: {:?}",
        results[2]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_intra_batch_idempotency_dedupes_within_one_call() {
    // Two items in the SAME batch that share an idempotency_key must dedupe
    // against each other, exactly as the single-item path dedupes on
    // (tenant, idempotency_key)/content_hash. The old batch path probed storage
    // for every item BEFORE any insert, so neither item saw the other and both
    // landed as separate rows. Assert only ONE row is created: both results
    // carry the same capability_capsule_id, and the single cap slot is consumed
    // just once (a following unique ingest still fits under cap=2).
    let (_dir, svc) = make_service(Some(2)).await;
    let results = svc
        .ingest_batch(vec![
            request("intra-batch dup A", Some("dup-key")),
            request("intra-batch dup B", Some("dup-key")),
        ])
        .await
        .expect("batch call");

    let id0 = match &results[0] {
        BatchIngestItem::Ok { response } => response.capability_capsule_id.clone(),
        other => panic!("item 0 must be Ok: {other:?}"),
    };
    let id1 = match &results[1] {
        BatchIngestItem::Ok { response } => response.capability_capsule_id.clone(),
        other => panic!("item 1 must be Ok: {other:?}"),
    };
    assert_eq!(
        id0, id1,
        "intra-batch idempotent items must resolve to a single row, not two"
    );

    // Only one slot was consumed (dedup didn't reserve a second), so a distinct
    // capsule still fits, and the one after that hits cap=2.
    svc.ingest(request("distinct after dedup", Some("k2")))
        .await
        .expect("second distinct row fits: dedup consumed only one slot");
    svc.ingest(request("over cap", Some("k3")))
        .await
        .expect_err("third distinct row must hit cap=2");
}

// ────────────────────── Step-3 quality gate (wiring) ──────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn quality_gate_rejects_bare_experience_at_ingest() {
    let (_dir, svc) = make_gated_service().await;
    // A short single-line experience with no evidence / code_refs — the
    // exact shape of a per-commit subject. Must be refused with a reason.
    let err = svc
        .ingest(request("fix login bug", None))
        .await
        .expect_err("bare experience must be rejected by the gate");
    let msg = format!("{err}");
    assert!(
        msg.contains("too short") || msg.contains("commit subject"),
        "gate error must explain why: {msg}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn quality_gate_default_off_accepts_bare_experience() {
    // Default service (gate disabled) must keep accepting the same capsule —
    // the gate is strictly opt-in, no behavior change by default.
    let (_dir, svc) = make_service(None).await;
    svc.ingest(request("fix login bug", None))
        .await
        .expect("with gate off, ingest is unchanged");
}

#[tokio::test(flavor = "multi_thread")]
async fn quality_gate_accepts_substantive_experience() {
    let (_dir, svc) = make_gated_service().await;
    // A real lesson with a body — comfortably over the floor, multi-line.
    let lesson = "Decay sweep overwrote last_used_at every hour, destroying the \
                  recall signal.\nFix: add a separate last_recalled_at column.";
    svc.ingest(request(lesson, None))
        .await
        .expect("substantive experience must pass the gate");
}
