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
// `lance_store` and `duckdb_query` remain `pub` for now: hiding
// them under `pub(crate)` is sound at the call-site level (all
// external callers go through `Backend` or a sub-trait) but
// surfaces ~20 LanceStore READ methods that became dead when
// DuckDbQuery took over reads. That cleanup is its own commit
// — see doc §6.6 "pub(crate) hiding" tail item.
pub mod duckdb_query;
pub mod embedding_job_store;
pub mod embedding_vector_store;
pub mod entity_registry;
pub mod graph_store;
pub mod lance_store;
pub mod maintenance_store;
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
