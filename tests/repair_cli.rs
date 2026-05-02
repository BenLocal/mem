use mem::cli::repair::{
    compute_rebuild_graph_outcome, format_check_json, format_check_text, format_rebuild_graph_json,
    format_rebuild_graph_text, format_rebuild_json, format_rebuild_text, run_check_for_test,
    RebuildGraphOutcome, RebuildOutcome,
};
use mem::storage::{
    rebuild_index, sidecar_paths, transcript_sidecar_paths, DiagnosticReport, DiagnosticStatus,
    PathInfo, SidecarFile, VectorIndexFingerprint,
};
use std::path::PathBuf;

// ── imports used by the async integration tests ───────────────────────────────
use mem::config::{EmbeddingProviderKind, EmbeddingSettings};
use mem::domain::memory::{IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode};
use mem::embedding::arc_embedding_provider;
use mem::service::{embedding_worker, MemoryService};
use mem::storage::{diagnose, DuckDbRepository, VectorIndex};
use std::sync::Arc;
use tempfile::tempdir;

// ── extra imports used by the `--rebuild-graph` migration tests ───────────────
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tempfile::TempDir;
use tower::util::ServiceExt;

/// Embedding settings for the integration tests in this file.
///
/// These tests exercise the diagnose/rebuild plumbing — not the embedding
/// provider — so they pin a small, offline `Fake` provider with a fixed
/// dim. This decouples the tests from `EmbeddingSettings::development_defaults`
/// (which now uses the EmbedAnything backend; see master commit 47aff1e) and
/// keeps the test fixture vector sizes in sync with the runtime fingerprint.
fn test_settings() -> EmbeddingSettings {
    let mut s = EmbeddingSettings::development_defaults();
    s.provider = EmbeddingProviderKind::Fake;
    s.model = "fake".to_string();
    s.dim = 256;
    s
}

fn fp(dim: usize) -> VectorIndexFingerprint {
    VectorIndexFingerprint {
        provider: "fake".into(),
        model: "fake".into(),
        dim,
    }
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
        (DiagnosticStatus::Healthy { rows: 42 }, "healthy", "Healthy"),
        (
            DiagnosticStatus::SidecarMissing {
                which: SidecarFile::Index,
            },
            "corrupt",
            "SidecarMissing",
        ),
        (
            DiagnosticStatus::MetaCorrupt {
                reason: "parse fail".into(),
            },
            "corrupt",
            "MetaCorrupt",
        ),
        (
            DiagnosticStatus::FingerprintMismatch {
                stored: fp(128),
                current: fp(256),
            },
            "corrupt",
            "FingerprintMismatch",
        ),
        (
            DiagnosticStatus::IndexCorrupt {
                reason: "load fail".into(),
            },
            "corrupt",
            "IndexCorrupt",
        ),
        (
            DiagnosticStatus::IndexMetaDrift {
                index_size: 5,
                meta_count: 6,
            },
            "drift",
            "IndexMetaDrift",
        ),
        (
            DiagnosticStatus::DbDrift {
                meta_count: 7,
                db_count: 8,
            },
            "drift",
            "DbDrift",
        ),
        (
            DiagnosticStatus::DbUnavailable {
                reason: "locked".into(),
            },
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
        assert_eq!(
            v["status"], expected_status,
            "status string for {expected_kind}"
        );
        assert_eq!(
            v["details"]["kind"], expected_kind,
            "details.kind for {expected_kind}"
        );
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

async fn seed_one_row_with_index(
    db_path: &std::path::Path,
) -> (DuckDbRepository, Arc<VectorIndex>) {
    let repo = DuckDbRepository::open(db_path).await.unwrap();
    let settings = test_settings();
    let provider = arc_embedding_provider(&settings).unwrap();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = Arc::new(
        VectorIndex::open_or_rebuild(&repo, db_path, &fp)
            .await
            .unwrap(),
    );
    repo.attach_vector_index(idx.clone());
    let svc = MemoryService::new(repo.clone());
    svc.ingest(IngestMemoryRequest {
        tenant: "t".into(),
        memory_type: MemoryType::Implementation,
        content: "diag-target".into(),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: None,
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    })
    .await
    .unwrap();
    embedding_worker::tick(&repo, provider.as_ref(), &settings)
        .await
        .unwrap();
    // Force a save so the meta.row_count is durable on disk.
    idx.save_at_default_paths().await.unwrap();
    (repo, idx)
}

#[tokio::test]
async fn diagnose_healthy_db_returns_healthy() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("h.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;

    let settings = test_settings();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };

    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "healthy");
    assert!(matches!(
        report.details,
        DiagnosticStatus::Healthy { rows: 1 }
    ));
    assert_eq!(report.details.exit_code(), 0);
}

