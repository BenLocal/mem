//! Shared test helpers for HTTP integration tests.
//!
//! The single point of truth for assembling an [`AppState`] in tests.
//! Each test file picks the [`MemoryService`] flavor it needs (e.g.
//! `MemoryService::new`, `new_with_graph`, `with_graph_and_embedding_providers`)
//! and hands the repo + constructed service to [`test_app_state`]; the helper
//! then fills in the rest of the `AppState` plumbing — currently the in-memory
//! `transcript_index` placeholder and a no-provider [`TranscriptService`].
//!
//! Centralising the boilerplate here means future `AppState` field additions
//! only need a one-line edit in this module instead of an N-file mechanical
//! sweep across every integration test.
//!
//! NOTE: this module deliberately uses `#[allow(dead_code)]` on every public
//! item — Cargo compiles `tests/common/mod.rs` separately for each
//! `tests/foo.rs` that pulls it in via `mod common;`, and any helper not used
//! by a particular suite would otherwise produce a dead-code warning.

#![allow(dead_code)]

use std::sync::Arc;

use mem::{
    app::AppState,
    service::{MemoryService, TranscriptService},
    storage::{DuckDbRepository, VectorIndex},
};

/// Builds an [`AppState`] suitable for integration tests.
///
/// The caller passes the open [`DuckDbRepository`] (already seeded as needed)
/// and the [`MemoryService`] flavor under test. The helper provides the
/// transcript-side pieces: an 8-dim in-memory `VectorIndex` placeholder and
/// a [`TranscriptService`] with no embedding provider attached. Tests that
/// need a real-sized index or a live provider should bypass this helper and
/// construct `AppState` directly.
pub fn test_app_state(repo: DuckDbRepository, memory_service: MemoryService) -> AppState {
    let transcript_index = Arc::new(VectorIndex::new_in_memory(8, "fake", "fake", 8));
    let transcript_service = TranscriptService::new(repo, transcript_index.clone(), None);
    AppState {
        memory_service,
        config: mem::config::Config::local(),
        transcript_index,
        transcript_service,
    }
}
