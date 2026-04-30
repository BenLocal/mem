//! Shared test helpers for HTTP integration tests.
//!
//! The single point of truth for assembling an [`AppState`] in tests.
//! Each test file picks the [`MemoryService`] flavor it needs (e.g.
//! `MemoryService::new`, `new_with_graph`, `with_graph_and_embedding_providers`)
//! and hands the constructed service to [`test_app_state`]; the helper then
//! fills in the rest of the `AppState` plumbing — currently the in-memory
//! `transcript_index` placeholder.
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

use mem::{app::AppState, service::MemoryService, storage::VectorIndex};

/// Builds an [`AppState`] suitable for integration tests.
///
/// The caller picks the [`MemoryService`] flavor it needs — plain `new`,
/// `new_with_graph`, `with_graph_and_embedding_providers`, etc. — and this
/// helper assembles the rest of the `AppState`. Currently that means the
/// `transcript_index` placeholder; future fields land here too so individual
/// test suites don't need to be edited.
pub fn test_app_state(memory_service: MemoryService) -> AppState {
    AppState {
        memory_service,
        config: mem::config::Config::local(),
        transcript_index: Arc::new(VectorIndex::new_in_memory(8, "fake", "fake", 8)),
    }
}