#[tokio::test]
async fn diagnose_reports_sidecar_missing_when_index_file_deleted() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sm.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;
    let (idx_path, _) = sidecar_paths(&db);
    std::fs::remove_file(&idx_path).unwrap();

    let settings = test_settings();
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

    let settings = test_settings();
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

    let settings = test_settings();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "corrupt");
    assert!(matches!(
        report.details,
        DiagnosticStatus::MetaCorrupt { .. }
    ));
}

#[tokio::test]
async fn diagnose_reports_fingerprint_mismatch_on_dim_change() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("fp.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;

    // Pass a fingerprint with a different dim than what's on disk
    let settings = test_settings();
    let mut fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    fp.dim = 128; // disk has settings.dim from test_settings()

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
    let settings = test_settings();
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
        dim: 0, // matches stored — but the dim==0 guard fires regardless
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "corrupt");
    assert!(matches!(
        report.details,
        DiagnosticStatus::FingerprintMismatch { .. }
    ));
}

#[tokio::test]
async fn diagnose_reports_index_corrupt_when_binary_is_garbage() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ic.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;
    let (idx_path, _) = sidecar_paths(&db);
    std::fs::write(&idx_path, b"GARBAGE_NOT_USEARCH_BINARY").unwrap();

    let settings = test_settings();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "corrupt");
    assert!(matches!(
        report.details,
        DiagnosticStatus::IndexCorrupt { .. }
    ));
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
    meta.row_count += 5;
    std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

    let settings = test_settings();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "drift");
    match report.details {
        DiagnosticStatus::IndexMetaDrift {
            index_size,
            meta_count,
        } => {
            assert_eq!(index_size, 1);
            assert_eq!(meta_count, 6);
        }
        other => panic!("expected IndexMetaDrift, got {other:?}"),
    }
}

#[tokio::test]
async fn diagnose_reports_db_drift_when_db_has_extra_rows() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("dd.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;

    // Bypass the worker — add a memory_embeddings row directly so the index
    // can't know about it. seed_memory_embedding_for_test from §3 Task 8.
    repo.seed_memory_embedding_for_test("ghost_row", "t", &vec![0.0f32; 256])
        .await
        .unwrap();

    let settings = test_settings();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "drift");
    match report.details {
        DiagnosticStatus::DbDrift {
            meta_count,
            db_count,
        } => {
            assert_eq!(meta_count, 1);
            assert_eq!(db_count, 2);
        }
        other => panic!("expected DbDrift, got {other:?}"),
    }
}

#[tokio::test]
async fn rebuild_index_recovers_from_drift() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("rb.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;

    // Induce DbDrift by bypassing service
    repo.seed_memory_embedding_for_test("orphan", "t", &vec![0.0f32; 256])
        .await
        .unwrap();

    let settings = test_settings();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };

    let pre = diagnose(&repo, &db, &fp).await.unwrap();
    assert!(matches!(pre.details, DiagnosticStatus::DbDrift { .. }));

    let new_idx = rebuild_index(&repo, &db, &fp).await.unwrap();
    assert_eq!(new_idx.size(), 2);

    let post = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(post.status, "healthy");
    match post.details {
        DiagnosticStatus::Healthy { rows } => assert_eq!(rows, 2),
        other => panic!("expected Healthy after rebuild, got {other:?}"),
    }
}

#[test]
fn format_check_text_for_healthy() {
    let report = DiagnosticReport {
        status: "healthy",
        details: DiagnosticStatus::Healthy { rows: 1247 },
        paths: paths(),
        elapsed_ms: 18,
    };
    let s = format_check_text(&report);
    assert!(s.contains("Healthy"), "{s}");
    assert!(s.contains("1247"), "{s}");
    assert!(s.contains("18ms"), "{s}");
}

#[test]
fn format_check_text_for_db_drift() {
    let report = DiagnosticReport {
        status: "drift",
        details: DiagnosticStatus::DbDrift {
            meta_count: 1247,
            db_count: 1250,
        },
        paths: paths(),
        elapsed_ms: 12,
    };
    let s = format_check_text(&report);
    assert!(s.contains("Drift detected"), "{s}");
    assert!(s.contains("1247"), "{s}");
    assert!(s.contains("1250"), "{s}");
    assert!(
        s.contains("mem repair --rebuild"),
        "should suggest rebuild: {s}"
    );
}

