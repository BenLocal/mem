//! Storage layer.
//!
//! The whole layer is LanceDB on disk + DuckDB as a SQL query
//! engine on top via the lance core extension. Single backend; no
//! traits, no abstraction.
//!
//! Layout:
//!
//! - `lance_store::LanceStore` — write half. Holds the
//!   `lancedb::Connection`. Inherent methods cover memory CRUD,
//!   embedding-job queues (memory + transcript), graph edges,
//!   entity registry, transcripts.
//! - `duckdb_query::DuckDbQuery` — read half. Holds an in-process
//!   `duckdb::Connection` with `INSTALL lance; LOAD lance;` +
//!   `ATTACH '<path>' AS ns`. Inherent methods cover all SELECT-
//!   shaped reads plus bulk SQL writes (decay sweep).
//! - `store::Store` — composes the two halves. The handle every
//!   service / worker / HTTP layer carries.
//! - `types` — shared row payloads + `StorageError` + `GraphError`.

pub mod backend;
pub mod capsule_search_store;
pub mod capsule_store;
// The two storage halves are implementation detail. External
// callers go through `Backend` (umbrella) or one of the 9
// sub-traits — never the concrete halves. `pub(crate)` blocks
// cross-crate access; intra-storage modules still see them.
pub(crate) mod duckdb_query;
pub mod embedding_job_store;
pub mod embedding_vector_store;
pub mod entity_registry;
pub mod graph_store;
pub(crate) mod lance_store;
pub mod maintenance_store;
pub mod open_lock;
#[cfg(feature = "postgres")]
pub mod postgres_capsule_store;
pub mod session_store;
pub mod store;
pub mod time;
pub mod transcript_store;
pub mod types;

pub use backend::Backend;
pub use capsule_search_store::CapsuleSearchStore;
pub use capsule_store::{CapsuleStore, InMemoryCapsuleStore};
pub use embedding_job_store::EmbeddingJobStore;
pub use embedding_vector_store::EmbeddingVectorStore;
pub use entity_registry::EntityRegistry;
pub use graph_store::GraphStore;
pub use lance_store::VacuumStats;
pub use maintenance_store::MaintenanceStore;
#[cfg(feature = "postgres")]
pub use postgres_capsule_store::PostgresCapsuleStore;
pub use session_store::SessionStore;
pub use store::Store;
pub use time::{current_timestamp, timestamp_add_ms};
pub use transcript_store::TranscriptStore;
pub use types::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, ContextWindow, EmbeddingJobInsert,
    FeedbackEvent, GraphError, StorageError, TranscriptSessionSummary,
};
