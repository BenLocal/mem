use std::path::PathBuf;

use serde::Serialize;

use super::VectorIndexFingerprint;

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReport {
    /// Coarse status string for `jq '.status'` filtering.
    /// One of: "healthy", "drift", "corrupt", "db_unavailable".
    pub status: &'static str,
    /// Full structured detail.
    pub details: DiagnosticStatus,
    pub paths: PathInfo,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathInfo {
    pub db: PathBuf,
    pub index: PathBuf,
    pub meta: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum SidecarFile {
    Index,
    Meta,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum DiagnosticStatus {
    Healthy { rows: i64 },
    SidecarMissing { which: SidecarFile },
    MetaCorrupt { reason: String },
    FingerprintMismatch {
        stored: VectorIndexFingerprint,
        current: VectorIndexFingerprint,
    },
    IndexCorrupt { reason: String },
    IndexMetaDrift { index_size: usize, meta_count: usize },
    DbDrift { meta_count: usize, db_count: i64 },
    DbUnavailable { reason: String },
}

impl DiagnosticStatus {
    pub fn coarse_status(&self) -> &'static str {
        match self {
            DiagnosticStatus::Healthy { .. } => "healthy",
            DiagnosticStatus::IndexMetaDrift { .. }
            | DiagnosticStatus::DbDrift { .. } => "drift",
            DiagnosticStatus::SidecarMissing { .. }
            | DiagnosticStatus::MetaCorrupt { .. }
            | DiagnosticStatus::FingerprintMismatch { .. }
            | DiagnosticStatus::IndexCorrupt { .. } => "corrupt",
            DiagnosticStatus::DbUnavailable { .. } => "db_unavailable",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            DiagnosticStatus::Healthy { .. } => 0,
            DiagnosticStatus::DbUnavailable { .. } => 2,
            _ => 1,
        }
    }
}
