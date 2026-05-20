//! Shared test helpers for HTTP integration tests.
//!
//! The single point of truth for assembling an [`AppState`] in tests.
//! Each test file calls `common::test_app_state(store, memory_service)`
//! with a `CapabilityCapsuleService` flavor it wants (typically
//! `CapabilityCapsuleService::new(store.clone())`); this helper assembles the
//! `EntityService` + `TranscriptService` + `Config` plumbing.
//!
//! Centralising the boilerplate here means future `AppState` field
//! additions only need a one-line edit in this module instead of an
//! N-file mechanical sweep.
//!
//! NOTE: this module deliberately uses `#[allow(dead_code)]` on
//! every public item — Cargo compiles `tests/common/mod.rs`
//! separately for each `tests/foo.rs` that pulls it in via
//! `mod common;`, and any helper not used by a particular suite
//! would otherwise produce a dead-code warning.

#![allow(dead_code)]

use std::sync::Arc;

use mem::{
    app::AppState,
    service::{CapabilityCapsuleService, EntityService, FactCheckService, TranscriptService},
    storage::Store,
};
use tempfile::TempDir;

/// Open a fresh [`Store`] under a tempdir. Returned `TempDir` must be
/// kept alive (and dropped only after the store) — `Store` holds open
/// handles inside the tempdir.
pub async fn test_store() -> (TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mem.lance");
    let store = Store::open(&path).await.expect("Store::open");
    // `set_transcript_job_provider` is required before any
    // embed-eligible `create_conversation_message` write — most
    // tests don't write transcripts but the call is cheap and idempotent.
    store.set_transcript_job_provider("fake");
    (dir, Arc::new(store))
}

/// Builds an [`AppState`] suitable for integration tests.
///
/// The caller passes the open [`Store`] handle and the
/// [`CapabilityCapsuleService`] flavor under test. The helper assembles the
/// transcript-side and entity-side service façades and the default
/// `Config::local()` so request handlers have a complete state.
pub fn test_app_state(
    store: Arc<Store>,
    capability_capsule_service: CapabilityCapsuleService,
) -> AppState {
    let transcript_service = Arc::new(TranscriptService::new(store.clone(), None));
    let entity_service = EntityService::new(store.clone());
    let fact_check_service = FactCheckService::new(store);
    AppState {
        capability_capsule_service,
        config: mem::config::Config::local(),
        transcript_service,
        entity_service,
        fact_check_service,
    }
}
