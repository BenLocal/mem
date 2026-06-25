//! Storage layer.
//!
//! The whole layer is LanceDB on disk, read and written through the
//! lancedb Rust API. Single backend; no traits, no abstraction.
//!
//! Layout:
//!
//! - `lance_store::LanceStore` — the storage half. Holds the
//!   `lancedb::Connection` (opened with `read_consistency_interval(0)`
//!   so reads see writes natively). Inherent methods cover memory CRUD,
//!   embedding-job queues (memory + transcript), graph edges, entity
//!   registry, transcripts, and all reads.
//! - `store::Store` — composes `LanceStore` with the open-lock and
//!   exposes the method surface every service / worker / HTTP layer
//!   carries.
//! - `types` — shared row payloads + `StorageError` + `GraphError`.

pub mod backend;
pub mod capsule_search_store;
pub mod capsule_store;
// The storage half (`LanceStore`) is an implementation detail. External
// callers go through `Backend` (umbrella) or one of the 9 sub-traits —
// never the concrete type. `pub(crate)` blocks cross-crate access;
// intra-storage modules still see it.
#[cfg(feature = "clickhouse")]
pub mod clickhouse_store;
pub mod embedding_job_store;
pub mod embedding_vector_store;
pub mod entity_registry;
pub mod evolution_candidate_store;
// Route-B Tantivy full-text (BM25) subsystem replacing the DuckDB
// `lance_fts` read path for capsule search. See
// `docs/remove-duckdb-keep-lance.md` §4.
pub(crate) mod fts;
pub mod graph_store;
pub(crate) mod lance_store;
pub mod maintenance_store;
pub mod mine_cursor_store;
pub mod open_lock;
#[cfg(feature = "postgres")]
pub mod postgres_store;
pub mod session_store;
pub mod store;
pub mod time;
pub mod transcript_store;
pub mod types;

pub use backend::Backend;
pub use capsule_search_store::CapsuleSearchStore;
pub use capsule_store::{CapsuleStore, InMemoryCapsuleStore};
#[cfg(feature = "clickhouse")]
pub use clickhouse_store::ClickHouseBackend;
pub use embedding_job_store::EmbeddingJobStore;
pub use embedding_vector_store::EmbeddingVectorStore;
pub use entity_registry::EntityRegistry;
pub use evolution_candidate_store::{EvolutionCandidate, EvolutionCandidateStore};
pub use graph_store::GraphStore;
pub use lance_store::{IndexMaintenanceStats, VacuumStats};
pub use maintenance_store::MaintenanceStore;
pub use mine_cursor_store::{MineCursor, MineCursorStore};
#[cfg(feature = "postgres")]
pub use postgres_store::PostgresCapsuleStore;
pub use session_store::SessionStore;
pub use store::Store;
pub use time::{current_timestamp, timestamp_add_ms, timestamp_sub_ms};

/// Visibility-timeout (lease) for an embedding job claimed into the
/// `processing` state. A job that has been `processing` longer than this is
/// treated as **orphaned** — its worker crashed, the process restarted
/// mid-embed, or a mid-batch error abandoned it — and the next claim is
/// allowed to reclaim it. Without this, an orphaned `processing` row is never
/// re-picked (the claim filter only matches `pending`/`failed`) and the
/// capsule silently loses its embedding forever. 5 minutes is far above the
/// real embed latency (~100ms–1s) so it never steals a genuinely in-flight
/// job, while still reclaiming orphans promptly.
pub const EMBEDDING_JOB_LEASE_MS: u128 = 300_000;
pub use transcript_store::TranscriptStore;
pub use types::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, ContextWindow, EmbeddingJobInsert,
    FeedbackEvent, GraphError, StorageError, TranscriptSessionSummary,
};
