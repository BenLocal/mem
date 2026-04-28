pub mod duckdb;
pub mod graph_store;
pub mod schema;
pub mod vector_index;
pub mod vector_index_diagnose;

pub use duckdb::{
    ClaimedEmbeddingJob, DuckDbRepository, EmbeddingJobInsert, FeedbackEvent, StorageError,
};
pub use graph_store::{DuckDbGraphStore, GraphError};
pub use vector_index::{
    sidecar_paths, EmbeddingRowSource, VectorIndex, VectorIndexError, VectorIndexFingerprint,
    VectorIndexMeta,
};
pub use vector_index_diagnose::{
    diagnose, rebuild_index, DiagnosticReport, DiagnosticStatus, PathInfo, SidecarFile,
};
