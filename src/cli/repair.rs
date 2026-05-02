use crate::config::Config;
use crate::storage::{
    diagnose, diagnose_transcripts, rebuild_index, rebuild_transcripts_index, sidecar_paths,
    transcript_sidecar_paths, DiagnosticReport, DiagnosticStatus, DuckDbRepository, PathInfo,
    SidecarFile, VectorIndexFingerprint,
};
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
    /// Force rebuild of the HNSW vector-index sidecar from DuckDB
    /// (requires `mem serve` to be stopped).
    #[arg(long)]
    pub rebuild: bool,
    /// Re-derive every memory→{entity,memory} edge from `memories` using the
    /// production extract_graph_edge_drafts → resolve_drafts_to_edges path.
    /// Sweeps any pre-migration legacy `project:foo`/`topic:rust` edges.
    /// Idempotent, but requires `mem serve` to be stopped — DuckDB is
    /// single-writer.
    #[arg(long)]
    pub rebuild_graph: bool,
}

/// Render a human-readable summary of a [`DiagnosticReport`].
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
            )
            .unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::SidecarMissing { which } => {
            let name = match which {
                SidecarFile::Index => "index file",
                SidecarFile::Meta => "metadata file",
            };
            writeln!(&mut s, "❌ Sidecar {name} is missing.").unwrap();
            writeln!(
                &mut s,
                "   → Run `mem repair --rebuild` to recreate from DuckDB."
            )
            .unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::MetaCorrupt { reason } => {
            writeln!(&mut s, "❌ Metadata file is corrupt: {reason}").unwrap();
            writeln!(
                &mut s,
                "   → Run `mem repair --rebuild` to recreate from DuckDB."
            )
            .unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::FingerprintMismatch { stored, current } => {
            writeln!(
                &mut s,
                "❌ Fingerprint mismatch: stored=({}, {}, dim={}) current=({}, {}, dim={})",
                stored.provider,
                stored.model,
                stored.dim,
                current.provider,
                current.model,
                current.dim,
            )
            .unwrap();
            writeln!(
                &mut s,
                "   → Run `mem repair --rebuild` to recreate with the current config."
            )
            .unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::IndexCorrupt { reason } => {
            writeln!(&mut s, "❌ Index file is corrupt: {reason}").unwrap();
            writeln!(
                &mut s,
                "   → Run `mem repair --rebuild` to recreate from DuckDB."
            )
            .unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::IndexMetaDrift {
            index_size,
            meta_count,
        } => {
            writeln!(
                &mut s,
                "❌ Drift detected: index has {index_size} vectors but meta claims {meta_count}."
            )
            .unwrap();
            writeln!(&mut s, "   → Run `mem repair --rebuild` to reconcile.").unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::DbDrift {
            meta_count,
            db_count,
        } => {
            writeln!(
                &mut s,
                "❌ Drift detected: meta.row_count={meta_count} but db has {db_count}."
            )
            .unwrap();
            writeln!(&mut s, "   → Run `mem repair --rebuild` to reconcile.").unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
        DiagnosticStatus::DbUnavailable { reason } => {
            writeln!(
                &mut s,
                "❌ Could not open DB at {}: {reason}",
                report.paths.db.display()
            )
            .unwrap();
            writeln!(
                &mut s,
                "   Is `mem serve` running? Stop the service before running this command."
            )
            .unwrap();
            writeln!(&mut s, "   elapsed={}ms", report.elapsed_ms).unwrap();
        }
    }
    s
}

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub enum RebuildOutcome {
    Rebuilt {
        rows: usize,
        paths: PathInfo,
        elapsed_ms: u64,
    },
    DbUnavailable {
        reason: String,
        paths: PathInfo,
    },
    Failed {
        reason: String,
        paths: PathInfo,
    },
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

/// Outcome of `mem repair --rebuild-graph`. Process exit code mirrors the
/// `RebuildOutcome` convention: 0 on success, 2 on any failure.
#[derive(Debug, Clone)]
pub enum RebuildGraphOutcome {
    Rebuilt {
        rebuilt_memory_count: usize,
        new_edge_count: usize,
        elapsed_ms: u64,
    },
    Failed {
        reason: String,
    },
}

impl RebuildGraphOutcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            RebuildGraphOutcome::Rebuilt { .. } => 0,
            RebuildGraphOutcome::Failed { .. } => 2,
        }
    }

    pub fn coarse_status(&self) -> &'static str {
        match self {
            RebuildGraphOutcome::Rebuilt { .. } => "rebuilt",
            RebuildGraphOutcome::Failed { .. } => "rebuild_failed",
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
        RebuildOutcome::Rebuilt {
            rows,
            paths,
            elapsed_ms,
        } => {
            writeln!(
                &mut s,
                "🔨 Rebuilding vector index from {}...",
                paths.db.display()
            )
            .unwrap();
            writeln!(&mut s, "✅ Rebuilt: {rows} rows in {elapsed_ms}ms.").unwrap();
            writeln!(&mut s, "   New sidecar at {}", paths.index.display()).unwrap();
        }
        RebuildOutcome::DbUnavailable { reason, paths } => {
            writeln!(
                &mut s,
                "❌ Could not open DB at {}: {reason}",
                paths.db.display()
            )
            .unwrap();
            writeln!(
                &mut s,
                "   Is `mem serve` running? Stop the service before running this command."
            )
            .unwrap();
        }
        RebuildOutcome::Failed { reason, paths } => {
            writeln!(&mut s, "❌ Rebuild failed: {reason}").unwrap();
            writeln!(
                &mut s,
                "   DB at {} is unchanged; sidecar may be partially deleted.",
                paths.db.display()
            )
            .unwrap();
        }
    }
    s
}

pub fn format_rebuild_json(outcome: &RebuildOutcome) -> Value {
    match outcome {
        RebuildOutcome::Rebuilt {
            rows,
            paths,
            elapsed_ms,
        } => json!({
            "command": "rebuild",
            "status": outcome.coarse_status(),
            "exit_code": outcome.exit_code(),
            "rows": rows,
            "paths": serde_json::to_value(paths).unwrap(),
            "elapsed_ms": elapsed_ms,
        }),
        RebuildOutcome::DbUnavailable { reason, paths } => json!({
            "command": "rebuild",
            "status": outcome.coarse_status(),
            "exit_code": outcome.exit_code(),
            "details": {"reason": reason},
            "paths": serde_json::to_value(paths).unwrap(),
        }),
        RebuildOutcome::Failed { reason, paths } => json!({
            "command": "rebuild",
            "status": outcome.coarse_status(),
            "exit_code": outcome.exit_code(),
            "details": {"reason": reason},
            "paths": serde_json::to_value(paths).unwrap(),
        }),
    }
}

/// Aggregated text output for `mem repair --check`. Two labelled sections,
/// one per sidecar.
pub fn format_aggregate_check_text(
    memories: &DiagnosticReport,
    transcripts: &DiagnosticReport,
) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    writeln!(&mut s, "=== Memories sidecar ===").unwrap();
    s.push_str(&format_check_text(memories));
    writeln!(&mut s, "\n=== Transcripts sidecar ===").unwrap();
    s.push_str(&format_check_text(transcripts));
    s
}

/// Aggregated text output for `mem repair --rebuild`.
pub fn format_aggregate_rebuild_text(
    memories: &RebuildOutcome,
    transcripts: &RebuildOutcome,
) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    writeln!(&mut s, "=== Memories sidecar ===").unwrap();
    s.push_str(&format_rebuild_text(memories));
    writeln!(&mut s, "\n=== Transcripts sidecar ===").unwrap();
    s.push_str(&format_rebuild_text(transcripts));
    s
}

