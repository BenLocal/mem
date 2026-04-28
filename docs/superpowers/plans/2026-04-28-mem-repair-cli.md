# `mem repair` Subcommand Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `mem repair` subcommand with two modes — `--check` (read-only health diagnostic) and `--rebuild` (force-recreate sidecar from DuckDB) — closing mempalace-diff §8 #4.

**Architecture:** A new `vector_index_diagnose` module exposes a single `diagnose()` function returning a `DiagnosticReport` with eight possible status variants. A new `cli/repair` module formats reports as text or JSON and maps statuses to exit codes. The rebuild path reuses `VectorIndex::open_or_rebuild` verbatim — it deletes the two sidecar files first so the function's load branch fails through to the rebuild branch. Both modes are offline (DuckDB single-writer lock physically prevents concurrent access).

**Tech Stack:** Rust, clap (subcommand), serde (JSON output), existing `usearch` + `DuckDbRepository` + `VectorIndex` from §3.

**Spec:** `docs/superpowers/specs/2026-04-28-mem-repair-cli-design.md`

---

## File Structure

**Create:**
- `src/storage/vector_index_diagnose.rs` — `DiagnosticReport`, `DiagnosticStatus`, `PathInfo`, `SidecarFile`, `diagnose()` + `rebuild()` free functions
- `src/cli/mod.rs` — `pub mod repair;`
- `src/cli/repair.rs` — `run()` entry point + text/JSON formatters
- `tests/repair_cli.rs` — integration tests calling `diagnose()` / `rebuild()` / formatter functions directly

**Modify:**
- `src/lib.rs` — `pub mod cli;`
- `src/storage/mod.rs` — re-export `DiagnosticReport`, `DiagnosticStatus`, `PathInfo`, `SidecarFile`, `diagnose`, `rebuild_index`
- `src/main.rs` — add `Repair(RepairArgs)` variant + dispatch to `mem::cli::repair::run`
- `docs/mempalace-diff.md` — once landed, mark §8 row #4 ✅

---

## Task 1: `DiagnosticReport` types + serde tagging + JSON shape test

**Files:**
- Create: `src/storage/vector_index_diagnose.rs`
- Modify: `src/storage/mod.rs`
- Test: `tests/repair_cli.rs` (new)

- [ ] **Step 1: Write the failing JSON shape test**

Create `tests/repair_cli.rs`:

```rust
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
            status,
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test repair_cli -q`
Expected: compile error (`mem::storage::DiagnosticReport` undefined).

- [ ] **Step 3: Implement the types**

Create `src/storage/vector_index_diagnose.rs`:

```rust
use std::path::PathBuf;

use serde::Serialize;

use super::{VectorIndexFingerprint};

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
#[serde(rename_all = "PascalCase")]
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
```

In `src/storage/mod.rs`, add:

```rust
pub mod vector_index_diagnose;

pub use vector_index_diagnose::{
    DiagnosticReport, DiagnosticStatus, PathInfo, SidecarFile,
};
```

`VectorIndexFingerprint` already needs `Serialize`; if it doesn't have it yet, add `Serialize` to its derives in `src/storage/vector_index.rs`. Since it lives in a sibling module and the test imports it directly, this must compile. Verify the existing derives include `Serialize` — if not, add it (it already has `Clone + Debug` from §3 Task 7).

- [ ] **Step 4: Run tests**

