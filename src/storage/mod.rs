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

pub mod capsule_search_store;
pub mod capsule_store;
pub mod duckdb_query;
pub mod embedding_job_store;
pub mod embedding_vector_store;
pub mod entity_registry;
pub mod graph_store;
pub mod lance_store;
pub mod maintenance_store;
pub mod session_store;
pub mod store;
pub mod time;
pub mod transcript_store;
pub mod types;

pub use capsule_search_store::CapsuleSearchStore;
pub use capsule_store::{CapsuleStore, InMemoryCapsuleStore};
pub use embedding_job_store::EmbeddingJobStore;
pub use embedding_vector_store::EmbeddingVectorStore;
pub use entity_registry::EntityRegistry;
pub use graph_store::GraphStore;
pub use maintenance_store::MaintenanceStore;
pub use session_store::SessionStore;
pub use store::Store;
pub use time::{current_timestamp, timestamp_add_ms};
pub use transcript_store::TranscriptStore;
pub use types::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, ContextWindow, EmbeddingJobInsert,
    FeedbackEvent, GraphError, StorageError, TranscriptSessionSummary,
};