/// Aggregated JSON output for `mem repair --check`.
pub fn format_aggregate_check_json(
    memories: &DiagnosticReport,
    transcripts: &DiagnosticReport,
) -> Value {
    let exit = aggregate_exit_code(
        memories.details.exit_code(),
        transcripts.details.exit_code(),
    );
    json!({
        "command": "check",
        "memories": format_check_json(memories),
        "transcripts": format_check_json(transcripts),
        "exit_code": exit,
    })
}

/// Aggregated JSON output for `mem repair --rebuild`.
pub fn format_aggregate_rebuild_json(
    memories: &RebuildOutcome,
    transcripts: &RebuildOutcome,
) -> Value {
    let exit = aggregate_exit_code(memories.exit_code(), transcripts.exit_code());
    json!({
        "command": "rebuild",
        "memories": format_rebuild_json(memories),
        "transcripts": format_rebuild_json(transcripts),
        "exit_code": exit,
    })
}

/// Worst-of-two exit code aggregator: any unhealthy pipeline propagates.
pub fn aggregate_exit_code(a: i32, b: i32) -> i32 {
    a.max(b)
}

/// Entry point for `mem repair`. Returns the process exit code.
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

    if args.mode.rebuild_graph {
        let outcome = match compute_rebuild_graph_outcome(&config).await {
            Ok(o) => o,
            Err(e) => RebuildGraphOutcome::Failed {
                reason: e.to_string(),
            },
        };
        emit_rebuild_graph(&outcome, args.json)
    } else if args.mode.rebuild {
        let (mem_outcome, tr_outcome) = compute_rebuild_outcomes(&config, &fp).await;
        emit_aggregate_rebuild(&mem_outcome, &tr_outcome, args.json)
    } else {
        let (mem_report, tr_report) = compute_check_reports(&config, &fp).await;
        emit_aggregate_check(&mem_report, &tr_report, args.json)
    }
}

