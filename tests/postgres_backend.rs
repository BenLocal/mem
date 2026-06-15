//! Postgres backend integration tests (postgres-backend.md P1).
//!
//! Gated on the `postgres` cargo feature AND a reachable test database:
//! every test reads `MEM_TEST_POSTGRES_URL` and **skips** (prints +
//! returns) when it is unset, so the default `cargo test` (no feature,
//! no DB) stays green and CI's `rust` job is unaffected. To run:
//!
//! ```bash
//! docker run -d --name mem-pg -e POSTGRES_PASSWORD=mem -e POSTGRES_DB=mem \
//!   -p 5433:5432 pgvector/pgvector:pg16
//! MEM_TEST_POSTGRES_URL=postgres://postgres:mem@127.0.0.1:5433/mem \
//!   cargo test --features postgres --test postgres_backend
//! ```
//!
//! P1 validates the existing `PostgresCapsuleStore` scaffold (the
//! `CapsuleStore` trait) against a real Postgres — the Phase-4 spike
//! validation that the doc said "needs Docker + testcontainers" and
//! never ran. Cases mirror `capsule_store_parity.rs`.
#![cfg(feature = "postgres")]

use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
};
use mem::storage::{current_timestamp, CapsuleStore, FeedbackEvent, PostgresCapsuleStore};

/// `Some(store)` on a fresh schema when `MEM_TEST_POSTGRES_URL` is set,
/// else `None` (caller skips). Each call drops + re-applies the schema
/// so tests are order-independent.
async fn backend() -> Option<Arc<dyn CapsuleStore>> {
    let url = std::env::var("MEM_TEST_POSTGRES_URL").ok()?;
    let store = PostgresCapsuleStore::connect_fresh(&url)
        .await
        .expect("connect + migrate test postgres");
    Some(Arc::new(store))
}

macro_rules! pg_test {
    ($name:ident, $backend:ident, $body:block) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some($backend) = backend().await else {
                eprintln!("skip {}: MEM_TEST_POSTGRES_URL unset", stringify!($name));
                return;
            };
            $body
        }
    };
}

fn fixture(id: &str, status: CapabilityCapsuleStatus) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "t".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status,
        scope: Scope::Repo,
        visibility: Visibility::Private,
        version: 1,
        summary: format!("summary-{id}"),
        content: format!("content-{id}"),
        content_hash: format!("{id:0>64}"),
        confidence: 0.5,
        decay_score: 0.0,
        source_agent: "test".into(),
        created_at: "00000000000000000000".into(),
        updated_at: "00000000000000000000".into(),
        ..Default::default()
    }
}

pg_test!(insert_and_get_round_trip, backend, {
    let row = fixture("a", CapabilityCapsuleStatus::Active);
    backend
        .insert_capability_capsule(row.clone())
        .await
        .unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("t", "a")
        .await
        .unwrap()
        .expect("tenant-scoped get should find the row");
    assert_eq!(got.capability_capsule_id, "a");
    assert_eq!(got.tenant, "t");
    assert_eq!(got.status, CapabilityCapsuleStatus::Active);
    assert_eq!(got.content, "content-a");
});

pg_test!(get_for_other_tenant_returns_none, backend, {
    backend
        .insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("other", "a")
        .await
        .unwrap();
    assert!(got.is_none(), "cross-tenant get must not leak");
});

pg_test!(accept_pending_transitions_status, backend, {
    backend
        .insert_capability_capsule(fixture("p", CapabilityCapsuleStatus::PendingConfirmation))
        .await
        .unwrap();
    backend.accept_pending("t", "p").await.unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("t", "p")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.status, CapabilityCapsuleStatus::Active);
});

pg_test!(list_pending_review_filters_status, backend, {
    backend
        .insert_capability_capsule(fixture("act", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    backend
        .insert_capability_capsule(fixture(
            "pend",
            CapabilityCapsuleStatus::PendingConfirmation,
        ))
        .await
        .unwrap();
    let pending = backend.list_pending_review("t").await.unwrap();
    let ids: Vec<&str> = pending
        .iter()
        .map(|c| c.capability_capsule_id.as_str())
        .collect();
    assert_eq!(ids, vec!["pend"], "only PendingConfirmation rows listed");
});

pg_test!(find_by_idempotency_dedups_on_hash, backend, {
    let mut row = fixture("h", CapabilityCapsuleStatus::Active);
    row.content_hash = "deadbeef".into();
    backend.insert_capability_capsule(row).await.unwrap();
    let hit = backend
        .find_by_idempotency_or_hash("t", &None, "deadbeef")
        .await
        .unwrap();
    assert!(hit.is_some(), "existing content_hash should dedup");
    let miss = backend
        .find_by_idempotency_or_hash("t", &None, "00000000")
        .await
        .unwrap();
    assert!(miss.is_none(), "unknown hash is not a dup");
});

pg_test!(apply_feedback_moves_confidence, backend, {
    let row = fixture("f", CapabilityCapsuleStatus::Active);
    backend
        .insert_capability_capsule(row.clone())
        .await
        .unwrap();
    let before = backend
        .get_capability_capsule_for_tenant("t", "f")
        .await
        .unwrap()
        .unwrap()
        .confidence;
    let event = FeedbackEvent {
        feedback_id: "fb_1".into(),
        capability_capsule_id: "f".into(),
        feedback_kind: "useful".into(),
        created_at: current_timestamp(),
        note: None,
    };
    backend.apply_feedback(&row, event).await.unwrap();
    let after = backend
        .get_capability_capsule_for_tenant("t", "f")
        .await
        .unwrap()
        .unwrap()
        .confidence;
    assert!(
        after > before,
        "useful feedback raises confidence ({before} -> {after})"
    );
});

/// P2 smoke test: `mem serve` boots on the Postgres backend. Builds a
/// `Config` with `backend = Postgres` + a Fake embedding provider (so no
/// model download) and asserts `AppState::from_config` assembles cleanly
/// — i.e. the Postgres arm of `app.rs` connects, migrates, wires up the
/// services/workers, and never hits an `unimplemented!()` stub on the
/// startup path. Skips when `MEM_TEST_POSTGRES_URL` is unset.
#[tokio::test(flavor = "multi_thread")]
async fn serve_boots_on_postgres() {
    let Some(url) = std::env::var("MEM_TEST_POSTGRES_URL").ok() else {
        eprintln!("skip serve_boots_on_postgres: MEM_TEST_POSTGRES_URL unset");
        return;
    };
    let mut config = mem::config::Config::local();
    config.backend = mem::config::BackendKind::Postgres;
    config.postgres_url = Some(url);
    config.embedding.provider = mem::config::EmbeddingProviderKind::Fake;
    config.embedding.model = "fake".to_string();
    config.embedding.dim = 64;

    let state = mem::app::AppState::from_config(config).await;
    assert!(
        state.is_ok(),
        "AppState::from_config should assemble on the Postgres backend: {:?}",
        state.err()
    );
}
