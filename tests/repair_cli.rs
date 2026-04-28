use mem::storage::{DiagnosticReport, DiagnosticStatus, PathInfo, SidecarFile, VectorIndexFingerprint, sidecar_paths};
use std::path::PathBuf;

// ── imports used by the async integration tests ───────────────────────────────
use mem::config::EmbeddingSettings;
use mem::domain::memory::{
    IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode,
};
use mem::embedding::arc_embedding_provider;
use mem::service::{embedding_worker, MemoryService};
use mem::storage::{diagnose, DuckDbRepository, VectorIndex};
use std::sync::Arc;
use tempfile::tempdir;

fn fp(dim: usize) -> VectorIndexFingerprint {
    VectorIndexFingerprint { provider: "fake".into(), model: "fake".into(), dim }
}

fn paths() -> PathInfo {
    PathInfo {
        db: PathBuf::from("/tmp/x.duckdb"),
        index: PathBuf::from("/tmp/x.duckdb.usearch"),
        meta: PathBuf::from("/tmp/x.duckdb.usearch.meta.json"),
    }
}

#[test]
fn diagnostic_report_serializes_with_status_string_and_kind_field() {
    let cases = vec![
        (
            DiagnosticStatus::Healthy { rows: 42 },
            "healthy",
            "Healthy",
        ),
        (
            DiagnosticStatus::SidecarMissing { which: SidecarFile::Index },
            "corrupt",
            "SidecarMissing",
        ),
        (
            DiagnosticStatus::MetaCorrupt { reason: "parse fail".into() },
            "corrupt",
            "MetaCorrupt",
        ),
        (
            DiagnosticStatus::FingerprintMismatch { stored: fp(128), current: fp(256) },
            "corrupt",
            "FingerprintMismatch",
        ),
        (
            DiagnosticStatus::IndexCorrupt { reason: "load fail".into() },
            "corrupt",
            "IndexCorrupt",
        ),
        (
            DiagnosticStatus::IndexMetaDrift { index_size: 5, meta_count: 6 },
            "drift",
            "IndexMetaDrift",
        ),
        (
            DiagnosticStatus::DbDrift { meta_count: 7, db_count: 8 },
            "drift",
            "DbDrift",
        ),
        (
            DiagnosticStatus::DbUnavailable { reason: "locked".into() },
            "db_unavailable",
            "DbUnavailable",
        ),
    ];

    for (status, expected_status, expected_kind) in cases {
        let report = DiagnosticReport {
            status: expected_status_to_static(expected_status),
            details: status,
            paths: paths(),
            elapsed_ms: 12,
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["status"], expected_status, "status string for {expected_kind}");
        assert_eq!(v["details"]["kind"], expected_kind, "details.kind for {expected_kind}");
        assert!(v["paths"]["db"].is_string(), "paths.db present");
        assert!(v["paths"]["index"].is_string(), "paths.index present");
        assert!(v["paths"]["meta"].is_string(), "paths.meta present");
        assert!(v["elapsed_ms"].is_number(), "elapsed_ms present");
    }
}

fn expected_status_to_static(s: &str) -> &'static str {
    match s {
        "healthy" => "healthy",
        "drift" => "drift",
        "corrupt" => "corrupt",
        "db_unavailable" => "db_unavailable",
        _ => unreachable!(),
    }
}

// ── integration helpers ───────────────────────────────────────────────────────

async fn seed_one_row_with_index(db_path: &std::path::Path) -> (DuckDbRepository, Arc<VectorIndex>) {
    let repo = DuckDbRepository::open(db_path).await.unwrap();
    let settings = EmbeddingSettings::development_defaults();
    let provider = arc_embedding_provider(&settings).unwrap();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = Arc::new(VectorIndex::open_or_rebuild(&repo, db_path, &fp).await.unwrap());
    repo.attach_vector_index(idx.clone());
    let svc = MemoryService::new(repo.clone());
    svc.ingest(IngestMemoryRequest {
        tenant: "t".into(),
        memory_type: MemoryType::Implementation,
        content: "diag-target".into(),
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: None,
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    })
    .await
    .unwrap();
    embedding_worker::tick(&repo, provider.as_ref(), &settings).await.unwrap();
    // Force a save so the meta.row_count is durable on disk.
    idx.save_at_default_paths().await.unwrap();
    (repo, idx)
}

