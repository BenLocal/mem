use mem::storage::{DiagnosticReport, DiagnosticStatus, PathInfo, SidecarFile, VectorIndexFingerprint};
use std::path::PathBuf;

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