#[test]
fn format_check_text_for_index_corrupt() {
    let report = DiagnosticReport {
        status: "corrupt",
        details: DiagnosticStatus::IndexCorrupt {
            reason: "boom".into(),
        },
        paths: paths(),
        elapsed_ms: 8,
    };
    let s = format_check_text(&report);
    assert!(s.contains("Index file is corrupt"), "{s}");
    assert!(s.contains("boom"), "{s}");
    assert!(s.contains("mem repair --rebuild"), "{s}");
}

#[test]
fn format_check_text_for_db_unavailable() {
    let report = DiagnosticReport {
        status: "db_unavailable",
        details: DiagnosticStatus::DbUnavailable {
            reason: "file is locked".into(),
        },
        paths: paths(),
        elapsed_ms: 2,
    };
    let s = format_check_text(&report);
    assert!(s.contains("Could not open DB"), "{s}");
    assert!(
        s.contains("mem serve"),
        "should hint at running service: {s}"
    );
}

#[test]
fn format_check_json_emits_top_level_status_and_exit_code() {
    let report = DiagnosticReport {
        status: "drift",
        details: DiagnosticStatus::DbDrift {
            meta_count: 5,
            db_count: 7,
        },
        paths: paths(),
        elapsed_ms: 10,
    };
    let v = format_check_json(&report);
    assert_eq!(v["command"], "check");
    assert_eq!(v["status"], "drift");
    assert_eq!(v["exit_code"], 1);
    assert_eq!(v["details"]["kind"], "DbDrift");
    assert_eq!(v["details"]["meta_count"], 5);
    assert_eq!(v["details"]["db_count"], 7);
    assert!(v["paths"]["db"].is_string());
    assert_eq!(v["elapsed_ms"], 10);
}

#[test]
fn format_check_json_unavailable_uses_exit_code_two() {
    let report = DiagnosticReport {
        status: "db_unavailable",
        details: DiagnosticStatus::DbUnavailable {
            reason: "locked".into(),
        },
        paths: paths(),
        elapsed_ms: 1,
    };
    let v = format_check_json(&report);
    assert_eq!(v["exit_code"], 2);
}

#[test]
fn format_rebuild_text_for_success() {
    let outcome = RebuildOutcome::Rebuilt {
        rows: 1247,
        paths: paths(),
        elapsed_ms: 832,
    };
    let s = format_rebuild_text(&outcome);
    assert!(s.contains("1247"), "{s}");
    assert!(s.contains("832"), "{s}");
    assert!(s.contains("Rebuilt"), "{s}");
}

#[test]
fn format_rebuild_json_for_success() {
    let outcome = RebuildOutcome::Rebuilt {
        rows: 1247,
        paths: paths(),
        elapsed_ms: 832,
    };
    let v = format_rebuild_json(&outcome);
    assert_eq!(v["command"], "rebuild");
    assert_eq!(v["status"], "rebuilt");
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["rows"], 1247);
    assert_eq!(v["elapsed_ms"], 832);
}

#[test]
fn format_rebuild_json_for_db_unavailable() {
    let outcome = RebuildOutcome::DbUnavailable {
        reason: "locked".into(),
        paths: paths(),
    };
    let v = format_rebuild_json(&outcome);
    assert_eq!(v["status"], "db_unavailable");
    assert_eq!(v["exit_code"], 2);
}

#[test]
fn format_rebuild_json_for_failed() {
    let outcome = RebuildOutcome::Failed {
        reason: "disk full".into(),
        paths: paths(),
    };
    let v = format_rebuild_json(&outcome);
    assert_eq!(v["status"], "rebuild_failed");
    assert_eq!(v["exit_code"], 2);
    assert_eq!(v["details"]["reason"], "disk full");
}

// ── Aggregate (memories + transcripts) repair tests ───────────────────────────