#[tokio::test]
async fn diagnose_healthy_db_returns_healthy() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("h.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;

    let settings = EmbeddingSettings::development_defaults();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };

    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "healthy");
    assert!(matches!(report.details, DiagnosticStatus::Healthy { rows: 1 }));
    assert_eq!(report.details.exit_code(), 0);
}

#[tokio::test]
async fn diagnose_reports_sidecar_missing_when_index_file_deleted() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sm.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;
    let (idx_path, _) = sidecar_paths(&db);
    std::fs::remove_file(&idx_path).unwrap();

    let settings = EmbeddingSettings::development_defaults();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "corrupt");
    match report.details {
        DiagnosticStatus::SidecarMissing { which } => {
            assert_eq!(which, SidecarFile::Index);
        }
        other => panic!("expected SidecarMissing, got {other:?}"),
    }
}

#[tokio::test]
async fn diagnose_reports_sidecar_missing_when_meta_file_deleted() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("smm.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;
    let (_, meta_path) = sidecar_paths(&db);
    std::fs::remove_file(&meta_path).unwrap();

    let settings = EmbeddingSettings::development_defaults();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    match report.details {
        DiagnosticStatus::SidecarMissing { which } => {
            assert_eq!(which, SidecarFile::Meta);
        }
        other => panic!("expected SidecarMissing(Meta), got {other:?}"),
    }
}

#[tokio::test]
async fn diagnose_reports_meta_corrupt_when_meta_is_invalid_json() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("mc.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;
    let (_, meta_path) = sidecar_paths(&db);
    std::fs::write(&meta_path, b"{ this is not json").unwrap();

    let settings = EmbeddingSettings::development_defaults();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "corrupt");
    assert!(matches!(report.details, DiagnosticStatus::MetaCorrupt { .. }));
}

#[tokio::test]
async fn diagnose_reports_fingerprint_mismatch_on_dim_change() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("fp.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;

    // Pass a fingerprint with a different dim than what's on disk
    let settings = EmbeddingSettings::development_defaults();
    let mut fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    fp.dim = 128;  // disk has 256 (development default)

    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "corrupt");
    match report.details {
        DiagnosticStatus::FingerprintMismatch { stored, current } => {
            assert_eq!(stored.dim, settings.dim);
            assert_eq!(current.dim, 128);
        }
        other => panic!("expected FingerprintMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn diagnose_reports_fingerprint_mismatch_on_zero_dim_meta() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("zd.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;
    let (_, meta_path) = sidecar_paths(&db);

    // Hand-edit meta to have dim=0
    let settings = EmbeddingSettings::development_defaults();
    let zero_meta = mem::storage::VectorIndexMeta {
        schema_version: 1,
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: 0,
        row_count: 1,
        id_map: Default::default(),
    };
    std::fs::write(&meta_path, serde_json::to_vec(&zero_meta).unwrap()).unwrap();

    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: 0,  // matches stored — but the dim==0 guard fires regardless
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "corrupt");
    assert!(matches!(report.details, DiagnosticStatus::FingerprintMismatch { .. }));
}

#[tokio::test]
async fn diagnose_reports_index_corrupt_when_binary_is_garbage() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ic.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;
    let (idx_path, _) = sidecar_paths(&db);
    std::fs::write(&idx_path, b"GARBAGE_NOT_USEARCH_BINARY").unwrap();

    let settings = EmbeddingSettings::development_defaults();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "corrupt");
    assert!(matches!(report.details, DiagnosticStatus::IndexCorrupt { .. }));
}

#[tokio::test]
async fn diagnose_reports_index_meta_drift_when_meta_lies_about_count() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("imd.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;
    let (_, meta_path) = sidecar_paths(&db);

    // Read meta, bump row_count by 5 (meta lies about count), write back
    let raw = std::fs::read(&meta_path).unwrap();
    let mut meta: mem::storage::VectorIndexMeta = serde_json::from_slice(&raw).unwrap();
    meta.row_count = meta.row_count + 5;
    std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

    let settings = EmbeddingSettings::development_defaults();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "drift");
    match report.details {
        DiagnosticStatus::IndexMetaDrift { index_size, meta_count } => {
            assert_eq!(index_size, 1);
            assert_eq!(meta_count, 6);
        }
        other => panic!("expected IndexMetaDrift, got {other:?}"),
    }
}
