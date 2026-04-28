use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use super::vector_index::{sidecar_paths, VectorIndex, VectorIndexMeta};
use super::{DuckDbRepository, StorageError, VectorIndexFingerprint};

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

// ── diagnose() ───────────────────────────────────────────────────────────────

/// Inspect the sidecar files and the live DB to determine the health of the
/// vector index.  Returns `Err` only when the DB itself is unavailable
/// (i.e. `count_total_memory_embeddings` fails); every other anomaly is
/// reported as a `DiagnosticStatus` variant inside the returned `Ok`.
pub async fn diagnose(
    repo: &DuckDbRepository,
    db_path: &Path,
    expected_fp: &VectorIndexFingerprint,
) -> Result<DiagnosticReport, StorageError> {
    let started = Instant::now();
    let (idx_path, meta_path) = sidecar_paths(db_path);
    let path_info = PathInfo {
        db: db_path.to_path_buf(),
        index: idx_path.clone(),
        meta: meta_path.clone(),
    };

    let live_count = repo.count_total_memory_embeddings().await?;

    if !idx_path.exists() {
        return Ok(report_for(
            DiagnosticStatus::SidecarMissing { which: SidecarFile::Index },
            path_info,
            started,
        ));
    }
    if !meta_path.exists() {
        return Ok(report_for(
            DiagnosticStatus::SidecarMissing { which: SidecarFile::Meta },
            path_info,
            started,
        ));
    }

    let meta_bytes = match std::fs::read(&meta_path) {
        Ok(b) => b,
        Err(e) => {
            return Ok(report_for(
                DiagnosticStatus::MetaCorrupt { reason: e.to_string() },
                path_info,
                started,
            ));
        }
    };
    let meta: VectorIndexMeta = match serde_json::from_slice(&meta_bytes) {
        Ok(m) => m,
        Err(e) => {
            return Ok(report_for(
                DiagnosticStatus::MetaCorrupt { reason: e.to_string() },
                path_info,
                started,
            ));
        }
    };

    if meta.dim == 0
        || meta.provider != expected_fp.provider
        || meta.model != expected_fp.model
        || meta.dim != expected_fp.dim
    {
        return Ok(report_for(
            DiagnosticStatus::FingerprintMismatch {
                stored: VectorIndexFingerprint {
                    provider: meta.provider.clone(),
                    model: meta.model.clone(),
                    dim: meta.dim,
                },
                current: expected_fp.clone(),
            },
            path_info,
            started,
        ));
    }

    let opts = usearch_options(&meta);
    let index_load_result = match usearch::Index::new(&opts) {
        Ok(idx) => match idx.reserve(meta.row_count.max(8)) {
            Ok(()) => match idx_path.to_str() {
                Some(p) => match idx.load(p) {
                    Ok(()) => Ok(idx),
                    Err(e) => Err(format!("usearch load failed: {e}")),
                },
                None => Err("non-utf8 sidecar path".to_string()),
            },
            Err(e) => Err(format!("reserve failed: {e}")),
        },
        Err(e) => Err(format!("new index failed: {e}")),
    };
    let index = match index_load_result {
        Ok(i) => i,
        Err(reason) => {
            return Ok(report_for(
                DiagnosticStatus::IndexCorrupt { reason },
                path_info,
                started,
            ));
        }
    };

    let index_size = index.size();
    if index_size != meta.row_count {
        return Ok(report_for(
            DiagnosticStatus::IndexMetaDrift { index_size, meta_count: meta.row_count },
            path_info,
            started,
        ));
    }
    if (meta.row_count as i64) != live_count {
        return Ok(report_for(
            DiagnosticStatus::DbDrift { meta_count: meta.row_count, db_count: live_count },
            path_info,
            started,
        ));
    }

    Ok(report_for(
        DiagnosticStatus::Healthy { rows: live_count },
        path_info,
        started,
    ))
}

fn report_for(
    status: DiagnosticStatus,
    paths: PathInfo,
    started: Instant,
) -> DiagnosticReport {
    let coarse = status.coarse_status();
    DiagnosticReport {
        status: coarse,
        details: status,
        paths,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

fn usearch_options(meta: &VectorIndexMeta) -> usearch::IndexOptions {
    usearch::IndexOptions {
        dimensions: meta.dim,
        metric: usearch::MetricKind::Cos,
        quantization: usearch::ScalarKind::F32,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    }
}

/// Force a fresh rebuild of the sidecar from DuckDB. Existing sidecar files are
/// deleted (best-effort) so `open_or_rebuild` falls through to its rebuild branch.
pub async fn rebuild_index(
    repo: &DuckDbRepository,
    db_path: &Path,
    expected_fp: &VectorIndexFingerprint,
) -> Result<Arc<VectorIndex>, StorageError> {
    let (idx_path, meta_path) = sidecar_paths(db_path);
    // Best-effort delete; NotFound is fine.
    if let Err(e) = std::fs::remove_file(&idx_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(StorageError::VectorIndex(format!("failed to remove old index: {e}")));
        }
    }
    if let Err(e) = std::fs::remove_file(&meta_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(StorageError::VectorIndex(format!("failed to remove old meta: {e}")));
        }
    }
    let idx = VectorIndex::open_or_rebuild(repo, db_path, expected_fp)
        .await
        .map_err(|e| StorageError::VectorIndex(format!("rebuild failed: {e}")))?;
    Ok(Arc::new(idx))
}
