pub mod duckdb;
pub mod fts;
pub mod repository;
pub mod time;
pub mod vector_index;
pub mod vector_index_diagnose;

pub(crate) use duckdb::{sweep_orphan_embeddings, sweep_orphan_jobs};
pub use duckdb::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, ContextWindow, DuckDbGraphStore,
    DuckDbRepository, EmbeddingJobInsert, EntityRegistry, FeedbackEvent, GraphError,
    TranscriptSessionSummary,
};
pub use repository::{MemoryRepository, Repository};
pub use time::{current_timestamp, timestamp_add_ms};
pub use vector_index::{
    sidecar_paths, transcript_sidecar_paths, EmbeddingRowSource, TranscriptEmbeddingRowSource,
    VectorIndex, VectorIndexError, VectorIndexFingerprint, VectorIndexMeta,
};
pub use vector_index_diagnose::{
    diagnose, diagnose_transcripts, rebuild_index, rebuild_transcripts_index, DiagnosticReport,
    DiagnosticStatus, PathInfo, SidecarFile,
};

// Re-export StorageError at top level — pulled directly from duckdb mod for now;
// the type itself isn't backend-specific (it's the shared error returned by all
// storage methods) and lives there for legacy reasons.
pub use duckdb::StorageError;
