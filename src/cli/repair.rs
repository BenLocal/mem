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