/// Run the graph rebuild end-to-end against `config.db_path`.
///
/// Walks every memory in the DB through the **same** production code path
/// used by `MemoryService::ingest`:
///
/// 1. `extract_graph_edge_drafts(memory)` — pure (no DB), per Task 7.
/// 2. `resolve_drafts_to_edges(drafts, &repo, tenant, now)` — calls
///    `EntityRegistry::resolve_or_create` for each `EntityRef` draft.
/// 3. Insert resolved edges atomically via
///    `DuckDbRepository::rebuild_tenant_graph` (delete + bulk insert in one
///    transaction per tenant).
///
/// Each tenant's rebuild is atomic; partial-failure across tenants is
/// possible (the surviving tenants are correctly rebuilt; failing tenant
/// left in pre-rebuild state).
///
/// Idempotent: re-running produces the same edge set because
/// `resolve_or_create` returns existing entity_ids when the alias is already
/// known. Requires `mem serve` to be stopped (DuckDB is single-writer).
pub async fn compute_rebuild_graph_outcome(
    config: &Config,
) -> Result<RebuildGraphOutcome, anyhow::Error> {
    use crate::pipeline::ingest::extract_graph_edge_drafts;
    use crate::service::memory_service::resolve_drafts_to_edges;
    use crate::storage::current_timestamp;

    let started = std::time::Instant::now();
    let repo = DuckDbRepository::open(&config.db_path).await?;
    let now = current_timestamp();

    let tenants = repo.list_distinct_memory_tenants().await?;
    let mut total_memories = 0usize;
    let mut total_edges = 0usize;

    for tenant in tenants {
        let memories = repo.list_memories_for_tenant(&tenant).await?;

        // Collect all edges for this tenant first, then atomically replace the
        // old graph in a single transaction.  This avoids a partially-demolished
        // tenant graph on mid-rebuild kill.
        let mut tenant_edges = Vec::new();
        for memory in &memories {
            let drafts = extract_graph_edge_drafts(memory);
            let edges = resolve_drafts_to_edges(drafts, &repo, &tenant, &now).await?;
            tenant_edges.extend(edges);
        }

        let inserted = repo.rebuild_tenant_graph(&tenant, &tenant_edges).await?;
        total_edges += inserted;
        total_memories += memories.len();
    }

    Ok(RebuildGraphOutcome::Rebuilt {
        rebuilt_memory_count: total_memories,
        new_edge_count: total_edges,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn emit_rebuild_graph(outcome: &RebuildGraphOutcome, as_json: bool) -> i32 {
    if as_json {
        let v = format_rebuild_graph_json(outcome);
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        print!("{}", format_rebuild_graph_text(outcome));
    }
    outcome.exit_code()
}

/// Render a human-readable summary of a [`RebuildGraphOutcome`].
pub fn format_rebuild_graph_text(outcome: &RebuildGraphOutcome) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    match outcome {
        RebuildGraphOutcome::Rebuilt {
            rebuilt_memory_count,
            new_edge_count,
            elapsed_ms,
        } => {
            writeln!(&mut s, "Rebuilt graph edges from memories.").unwrap();
            writeln!(
                &mut s,
                "   memories scanned : {rebuilt_memory_count}\n   edges written    : {new_edge_count}\n   elapsed          : {elapsed_ms}ms"
            )
            .unwrap();
        }
        RebuildGraphOutcome::Failed { reason } => {
            writeln!(&mut s, "Rebuild-graph failed: {reason}").unwrap();
            writeln!(
                &mut s,
                "   Is `mem serve` running? Stop the service before running this command."
            )
            .unwrap();
        }
    }
    s
}

/// Render a structured JSON summary of a [`RebuildGraphOutcome`].
pub fn format_rebuild_graph_json(outcome: &RebuildGraphOutcome) -> Value {
    match outcome {
        RebuildGraphOutcome::Rebuilt {
            rebuilt_memory_count,
            new_edge_count,
            elapsed_ms,
        } => json!({
            "command": "rebuild-graph",
            "status": outcome.coarse_status(),
            "exit_code": outcome.exit_code(),
            "rebuilt_memory_count": rebuilt_memory_count,
            "new_edge_count": new_edge_count,
            "elapsed_ms": elapsed_ms,
        }),
        RebuildGraphOutcome::Failed { reason } => json!({
            "command": "rebuild-graph",
            "status": outcome.coarse_status(),
            "exit_code": outcome.exit_code(),
            "details": {"reason": reason},
        }),
    }
}

/// Compute both per-pipeline diagnostic reports without printing. Useful for
/// tests; the CLI entry point wraps this with `emit_aggregate_check`.
pub async fn compute_check_reports(
    config: &Config,
    fp: &VectorIndexFingerprint,
) -> (DiagnosticReport, DiagnosticReport) {
    let started = std::time::Instant::now();
    let repo = match DuckDbRepository::open(&config.db_path).await {
        Ok(r) => r,
        Err(e) => {
            // DB unavailable: same error for both pipelines, but with their
            // respective sidecar paths.
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let (mem_idx, mem_meta) = sidecar_paths(&config.db_path);
            let (tr_idx, tr_meta) = transcript_sidecar_paths(&config.db_path);
            let mem_report = DiagnosticReport {
                status: "db_unavailable",
                details: DiagnosticStatus::DbUnavailable {
                    reason: e.to_string(),
                },
                paths: PathInfo {
                    db: config.db_path.clone(),
                    index: mem_idx,
                    meta: mem_meta,
                },
                elapsed_ms,
            };
            let tr_report = DiagnosticReport {
                status: "db_unavailable",
                details: DiagnosticStatus::DbUnavailable {
                    reason: e.to_string(),
                },
                paths: PathInfo {
                    db: config.db_path.clone(),
                    index: tr_idx,
                    meta: tr_meta,
                },
                elapsed_ms,
            };
            return (mem_report, tr_report);
        }
    };

    let mem_report = match diagnose(&repo, &config.db_path, fp).await {
        Ok(r) => r,
        Err(e) => {
            let (idx_path, meta_path) = sidecar_paths(&config.db_path);
            DiagnosticReport {
                status: "db_unavailable",
                details: DiagnosticStatus::DbUnavailable {
                    reason: e.to_string(),
                },
                paths: PathInfo {
                    db: config.db_path.clone(),
                    index: idx_path,
                    meta: meta_path,
                },
                elapsed_ms: started.elapsed().as_millis() as u64,
            }
        }
    };

    let tr_started = std::time::Instant::now();
    let tr_report = match diagnose_transcripts(&repo, &config.db_path, fp).await {
        Ok(r) => r,
        Err(e) => {
            let (idx_path, meta_path) = transcript_sidecar_paths(&config.db_path);
            DiagnosticReport {
                status: "db_unavailable",
                details: DiagnosticStatus::DbUnavailable {
                    reason: e.to_string(),
                },
                paths: PathInfo {
                    db: config.db_path.clone(),
                    index: idx_path,
                    meta: meta_path,
                },
                elapsed_ms: tr_started.elapsed().as_millis() as u64,
            }
        }
    };

    (mem_report, tr_report)
}

/// Compute both per-pipeline rebuild outcomes without printing.
pub async fn compute_rebuild_outcomes(
    config: &Config,
    fp: &VectorIndexFingerprint,
) -> (RebuildOutcome, RebuildOutcome) {
    let (mem_idx, mem_meta) = sidecar_paths(&config.db_path);
    let mem_paths = PathInfo {
        db: config.db_path.clone(),
        index: mem_idx,
        meta: mem_meta,
    };
    let (tr_idx, tr_meta) = transcript_sidecar_paths(&config.db_path);
    let tr_paths = PathInfo {
        db: config.db_path.clone(),
        index: tr_idx,
        meta: tr_meta,
    };

    let repo = match DuckDbRepository::open(&config.db_path).await {
        Ok(r) => r,
        Err(e) => {
            let mem_out = RebuildOutcome::DbUnavailable {
                reason: e.to_string(),
                paths: mem_paths,
            };
            let tr_out = RebuildOutcome::DbUnavailable {
                reason: e.to_string(),
                paths: tr_paths,
            };
            return (mem_out, tr_out);
        }
    };

    let mem_started = std::time::Instant::now();
    let mem_outcome = match rebuild_index(&repo, &config.db_path, fp).await {
        Ok(idx) => RebuildOutcome::Rebuilt {
            rows: idx.size(),
            paths: mem_paths,
            elapsed_ms: mem_started.elapsed().as_millis() as u64,
        },
        Err(e) => RebuildOutcome::Failed {
            reason: e.to_string(),
            paths: mem_paths,
        },
    };

    // Per task spec: don't short-circuit on first failure unless DB itself was
    // unavailable. Run the transcripts rebuild even if the memories rebuild
    // failed.
    let tr_started = std::time::Instant::now();
    let tr_outcome = match rebuild_transcripts_index(&repo, &config.db_path, fp).await {
        Ok(idx) => RebuildOutcome::Rebuilt {
            rows: idx.size(),
            paths: tr_paths,
            elapsed_ms: tr_started.elapsed().as_millis() as u64,
        },
        Err(e) => RebuildOutcome::Failed {
            reason: e.to_string(),
            paths: tr_paths,
        },
    };

    (mem_outcome, tr_outcome)
}

/// Test-only helper: run `--check` end-to-end and return the formatted text
/// plus aggregated exit code, without printing to stdout.
pub async fn run_check_for_test(config: &Config, fp: &VectorIndexFingerprint) -> (String, i32) {
    let (mem_report, tr_report) = compute_check_reports(config, fp).await;
    let text = format_aggregate_check_text(&mem_report, &tr_report);
    let exit = aggregate_exit_code(
        mem_report.details.exit_code(),
        tr_report.details.exit_code(),
    );
    (text, exit)
}

/// Test-only helper: run `--rebuild` end-to-end and return the formatted text
/// plus aggregated exit code, without printing to stdout.
pub async fn run_rebuild_for_test(config: &Config, fp: &VectorIndexFingerprint) -> (String, i32) {
    let (mem_outcome, tr_outcome) = compute_rebuild_outcomes(config, fp).await;
    let text = format_aggregate_rebuild_text(&mem_outcome, &tr_outcome);
    let exit = aggregate_exit_code(mem_outcome.exit_code(), tr_outcome.exit_code());
    (text, exit)
}

fn emit_aggregate_check(
    memories: &DiagnosticReport,
    transcripts: &DiagnosticReport,
    as_json: bool,
) -> i32 {
    if as_json {
        let v = format_aggregate_check_json(memories, transcripts);
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        print!("{}", format_aggregate_check_text(memories, transcripts));
    }
    aggregate_exit_code(
        memories.details.exit_code(),
        transcripts.details.exit_code(),
    )
}

fn emit_aggregate_rebuild(
    memories: &RebuildOutcome,
    transcripts: &RebuildOutcome,
    as_json: bool,
) -> i32 {
    if as_json {
        let v = format_aggregate_rebuild_json(memories, transcripts);
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        print!("{}", format_aggregate_rebuild_text(memories, transcripts));
    }
    aggregate_exit_code(memories.exit_code(), transcripts.exit_code())
}

fn emit_config_error(args: &RepairArgs, reason: &str) -> i32 {
    if args.json {
        let command = if args.mode.rebuild_graph {
            "rebuild-graph"
        } else if args.mode.rebuild {
            "rebuild"
        } else {
            "check"
        };
        let v = json!({
            "command": command,
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
