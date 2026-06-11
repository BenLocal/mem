//! Phase 5 — `Backend` umbrella trait. Aggregates every storage
//! sub-trait extracted in Phases 2 + 3 into a single supertrait so
//! services / workers can hold one `Arc<dyn Backend>` instead of one
//! `Arc<dyn CapsuleStore>` + one `Arc<dyn GraphStore>` + … per
//! responsibility.
//!
//! Why a supertrait + blanket impl rather than a struct of `Arc<dyn _>`?
//! - Single `Arc::clone` at handoff instead of N clones.
//! - The blanket impl is conditional on a concrete type implementing
//!   all 9 sub-traits; any backend that wires up the full surface
//!   (today: `Store`; tomorrow: a hypothetical `PostgresBackend` that
//!   bundles `PostgresCapsuleStore` + `PostgresGraphStore` + …) is a
//!   `Backend` without manual ceremony.
//! - Test backends that only implement a subset (`InMemoryCapsuleStore`
//!   today) deliberately do NOT satisfy `Backend` — the parity tests
//!   keep using `Arc<dyn CapsuleStore>` so this is fine.
//!
//! Per doc §3.2 the trait surface is:
//! `Backend: CapsuleStore + CapsuleSearchStore + EmbeddingJobStore +
//!  EmbeddingVectorStore + GraphStore + TranscriptStore +
//!  EntityRegistry + SessionStore + MaintenanceStore + Send + Sync +
//!  'static`. No methods of its own — it exists purely as an alias /
//!  bound.

use super::{
    CapsuleSearchStore, CapsuleStore, EmbeddingJobStore, EmbeddingVectorStore, EntityRegistry,
    EvolutionCandidateStore, GraphStore, MaintenanceStore, MineCursorStore, SessionStore,
    TranscriptStore,
};

/// Backend supertrait — anything that implements all 11 storage
/// sub-traits. See module docs for the rationale.
pub trait Backend:
    CapsuleStore
    + CapsuleSearchStore
    + EmbeddingJobStore
    + EmbeddingVectorStore
    + GraphStore
    + TranscriptStore
    + EntityRegistry
    + SessionStore
    + MaintenanceStore
    + MineCursorStore
    + EvolutionCandidateStore
    + Send
    + Sync
    + 'static
{
}

impl<T> Backend for T where
    T: CapsuleStore
        + CapsuleSearchStore
        + EmbeddingJobStore
        + EmbeddingVectorStore
        + GraphStore
        + TranscriptStore
        + EntityRegistry
        + SessionStore
        + MaintenanceStore
        + MineCursorStore
        + EvolutionCandidateStore
        + Send
        + Sync
        + 'static
{
}
