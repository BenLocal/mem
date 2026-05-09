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

pub mod duckdb_query;
pub mod lance_store;
pub mod store;
pub mod time;
pub mod types;

pub use store::Store;
pub use time::{current_timestamp, timestamp_add_ms};
pub use types::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, ContextWindow, EmbeddingJobInsert,
    FeedbackEvent, GraphError, StorageError, TranscriptSessionSummary,
};
