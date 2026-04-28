use clap::Args;
use crate::storage::{DiagnosticReport, DiagnosticStatus, SidecarFile};

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
    #[allow(dead_code)]
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