Run: `cargo test --test repair_cli -q`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index_diagnose.rs src/storage/mod.rs src/storage/vector_index.rs tests/repair_cli.rs
git commit -m "feat(storage): DiagnosticReport types + serde tagging"
```

---

## Task 2: `diagnose()` — Healthy path (the happy case)

**Files:**
- Modify: `src/storage/vector_index_diagnose.rs`
- Modify: `tests/repair_cli.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/repair_cli.rs`:

```rust
use mem::config::EmbeddingSettings;
use mem::domain::memory::{
    IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode,
};
use mem::embedding::arc_embedding_provider;
use mem::service::{embedding_worker, MemoryService};
use mem::storage::{diagnose, DuckDbRepository, VectorIndex};
use std::sync::Arc;
use tempfile::tempdir;

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
    matches!(report.details, DiagnosticStatus::Healthy { rows: 1 });
    assert_eq!(report.details.exit_code(), 0);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test repair_cli diagnose_healthy -q`
Expected: compile error (`diagnose` not exported / not defined).

- [ ] **Step 3: Implement `diagnose()` for the Healthy path**

In `src/storage/vector_index_diagnose.rs`, append:

```rust
use std::path::Path;
use std::time::Instant;
use super::{DuckDbRepository, StorageError, VectorIndex};
use super::vector_index::sidecar_paths;
use super::vector_index::VectorIndexMeta;

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
    // For now, return Healthy only when both files exist and counts match.
    // Subsequent tasks will add the failure branches.

    if !idx_path.exists() {
        // placeholder — Task 3 will refine
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
    let index = match usearch::Index::new(&opts).and_then(|idx| {
        idx.reserve(meta.row_count.max(8))?;
        idx.load(idx_path.to_str().unwrap_or(""))?;
        Ok(idx)
    }) {
        Ok(i) => i,
        Err(e) => {
            return Ok(report_for(
                DiagnosticStatus::IndexCorrupt { reason: e.to_string() },
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
```

Re-export `diagnose` from `src/storage/mod.rs`:

```rust
pub use vector_index_diagnose::{
    diagnose, DiagnosticReport, DiagnosticStatus, PathInfo, SidecarFile,
};
```

> Note: this task implements ALL diagnose() branches in one go because they're heavily intertwined. Subsequent tasks (3-7) will add tests for each branch but the implementation lands here. This is a pragmatic deviation from strict-TDD-per-branch — the alternative would force seven near-duplicate diff iterations on the same function.

- [ ] **Step 4: Run tests**

Run: `cargo test --test repair_cli -q`
Expected: 2 passed (1 prior + 1 new).

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index_diagnose.rs src/storage/mod.rs tests/repair_cli.rs
git commit -m "feat(storage): diagnose() with all DiagnosticStatus branches"
```

---

## Task 3: `diagnose()` — file-level failure tests (SidecarMissing, MetaCorrupt)

**Files:**
- Modify: `tests/repair_cli.rs`

- [ ] **Step 1: Write the tests**

Append to `tests/repair_cli.rs`:

```rust
use mem::storage::sidecar_paths;

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
    matches!(report.details, DiagnosticStatus::MetaCorrupt { .. });
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test repair_cli sidecar_missing -q && cargo test --test repair_cli meta_corrupt -q`
Expected: 3 passed (these test paths already work because Task 2's `diagnose()` handles them).

- [ ] **Step 3: Commit**

```bash
git add tests/repair_cli.rs
git commit -m "test(diagnose): SidecarMissing + MetaCorrupt cases"
```

---

## Task 4: `diagnose()` — fingerprint mismatch tests (incl. dim==0)

**Files:**
- Modify: `tests/repair_cli.rs`

- [ ] **Step 1: Write the tests**

Append:

```rust
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
    matches!(report.details, DiagnosticStatus::FingerprintMismatch { .. });
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test repair_cli fingerprint -q`
Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add tests/repair_cli.rs
git commit -m "test(diagnose): FingerprintMismatch including zero-dim guard"
```

---

## Task 5: `diagnose()` — index binary corruption tests (IndexCorrupt, IndexMetaDrift)

**Files:**
- Modify: `tests/repair_cli.rs`

- [ ] **Step 1: Write the tests**

Append:

```rust
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
    matches!(report.details, DiagnosticStatus::IndexCorrupt { .. });
}

#[tokio::test]
async fn diagnose_reports_index_meta_drift_when_meta_lies_about_count() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("imd.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;
    let (_, meta_path) = sidecar_paths(&db);

    // Read meta, bump row_count by 1 (meta lies about count), write back
    let raw = std::fs::read(&meta_path).unwrap();
    let mut meta: mem::storage::VectorIndexMeta = serde_json::from_slice(&raw).unwrap();
    meta.row_count = meta.row_count + 5;  // index has fewer rows than meta claims
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
```

- [ ] **Step 2: Run**

Run: `cargo test --test repair_cli index_corrupt index_meta_drift -q`
Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add tests/repair_cli.rs
git commit -m "test(diagnose): IndexCorrupt + IndexMetaDrift cases"
```

---

## Task 6: `diagnose()` — DbDrift test

**Files:**
- Modify: `tests/repair_cli.rs`

- [ ] **Step 1: Write the test**

Append:

```rust
#[tokio::test]
async fn diagnose_reports_db_drift_when_db_has_extra_rows() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("dd.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;

    // Bypass the worker — add a memory_embeddings row directly so the index can't
    // know about it. The seed_memory_embedding_for_test helper from §3 Task 8
    // is exactly this.
    repo.seed_memory_embedding_for_test("ghost_row", "t", &vec![0.0f32; 256])
        .await
        .unwrap();

    let settings = EmbeddingSettings::development_defaults();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let report = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(report.status, "drift");
    match report.details {
        DiagnosticStatus::DbDrift { meta_count, db_count } => {
            assert_eq!(meta_count, 1);
            assert_eq!(db_count, 2);
        }
        other => panic!("expected DbDrift, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test repair_cli db_drift -q`
Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add tests/repair_cli.rs
git commit -m "test(diagnose): DbDrift case"
```

---

## Task 7: `rebuild_index()` function

**Files:**
- Modify: `src/storage/vector_index_diagnose.rs`
- Modify: `src/storage/mod.rs`
- Modify: `tests/repair_cli.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/repair_cli.rs`:

```rust
use mem::storage::rebuild_index;

#[tokio::test]
async fn rebuild_index_recovers_from_drift() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("rb.duckdb");
    let (repo, _idx) = seed_one_row_with_index(&db).await;

    // Induce DbDrift by bypassing service
    repo.seed_memory_embedding_for_test("orphan", "t", &vec![0.0f32; 256])
        .await
        .unwrap();

    let settings = EmbeddingSettings::development_defaults();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };

    let pre = diagnose(&repo, &db, &fp).await.unwrap();
    matches!(pre.details, DiagnosticStatus::DbDrift { .. });

    let new_idx = rebuild_index(&repo, &db, &fp).await.unwrap();
    assert_eq!(new_idx.size(), 2);

    let post = diagnose(&repo, &db, &fp).await.unwrap();
    assert_eq!(post.status, "healthy");
    match post.details {
        DiagnosticStatus::Healthy { rows } => assert_eq!(rows, 2),
        other => panic!("expected Healthy after rebuild, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test repair_cli rebuild_index -q`
Expected: compile error (`rebuild_index` not exported).

- [ ] **Step 3: Implement `rebuild_index()`**

Append to `src/storage/vector_index_diagnose.rs`:

```rust
use std::sync::Arc;

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
```

In `src/storage/mod.rs`, extend the re-export:

```rust
pub use vector_index_diagnose::{
    diagnose, rebuild_index, DiagnosticReport, DiagnosticStatus, PathInfo, SidecarFile,
};
```

- [ ] **Step 4: Run**

Run: `cargo test --test repair_cli rebuild_index -q`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index_diagnose.rs src/storage/mod.rs tests/repair_cli.rs
git commit -m "feat(storage): rebuild_index() force-rebuilds sidecar from DuckDB"
```

---

## Task 8: CLI — `Repair` subcommand wiring (clap parsing only)

**Files:**
- Create: `src/cli/mod.rs`
- Create: `src/cli/repair.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add `cli` module**

Create `src/cli/mod.rs`:

```rust
pub mod repair;
```

Create `src/cli/repair.rs` with a stub `run` function:

```rust
use clap::Args;

#[derive(Debug, Args)]
pub struct RepairArgs {
    #[command(flatten)]
    pub mode: RepairMode,
    /// Output structured JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[group(multiple = false)]
pub struct RepairMode {
    /// Read-only health check (default).
    #[arg(long)]
    pub check: bool,
    /// Force rebuild from DuckDB (requires `mem serve` to be stopped).
    #[arg(long)]
    pub rebuild: bool,
}

/// Entry point for `mem repair`. Returns the process exit code.
pub async fn run(args: RepairArgs) -> i32 {
    // Subsequent tasks fill this in.
    if args.mode.rebuild {
        eprintln!("rebuild not yet implemented");
        2
    } else {
        eprintln!("check not yet implemented");
        2
    }
}
```

In `src/lib.rs`, add:

```rust
pub mod cli;
```

In `src/main.rs`, modify the `Command` enum:

```rust
use mem::cli::repair::RepairArgs;

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP memory service (default).
    Serve,
    /// Run the MCP (Model Context Protocol) stdio server.
    Mcp,
    /// Diagnose or rebuild the vector index sidecar.
    Repair(RepairArgs),
}
```

In `main`, dispatch the new variant:

```rust
match command {
    Command::Serve => run_serve().await,
    Command::Mcp => mcp::run().await,
    Command::Repair(args) => {
        let code = mem::cli::repair::run(args).await;
        std::process::exit(code);
    }
}
```

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: clean compile.

- [ ] **Step 3: Smoke**

```bash
cargo run -- repair --help
```

Expected: clap usage summary showing `--check`, `--rebuild`, `--json`.

```bash
cargo run -- repair --check --rebuild
```

Expected: clap rejects mutually-exclusive flags with exit 2.

- [ ] **Step 4: Commit**

```bash
git add src/cli/ src/lib.rs src/main.rs
git commit -m "feat(cli): scaffold mem repair subcommand"
```

---

## Task 9: CLI — `--check` text output + exit code

**Files:**
- Modify: `src/cli/repair.rs`
- Modify: `tests/repair_cli.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/repair_cli.rs`:

```rust
use mem::cli::repair::{format_check_text, format_rebuild_text};

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
        details: DiagnosticStatus::DbDrift { meta_count: 1247, db_count: 1250 },
        paths: paths(),
        elapsed_ms: 12,
    };
    let s = format_check_text(&report);
    assert!(s.contains("Drift detected"), "{s}");
    assert!(s.contains("1247"), "{s}");
    assert!(s.contains("1250"), "{s}");
    assert!(s.contains("mem repair --rebuild"), "should suggest rebuild: {s}");
}

#[test]
fn format_check_text_for_index_corrupt() {
    let report = DiagnosticReport {
        status: "corrupt",
        details: DiagnosticStatus::IndexCorrupt { reason: "boom".into() },
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
        details: DiagnosticStatus::DbUnavailable { reason: "file is locked".into() },
        paths: paths(),
        elapsed_ms: 2,
    };
    let s = format_check_text(&report);
    assert!(s.contains("Could not open DB"), "{s}");
    assert!(s.contains("mem serve"), "should hint at running service: {s}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test repair_cli format_check_text -q`
Expected: compile error (`format_check_text` undefined).

- [ ] **Step 3: Implement text formatter**

In `src/cli/repair.rs`, add:

```rust
use mem_storage_alias::{DiagnosticReport, DiagnosticStatus, SidecarFile};
// Use the actual path; the alias is just for clarity in this snippet.
// Replace with: use crate::storage::{DiagnosticReport, DiagnosticStatus, SidecarFile};

pub fn format_check_text(report: &DiagnosticReport) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    match &report.details {
        DiagnosticStatus::Healthy { rows } => {
            writeln!(
                &mut s,
                "✅ Healthy: {} rows. Sidecar at {}",
                rows,
                report.paths.index.display()
            ).unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::SidecarMissing { which } => {
            let name = match which {
                SidecarFile::Index => "index file",
                SidecarFile::Meta => "metadata file",
            };
            writeln!(&mut s, "❌ Sidecar {name} is missing.").unwrap();
            writeln!(&mut s, "   → Run `mem repair --rebuild` to recreate from DuckDB.").unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::MetaCorrupt { reason } => {
            writeln!(&mut s, "❌ Metadata file is corrupt: {reason}").unwrap();
            writeln!(&mut s, "   → Run `mem repair --rebuild` to recreate from DuckDB.").unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::FingerprintMismatch { stored, current } => {
            writeln!(
                &mut s,
                "❌ Fingerprint mismatch: stored=({}, {}, dim={}) current=({}, {}, dim={})",
                stored.provider, stored.model, stored.dim,
                current.provider, current.model, current.dim,
            ).unwrap();
            writeln!(&mut s, "   → Run `mem repair --rebuild` to recreate with the current config.").unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::IndexCorrupt { reason } => {
            writeln!(&mut s, "❌ Index file is corrupt: {reason}").unwrap();
            writeln!(&mut s, "   → Run `mem repair --rebuild` to recreate from DuckDB.").unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::IndexMetaDrift { index_size, meta_count } => {
            writeln!(&mut s, "❌ Drift detected: index has {index_size} vectors but meta claims {meta_count}.").unwrap();
            writeln!(&mut s, "   → Run `mem repair --rebuild` to reconcile.").unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::DbDrift { meta_count, db_count } => {
            writeln!(&mut s, "❌ Drift detected: meta.row_count={meta_count} but db has {db_count}.").unwrap();
            writeln!(&mut s, "   → Run `mem repair --rebuild` to reconcile.").unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::DbUnavailable { reason } => {
            writeln!(&mut s, "❌ Could not open DB at {}: {reason}",
                report.paths.db.display()).unwrap();
            writeln!(&mut s, "   Is `mem serve` running? Stop the service before running this command.").unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
    }
    s
}
```

Note: replace the alias-style import with the real path:

```rust
use crate::storage::{DiagnosticReport, DiagnosticStatus, SidecarFile};
```

(There's no `mem_storage_alias`; that was scratch.)

- [ ] **Step 4: Run**

Run: `cargo test --test repair_cli format_check_text -q`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src/cli/repair.rs tests/repair_cli.rs
git commit -m "feat(cli): format_check_text for human-readable check output"
```

---

## Task 10: CLI — `--check` JSON output + `format_rebuild_text/json`

**Files:**
- Modify: `src/cli/repair.rs`
- Modify: `tests/repair_cli.rs`

- [ ] **Step 1: Write the failing tests**

Append:

```rust
use mem::cli::repair::{format_check_json, format_rebuild_json, RebuildOutcome};

#[test]
fn format_check_json_emits_top_level_status_and_exit_code() {
    let report = DiagnosticReport {
        status: "drift",
        details: DiagnosticStatus::DbDrift { meta_count: 5, db_count: 7 },
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
        details: DiagnosticStatus::DbUnavailable { reason: "locked".into() },
        paths: paths(),
        elapsed_ms: 1,
    };
    let v = format_check_json(&report);
    assert_eq!(v["exit_code"], 2);
}

#[test]
fn format_rebuild_text_for_success() {
    let outcome = RebuildOutcome::Rebuilt { rows: 1247, paths: paths(), elapsed_ms: 832 };
    let s = format_rebuild_text(&outcome);
    assert!(s.contains("1247"), "{s}");
    assert!(s.contains("832"), "{s}");
    assert!(s.contains("Rebuilt"), "{s}");
}

#[test]
fn format_rebuild_json_for_success() {
    let outcome = RebuildOutcome::Rebuilt { rows: 1247, paths: paths(), elapsed_ms: 832 };
    let v = format_rebuild_json(&outcome);
    assert_eq!(v["command"], "rebuild");
    assert_eq!(v["status"], "rebuilt");
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["rows"], 1247);
    assert_eq!(v["elapsed_ms"], 832);
}

#[test]
fn format_rebuild_json_for_db_unavailable() {
    let outcome = RebuildOutcome::DbUnavailable { reason: "locked".into(), paths: paths() };
    let v = format_rebuild_json(&outcome);
    assert_eq!(v["status"], "db_unavailable");
    assert_eq!(v["exit_code"], 2);
}

#[test]
fn format_rebuild_json_for_failed() {
    let outcome = RebuildOutcome::Failed { reason: "disk full".into(), paths: paths() };
    let v = format_rebuild_json(&outcome);
    assert_eq!(v["status"], "rebuild_failed");
    assert_eq!(v["exit_code"], 2);
    assert_eq!(v["details"]["reason"], "disk full");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test repair_cli format_check_json format_rebuild -q`
Expected: compile errors.

- [ ] **Step 3: Implement formatters + RebuildOutcome**

In `src/cli/repair.rs`, append:

```rust
use serde_json::{json, Value};
use crate::storage::PathInfo;

#[derive(Debug, Clone)]
pub enum RebuildOutcome {
    Rebuilt { rows: usize, paths: PathInfo, elapsed_ms: u64 },
    DbUnavailable { reason: String, paths: PathInfo },
    Failed { reason: String, paths: PathInfo },
}

impl RebuildOutcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            RebuildOutcome::Rebuilt { .. } => 0,
            _ => 2,
        }
    }
    pub fn coarse_status(&self) -> &'static str {
        match self {
            RebuildOutcome::Rebuilt { .. } => "rebuilt",
            RebuildOutcome::DbUnavailable { .. } => "db_unavailable",
            RebuildOutcome::Failed { .. } => "rebuild_failed",
        }
    }
}

pub fn format_check_json(report: &DiagnosticReport) -> Value {
    json!({
        "command": "check",
        "status": report.status,
        "exit_code": report.details.exit_code(),
        "details": serde_json::to_value(&report.details).unwrap(),
        "paths": serde_json::to_value(&report.paths).unwrap(),
        "elapsed_ms": report.elapsed_ms,
    })
}

pub fn format_rebuild_text(outcome: &RebuildOutcome) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    match outcome {
        RebuildOutcome::Rebuilt { rows, paths, elapsed_ms } => {
            writeln!(&mut s, "🔨 Rebuilding vector index from {}...", paths.db.display()).unwrap();
            writeln!(&mut s, "✅ Rebuilt: {rows} rows in {elapsed_ms}ms.").unwrap();
            writeln!(&mut s, "   New sidecar at {}", paths.index.display()).unwrap();
        }
        RebuildOutcome::DbUnavailable { reason, paths } => {
            writeln!(&mut s, "❌ Could not open DB at {}: {reason}", paths.db.display()).unwrap();
            writeln!(&mut s, "   Is `mem serve` running? Stop the service before running this command.").unwrap();
        }
        RebuildOutcome::Failed { reason, paths } => {
            writeln!(&mut s, "❌ Rebuild failed: {reason}").unwrap();
            writeln!(&mut s, "   DB at {} is unchanged; sidecar may be partially deleted.", paths.db.display()).unwrap();
        }
    }
    s
}

pub fn format_rebuild_json(outcome: &RebuildOutcome) -> Value {
    match outcome {
        RebuildOutcome::Rebuilt { rows, paths, elapsed_ms } => json!({
            "command": "rebuild",
            "status": "rebuilt",
            "exit_code": 0,
            "rows": rows,
            "paths": serde_json::to_value(paths).unwrap(),
            "elapsed_ms": elapsed_ms,
        }),
        RebuildOutcome::DbUnavailable { reason, paths } => json!({
            "command": "rebuild",
            "status": "db_unavailable",
            "exit_code": 2,
            "details": {"reason": reason},
            "paths": serde_json::to_value(paths).unwrap(),
        }),
        RebuildOutcome::Failed { reason, paths } => json!({
            "command": "rebuild",
            "status": "rebuild_failed",
            "exit_code": 2,
            "details": {"reason": reason},
            "paths": serde_json::to_value(paths).unwrap(),
        }),
    }
}
```

- [ ] **Step 4: Run**

Run: `cargo test --test repair_cli format_ -q`
Expected: 9 passed (4 from Task 9 + 5 new).

- [ ] **Step 5: Commit**

```bash
git add src/cli/repair.rs tests/repair_cli.rs
git commit -m "feat(cli): JSON formatters + RebuildOutcome type"
```

---

## Task 11: CLI — wire `run()` end-to-end (config → diagnose/rebuild → format → exit)

**Files:**
- Modify: `src/cli/repair.rs`

- [ ] **Step 1: Implement `run()`**

In `src/cli/repair.rs`, replace the stub `run()` with:

```rust
use crate::config::Config;
use crate::storage::{diagnose, rebuild_index, DiagnosticStatus, DuckDbRepository, PathInfo, VectorIndexFingerprint, sidecar_paths};
use std::time::Instant;

pub async fn run(args: RepairArgs) -> i32 {
    // Resolve config.
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return emit_config_error(&args, &e.to_string());
        }
    };

    let fp = VectorIndexFingerprint {
        provider: config.embedding.job_provider_id().to_string(),
        model: config.embedding.model.clone(),
        dim: config.embedding.dim,
    };

    if args.mode.rebuild {
        run_rebuild(&config, &fp, args.json).await
    } else {
        run_check(&config, &fp, args.json).await
    }
}

async fn run_check(
    config: &Config,
    fp: &VectorIndexFingerprint,
    as_json: bool,
) -> i32 {
    let started = Instant::now();
    let repo = match DuckDbRepository::open(&config.db_path).await {
        Ok(r) => r,
        Err(e) => {
            let (idx_path, meta_path) = sidecar_paths(&config.db_path);
            let report = crate::storage::DiagnosticReport {
                status: "db_unavailable",
                details: DiagnosticStatus::DbUnavailable { reason: e.to_string() },
                paths: PathInfo {
                    db: config.db_path.clone(),
                    index: idx_path,
                    meta: meta_path,
                },
                elapsed_ms: started.elapsed().as_millis() as u64,
            };
            return emit_check(&report, as_json);
        }
    };

    let report = match diagnose(&repo, &config.db_path, fp).await {
        Ok(r) => r,
        Err(e) => {
            let (idx_path, meta_path) = sidecar_paths(&config.db_path);
            let report = crate::storage::DiagnosticReport {
                status: "db_unavailable",
                details: DiagnosticStatus::DbUnavailable { reason: e.to_string() },
                paths: PathInfo {
                    db: config.db_path.clone(),
                    index: idx_path,
                    meta: meta_path,
                },
                elapsed_ms: started.elapsed().as_millis() as u64,
            };
            return emit_check(&report, as_json);
        }
    };
    emit_check(&report, as_json)
}

async fn run_rebuild(
    config: &Config,
    fp: &VectorIndexFingerprint,
    as_json: bool,
) -> i32 {
    let (idx_path, meta_path) = sidecar_paths(&config.db_path);
    let paths = PathInfo {
        db: config.db_path.clone(),
        index: idx_path,
        meta: meta_path,
    };

    let repo = match DuckDbRepository::open(&config.db_path).await {
        Ok(r) => r,
        Err(e) => {
            return emit_rebuild(
                &RebuildOutcome::DbUnavailable { reason: e.to_string(), paths },
                as_json,
            );
        }
    };

    let started = Instant::now();
    match rebuild_index(&repo, &config.db_path, fp).await {
        Ok(idx) => {
            let outcome = RebuildOutcome::Rebuilt {
                rows: idx.size(),
                paths,
                elapsed_ms: started.elapsed().as_millis() as u64,
            };
            emit_rebuild(&outcome, as_json)
        }
        Err(e) => {
            emit_rebuild(
                &RebuildOutcome::Failed { reason: e.to_string(), paths },
                as_json,
            )
        }
    }
}

fn emit_check(report: &DiagnosticReport, as_json: bool) -> i32 {
    if as_json {
        let v = format_check_json(report);
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        print!("{}", format_check_text(report));
    }
    report.details.exit_code()
}

fn emit_rebuild(outcome: &RebuildOutcome, as_json: bool) -> i32 {
    if as_json {
        let v = format_rebuild_json(outcome);
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        print!("{}", format_rebuild_text(outcome));
    }
    outcome.exit_code()
}

fn emit_config_error(args: &RepairArgs, reason: &str) -> i32 {
    if args.json {
        let v = json!({
            "command": if args.mode.rebuild { "rebuild" } else { "check" },
            "status": "config_error",
            "exit_code": 2,
            "details": {"reason": reason},
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        eprintln!("❌ Invalid configuration: {reason}");
    }
    2
}
```

- [ ] **Step 2: Build + smoke**

```bash
cargo build
MEM_DB_PATH=/tmp/mem-repair-smoke.duckdb cargo run -- repair --check
```

Expected: prints either "✅ Healthy: 0 rows" (if the DB is fresh) or a `SidecarMissing` if the DB had rows but no sidecar (then suggest --rebuild).

```bash
MEM_DB_PATH=/tmp/mem-repair-smoke.duckdb cargo run -- repair --rebuild
```

Expected: "🔨 Rebuilding... ✅ Rebuilt: N rows".

```bash
MEM_DB_PATH=/tmp/mem-repair-smoke.duckdb cargo run -- repair --check --json
```

Expected: pretty JSON with `"command": "check"`, `"status": "healthy"`, etc.

- [ ] **Step 3: Run all tests**

```bash
cargo test -q
```

Expected: full suite passes.

- [ ] **Step 4: Commit**

```bash
git add src/cli/repair.rs
git commit -m "feat(cli): wire mem repair end-to-end (config → diagnose/rebuild → emit)"
```

---

## Task 12: Final verification + close §8 #4

**Files:**
- Modify: `docs/mempalace-diff.md`

- [ ] **Step 1: Run full verification**

```bash
cargo test -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

All three must be clean. If `cargo fmt --check` fails, run `cargo fmt`, commit as `chore: cargo fmt`.

- [ ] **Step 2: Manual smoke run**

```bash
MEM_DB_PATH=/tmp/mem-final-smoke.duckdb cargo run -- repair --check --json
```

Expected: valid JSON with `"status": "healthy"` (or a known-correct other status if there's existing data).

- [ ] **Step 3: Mark §8 row #4 complete**

In `docs/mempalace-diff.md`, change the row from:

```markdown
| 4 | ⚙️ | HNSW 健康度自检 + repair CLI | 🟠 配套 #3 | S（4h） | 低 | 新增 `bin/mem-repair` |
```

to:

```markdown
| 4 | ⚙️ | ✅ HNSW 健康度自检 + repair 子命令（`mem repair --check` / `--rebuild`，JSON 输出可选）| 🟠 配套 #3 | S（4h） | 低 | `src/cli/repair.rs`、`src/storage/vector_index_diagnose.rs` |
```

The change reflects the deviation from `bin/mem-repair` to a subcommand on the existing `mem` binary.

- [ ] **Step 4: Commit**

```bash
git add docs/mempalace-diff.md
git commit -m "docs(mempalace-diff): mark §8 #4 complete (closes mempalace-diff §8 #4)"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage check:** every section of `2026-04-28-mem-repair-cli-design.md` maps to one or more tasks above. Task 1 covers types + serde. Tasks 2–6 cover all eight `DiagnosticStatus` variants (Task 2 implements the full match in one go, Tasks 3–6 add per-variant tests). Task 7 covers `rebuild_index`. Tasks 8–11 cover the CLI surface. Task 12 closes the roadmap.
- **`config_error`** is in `--json` output only; the text path uses stderr with a clear message. This matches the spec's exit-code matrix.
- **`DbUnavailable`** is generated at TWO call sites: failure to open the repo (lock contention), and any `StorageError` from `diagnose()`. Both should produce identical JSON shapes.
- **The `report.status: &'static str`** field is set from `coarse_status()` at construction time. Don't change it post-hoc — derive once and freeze.
- **The `RebuildOutcome::Rebuilt::rows`** field uses `idx.size()` after the rebuild — that's `usize`, matches the JSON shape.
- **Smoke run order matters:** running `--rebuild` before `--check` on a fresh DB is fine, but if you run `--check` first and the sidecar exists, the check will pass and rebuild will overwrite a healthy state. That's expected for a destructive operation.
- **No HTTP smoke needed:** the repair command never starts the server, never opens port 3000.