/// `mem repair --check` reports per-pipeline status for both sidecars and
/// produces an aggregate exit code that reflects the worst of the two.
///
/// Setup: a temp DB with the memories sidecar created (one healthy row) and
/// the transcripts sidecar deleted to force a "missing" diagnostic on that
/// pipeline. Aggregate exit code is therefore 1 (corrupt > healthy).
#[tokio::test]
async fn repair_check_reports_status_for_both_sidecars() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("agg.duckdb");

    // Seed memories sidecar (healthy with one row).
    let (repo, _idx) = seed_one_row_with_index(&db).await;

    // Create the transcripts sidecar by calling its open_or_rebuild fn — both
    // files will exist on disk once it returns. We use the shared fingerprint
    // below; the transcript table is empty so the rebuild produces an
    // empty-but-valid sidecar.
    let settings = test_settings();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let _tr_idx = VectorIndex::open_or_rebuild_transcripts(&repo, &db, &fp)
        .await
        .unwrap();
    let (tr_idx_path, tr_meta_path) = transcript_sidecar_paths(&db);
    assert!(
        tr_idx_path.exists() && tr_meta_path.exists(),
        "transcript sidecar should be on disk after open_or_rebuild_transcripts"
    );

    // Now delete the transcripts sidecar binary to force "SidecarMissing".
    std::fs::remove_file(&tr_idx_path).unwrap();

    // Build a Config pointing at the temp db.
    let mut config = mem::config::Config::local();
    config.db_path = db.clone();
    config.embedding = settings;

    let (text, exit) = run_check_for_test(&config, &fp).await;

    assert!(
        text.contains("=== Memories sidecar ==="),
        "text should have memories section header: {text}"
    );
    assert!(
        text.contains("=== Transcripts sidecar ==="),
        "text should have transcripts section header: {text}"
    );

    // Memories side: expect a healthy line referencing `<db>.usearch` (but
    // NOT `.transcripts.usearch`). We split on the transcripts header so we
    // can scope the assertion to the memories block.
    let (mem_block, tr_block) = text
        .split_once("=== Transcripts sidecar ===")
        .expect("split on transcripts header");
    assert!(
        mem_block.contains("Healthy"),
        "memories section should be healthy: {mem_block}"
    );
    assert!(
        mem_block.contains(".usearch") && !mem_block.contains(".transcripts.usearch"),
        "memories section should reference memories sidecar path: {mem_block}"
    );

    // Transcripts side: expect "Sidecar index file is missing" and a path
    // hint that points at the transcripts sidecar.
    assert!(
        tr_block.contains("Sidecar index file is missing") || tr_block.contains("Sidecar index"),
        "transcripts section should say sidecar missing: {tr_block}"
    );
    assert!(
        tr_block.contains("mem repair --rebuild"),
        "transcripts section should suggest rebuild: {tr_block}"
    );

    // Aggregate exit code: memories=0, transcripts=1 (corrupt). Worst-of-two
    // bubbles up.
    assert_eq!(
        exit, 1,
        "aggregate exit code should reflect the unhealthy transcripts sidecar"
    );
}

// ── `mem repair --rebuild-graph` migration tests ──────────────────────────────
//
// These tests verify Task 11 of docs/superpowers/plans/2026-05-02-entity-registry.md:
// the rebuild walks every memory, deletes pre-migration legacy edges
// (`to_node_id LIKE 'project:%'` etc.), and re-derives `entity:<uuid>` edges
// through the production extract_graph_edge_drafts → resolve_drafts_to_edges
// chain. The rebuild must be idempotent (re-running is a no-op) and must
// degrade gracefully on an empty DB.

