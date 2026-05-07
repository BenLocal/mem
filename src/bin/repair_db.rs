//! One-shot DuckDB rebuild via EXPORT/IMPORT to clean up ART index corruption.
//!
//! Usage:
//!   1. Stop `mem serve` (DuckDB single-writer lock).
//!   2. `cargo run --bin repair_db -- /root/.mem/mem.duckdb`
//!   3. Restart `mem serve`.
//!
//! Strategy:
//!   * Open the (possibly corrupted) DB.
//!   * `EXPORT DATABASE '<tmp>' (FORMAT PARQUET);` — sequential row scan,
//!     no index lookups, dodges ART corruption.
//!   * Move the old file aside (kept as `<file>.corrupt-<ts>`).
//!   * Open a fresh DB at the original path and `IMPORT DATABASE '<tmp>';`
//!     — rebuilds every index (PK, FK, secondary) from row data.
//!
//! The export path is unique-per-run via timestamp; the corrupt original is
//! renamed (not deleted) so you have a recovery anchor.
//!
//! This binary is intentionally tiny and dependency-free beyond the mem
//! crate's existing `duckdb` dep, so it ships with the same bundled DuckDB
//! version mem itself uses — no cross-version surprises.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use duckdb::Connection;

fn main() -> Result<()> {
    let db = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/root/.mem/mem.duckdb".to_string());
    let db_path = PathBuf::from(&db);
    if !db_path.exists() {
        anyhow::bail!("DB not found: {}", db_path.display());
    }

    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let export_dir = std::env::temp_dir().join(format!("mem-export-{ts}"));
    let backup = db_path.with_extension(format!("duckdb.corrupt-{ts}"));
    let wal = db_path.with_extension("duckdb.wal");
    let wal_backup = wal.with_extension(format!("wal.corrupt-{ts}"));

    println!("== mem repair_db ==");
    println!("  source : {}", db_path.display());
    println!("  export : {}", export_dir.display());
    println!("  backup : {}", backup.display());

    // 1. EXPORT DATABASE — sequential scan, dodges ART
    {
        println!("\n[1/4] EXPORT DATABASE …");
        let conn =
            Connection::open(&db_path).with_context(|| format!("open {}", db_path.display()))?;
        std::fs::create_dir_all(&export_dir)?;
        conn.execute_batch(&format!(
            "EXPORT DATABASE '{}' (FORMAT PARQUET);",
            export_dir.display()
        ))
        .context("EXPORT failed — corruption deeper than ART, try plan C (rowid surgery)")?;
        // conn dropped here → file lock released
    }
    let exported_files: Vec<_> = std::fs::read_dir(&export_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    println!("       wrote {} files", exported_files.len());

    // 2. Move old DB aside
    println!("\n[2/4] moving corrupt DB → {}", backup.display());
    std::fs::rename(&db_path, &backup)
        .with_context(|| format!("rename {} → {}", db_path.display(), backup.display()))?;
    if wal.exists() {
        std::fs::rename(&wal, &wal_backup)
            .with_context(|| format!("rename {} → {}", wal.display(), wal_backup.display()))?;
        println!("       moved WAL too → {}", wal_backup.display());
    }

    // 3. IMPORT into fresh DB — rebuilds every index from scratch
    {
        println!("\n[3/4] IMPORT DATABASE into fresh file …");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open fresh {}", db_path.display()))?;
        conn.execute_batch(&format!("IMPORT DATABASE '{}';", export_dir.display()))
            .context("IMPORT failed — fresh DB is unusable; restore from backup")?;
    }

    // 4. Sanity probe — try a SELECT that touches the PK ART
    println!("\n[4/4] sanity probe …");
    {
        let conn = Connection::open(&db_path)?;
        let total: i64 = conn.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))?;
        println!("       memories rows: {total}");
        let problem_id = "mem_019de690-431d-7133-8b5a-2becc0e2ea43";
        let by_id: Option<String> = conn
            .query_row(
                "SELECT memory_id FROM memories WHERE memory_id = ?1",
                [problem_id],
                |r| r.get(0),
            )
            .ok();
        match by_id {
            Some(id) => println!("       PK lookup of {id} OK (ART rebuilt)"),
            None => {
                println!("       PK lookup found nothing for {problem_id} (gone or never existed)")
            }
        }
    }

    println!("\n✓ done.\n  export dir kept: {}\n  corrupt original kept: {}\n  next: restart `mem serve`", export_dir.display(), backup.display());
    Ok(())
}
