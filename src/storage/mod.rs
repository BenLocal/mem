pub mod duckdb;
pub mod graph_store;
pub mod schema;
pub mod transcript_repo;
pub mod vector_index;
pub mod vector_index_diagnose;

pub use duckdb::{
    ClaimedEmbeddingJob, DuckDbRepository, EmbeddingJobInsert, FeedbackEvent, StorageError,
};
pub use graph_store::{DuckDbGraphStore, GraphError};
pub use transcript_repo::ClaimedTranscriptEmbeddingJob;
pub use vector_index::{
    sidecar_paths, transcript_sidecar_paths, EmbeddingRowSource, TranscriptEmbeddingRowSource,
    VectorIndex, VectorIndexError, VectorIndexFingerprint, VectorIndexMeta,
};
pub use vector_index_diagnose::{
    diagnose, rebuild_index, DiagnosticReport, DiagnosticStatus, PathInfo, SidecarFile,
};