/// After ingest writes the new entity-typed graph edges, an injected
/// pre-migration legacy edge (`project:foo`) must be removed by
/// `--rebuild-graph`, leaving only `entity:<uuid>` edges. Idempotency
/// is covered by `rebuild_graph_is_idempotent` below.
#[tokio::test]
async fn rebuild_graph_converts_legacy_to_entity_refs() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");

    // Bootstrap and seed a memory through the production path.
    let app = mem::app::router_with_config(cfg.clone()).await.unwrap();
    let body = json!({
        "tenant": "local",
        "memory_type": "implementation",
        "content": "x",
        "scope": "global",
        "source_agent": "test",
        "project": "mem",
        "topics": ["Rust"],
        "write_mode": "auto"
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/memories")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Inject a legacy-format graph edge directly. The from_node_id references
    // a memory that exists in the table (we re-use one from above), simulating
    // a pre-migration edge that the rebuild should sweep away.
    //
    // We scope the side-channel `Connection` to a block so it is dropped
    // before `compute_rebuild_graph_outcome` opens its own connection — DuckDB
    // treats overlapping file connections cautiously, and dropping ours
    // first avoids visibility flakiness around the insert.
    {
        let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
        let memory_id: String = conn
            .query_row(
                "select memory_id from memories where tenant = 'local' limit 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let from = format!("memory:{memory_id}");
        conn.execute(
            "insert into graph_edges (from_node_id, to_node_id, relation, valid_from, valid_to) \
             values (?1, 'project:legacy-project', 'applies_to', '00000000020260501000', null)",
            duckdb::params![&from],
        )
        .unwrap();
    }

    let outcome = compute_rebuild_graph_outcome(&cfg).await.unwrap();
    assert!(matches!(outcome, RebuildGraphOutcome::Rebuilt { .. }));

    // After rebuild: NO legacy 'project:...' edges remain.
    let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
    let legacy_count: i64 = conn
        .query_row(
            "select count(*) from graph_edges where to_node_id like 'project:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy_count, 0);

    // All memory→entity edges use 'entity:<uuid>' format.
    let entity_count: i64 = conn
        .query_row(
            "select count(*) from graph_edges \
             where from_node_id like 'memory:%' and to_node_id like 'entity:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        entity_count >= 2,
        "got {entity_count}; expected at least project + topic edges"
    );
}

/// Running `--rebuild-graph` twice produces the same edge count as running
/// it once — `EntityRegistry::resolve_or_create` returns existing entity_ids
/// on lookup, so the rebuild is fully idempotent.
#[tokio::test]
async fn rebuild_graph_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = mem::app::router_with_config(cfg.clone()).await.unwrap();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/memories")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "local",
                        "memory_type": "implementation",
                        "content": "x",
                        "scope": "global",
                        "source_agent": "test",
                        "topics": ["Rust"],
                        "write_mode": "auto"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    compute_rebuild_graph_outcome(&cfg).await.unwrap();
    let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
    let count1: i64 = conn
        .query_row("select count(*) from graph_edges", [], |r| r.get(0))
        .unwrap();

    compute_rebuild_graph_outcome(&cfg).await.unwrap();
    let count2: i64 = conn
        .query_row("select count(*) from graph_edges", [], |r| r.get(0))
        .unwrap();

    assert_eq!(count1, count2, "rebuild must be idempotent");
}

/// On a freshly-bootstrapped DB with zero memories, `--rebuild-graph`
/// returns successfully with `rebuilt_memory_count == 0`.
#[tokio::test]
async fn rebuild_graph_handles_empty_database() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    // Bootstrap schema by spinning up the router (which calls
    // `DuckDbRepository::open` → schema migrations).
    let _app = mem::app::router_with_config(cfg.clone()).await.unwrap();

    let outcome = compute_rebuild_graph_outcome(&cfg).await.unwrap();
    match outcome {
        RebuildGraphOutcome::Rebuilt {
            rebuilt_memory_count,
            ..
        } => assert_eq!(rebuilt_memory_count, 0),
        other => panic!("expected Rebuilt, got {other:?}"),
    }
}

// ── format_rebuild_graph_text / _json unit tests ──────────────────────────────

#[test]
fn format_rebuild_graph_text_emits_status_for_rebuilt() {
    let outcome = RebuildGraphOutcome::Rebuilt {
        rebuilt_memory_count: 42,
        new_edge_count: 87,
        elapsed_ms: 155,
    };
    let s = format_rebuild_graph_text(&outcome);
    assert!(s.contains("Rebuilt"), "should say Rebuilt: {s}");
    assert!(s.contains("42"), "should include memory count: {s}");
    assert!(s.contains("87"), "should include edge count: {s}");
    assert!(s.contains("155"), "should include elapsed: {s}");
}

#[test]
fn format_rebuild_graph_text_emits_status_for_failed() {
    let outcome = RebuildGraphOutcome::Failed {
        reason: "connection refused".into(),
    };
    let s = format_rebuild_graph_text(&outcome);
    assert!(
        s.contains("Rebuild-graph failed:"),
        "should say Rebuild-graph failed: {s}"
    );
    assert!(
        s.contains("connection refused"),
        "should include the reason: {s}"
    );
}

#[test]
fn format_rebuild_graph_json_emits_keys_for_rebuilt() {
    let outcome = RebuildGraphOutcome::Rebuilt {
        rebuilt_memory_count: 10,
        new_edge_count: 25,
        elapsed_ms: 99,
    };
    let v = format_rebuild_graph_json(&outcome);
    assert_eq!(v["command"], "rebuild-graph");
    assert_eq!(v["status"], "rebuilt");
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["rebuilt_memory_count"], 10);
    assert_eq!(v["new_edge_count"], 25);
    assert_eq!(v["elapsed_ms"], 99);
}

#[test]
fn format_rebuild_graph_json_emits_keys_for_failed() {
    let outcome = RebuildGraphOutcome::Failed {
        reason: "disk full".into(),
    };
    let v = format_rebuild_graph_json(&outcome);
    assert_eq!(v["command"], "rebuild-graph");
    assert_eq!(v["status"], "rebuild_failed");
    assert_eq!(v["exit_code"], 2);
    assert_eq!(v["details"]["reason"], "disk full");
}
