pub mod duckdb;
pub mod entity_repo;
pub mod graph_store;
pub mod schema;
pub mod time;
pub mod transcript_repo;
pub mod vector_index;
pub mod vector_index_diagnose;

pub(crate) use duckdb::{sweep_orphan_embeddings, sweep_orphan_jobs};
pub use duckdb::{
    ClaimedEmbeddingJob, DuckDbRepository, EmbeddingJobInsert, EntityRegistry, FeedbackEvent,
    StorageError,
};
pub use graph_store::{DuckDbGraphStore, GraphError};
pub use time::{current_timestamp, timestamp_add_ms};
pub use transcript_repo::{ClaimedTranscriptEmbeddingJob, ContextWindow};
pub use vector_index::{
    sidecar_paths, transcript_sidecar_paths, EmbeddingRowSource, TranscriptEmbeddingRowSource,
    VectorIndex, VectorIndexError, VectorIndexFingerprint, VectorIndexMeta,
};
pub use vector_index_diagnose::{
    diagnose, diagnose_transcripts, rebuild_index, rebuild_transcripts_index, DiagnosticReport,
    DiagnosticStatus, PathInfo, SidecarFile,
};
