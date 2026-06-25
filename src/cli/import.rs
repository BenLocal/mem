//! `mem import` — bulk-archive an agent's conversation records into the
//! transcript archive (`conversation_messages`), **archive-only**.
//!
//! Contrast with `mem mine`, which is dual-sink (extract `<mem-save>`
//! memories AND archive blocks) over a **single** transcript file. `import`
//! is bulk + archive-only: it walks an agent's whole transcript store, parses
//! every `.jsonl`, and POSTs the per-block payloads to
//! `/transcripts/messages/batch`. No memory extraction, no `<mem-save>`
//! parsing, no mine cursor. It is the rebuild path for the verbatim archive.
//!
//! Idempotent: the batch endpoint dedups server-side by the
//! `(transcript_path, line_number, block_index)` triple, so re-running over an
//! already-imported store re-sends without double-inserting.
//!
//! **Extensible per source agent.** Each agent is a subcommand carrying its
//! own default discovery path and (today) the shared Claude-Code JSONL parser.
//! Adding the next agent (codex, cursor, …) means: add a variant to
//! [`ImportSource`], a default-root helper, and — if its on-disk format
//! differs — a parser that yields [`mine::ArchivedBlock`]s; the POST half is
//! already shared via [`mine::post_block_payloads`].

use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

use super::common::RemoteArgs;
use super::mine::{block_to_payload, parse_transcript_full, post_block_payloads};

/// One variant per supported source agent. Claude Code is implemented; the
/// enum is the extension point for the next agent's importer.
#[derive(Debug, Subcommand)]
pub enum ImportSource {
    /// Import Claude Code transcripts (`~/.claude/projects/**/*.jsonl`).
    ClaudeCode(ClaudeCodeArgs),
}

#[derive(Debug, Args)]
pub struct ClaudeCodeArgs {
    /// Directory to scan recursively for `*.jsonl` transcripts, or a single
    /// `.jsonl` file. Defaults to `~/.claude/projects` (Claude Code's
    /// per-project transcript store).
    #[arg(long)]
    pub path: Option<PathBuf>,

    #[command(flatten)]
    pub remote: RemoteArgs,

    /// `caller_agent` label stamped on every archived block.
    #[arg(long, default_value = "claude-code")]
    pub agent: String,

    /// Parse and report only; do not POST anything to the service.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub dry_run: bool,

    /// Print one progress line per transcript file.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub verbose: bool,
}

/// Dispatches to the selected source agent's importer. Returns the process
/// exit code (`0` = clean, `1` = any file/block failed or bad path).
pub async fn run(source: ImportSource) -> i32 {
    match source {
        ImportSource::ClaudeCode(a) => run_claude_code(a).await,
    }
}

/// Default Claude Code transcript root: `$HOME/.claude/projects`. Falls back
/// to `./.claude/projects` when `$HOME` is unset (degraded but deterministic).
fn default_claude_code_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".claude").join("projects")
}

/// Recursively collect `*.jsonl` files under `root`, sorted for deterministic
/// order. If `root` is itself a `.jsonl` file, returns just that file.
fn find_jsonl_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_jsonl(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_jsonl(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_file() {
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path.to_path_buf());
        }
        return Ok(());
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            collect_jsonl(&entry?.path(), out)?;
        }
    }
    Ok(())
}

async fn run_claude_code(args: ClaudeCodeArgs) -> i32 {
    let root = args.path.clone().unwrap_or_else(default_claude_code_root);
    if !root.exists() {
        eprintln!("import: path does not exist: {}", root.display());
        return 1;
    }

    let files = match find_jsonl_files(&root) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("import: failed to scan {}: {}", root.display(), e);
            return 1;
        }
    };

    if files.is_empty() {
        println!(
            "import: no .jsonl transcripts found under {}",
            root.display()
        );
        return 0;
    }

    let client = reqwest::Client::new();
    let mut files_ok: u32 = 0;
    let mut files_failed: u32 = 0;
    let mut total_blocks: u64 = 0;
    let mut total_archived: u64 = 0;
    let mut total_failed: u64 = 0;

    for file in &files {
        // Archive-only: keep the block half, drop the extracted-memory half.
        let blocks = match parse_transcript_full(file) {
            Ok((_memories, blocks)) => blocks,
            Err(e) => {
                eprintln!("import: skip {} (parse error: {})", file.display(), e);
                files_failed += 1;
                continue;
            }
        };

        if blocks.is_empty() {
            if args.verbose {
                println!("  {} — 0 blocks", file.display());
            }
            files_ok += 1;
            continue;
        }

        total_blocks += blocks.len() as u64;
        let transcript_path = file.display().to_string();
        let payloads: Vec<serde_json::Value> = blocks
            .iter()
            .map(|b| block_to_payload(b, &transcript_path, &args.remote.tenant, &args.agent))
            .collect();

        if args.dry_run {
            if args.verbose {
                println!(
                    "  {} — {} blocks (dry-run, not sent)",
                    file.display(),
                    payloads.len()
                );
            }
            total_archived += payloads.len() as u64;
            files_ok += 1;
            continue;
        }

        let (ok, fail) = post_block_payloads(&client, &args.remote.base_url, &payloads).await;
        total_archived += ok as u64;
        total_failed += fail as u64;
        if fail == 0 {
            files_ok += 1;
        } else {
            files_failed += 1;
        }
        if args.verbose {
            println!(
                "  {} — {} blocks ({} ok, {} failed)",
                file.display(),
                payloads.len(),
                ok,
                fail
            );
        }
    }

    println!(
        "Imported {} transcript file(s) [{} ok, {} failed]: {} blocks, {} archived, {} failed{}",
        files.len(),
        files_ok,
        files_failed,
        total_blocks,
        total_archived,
        total_failed,
        if args.dry_run { " (dry-run)" } else { "" }
    );

    if files_failed > 0 || total_failed > 0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_root_ends_with_claude_projects() {
        let root = default_claude_code_root();
        assert!(root.ends_with("projects"));
        assert!(root.to_string_lossy().contains(".claude"));
    }

    #[test]
    fn find_jsonl_walks_recursively_and_filters_extension() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Nested project dirs mirroring ~/.claude/projects/<slug>/<uuid>.jsonl.
        let proj_a = root.join("proj-a");
        let proj_b = root.join("proj-b").join("nested");
        fs::create_dir_all(&proj_a).unwrap();
        fs::create_dir_all(&proj_b).unwrap();

        fs::write(proj_a.join("session1.jsonl"), "{}").unwrap();
        fs::write(proj_a.join("session2.jsonl"), "{}").unwrap();
        fs::write(proj_b.join("session3.jsonl"), "{}").unwrap();
        // Non-jsonl siblings must be ignored.
        fs::write(proj_a.join("notes.txt"), "x").unwrap();
        fs::write(proj_a.join("config.json"), "{}").unwrap();

        let found = find_jsonl_files(root).unwrap();
        assert_eq!(found.len(), 3, "only the three .jsonl files: {found:?}");
        assert!(found.iter().all(|p| p.extension().unwrap() == "jsonl"));
        // Sorted output is deterministic.
        let mut sorted = found.clone();
        sorted.sort();
        assert_eq!(found, sorted);
    }

    #[test]
    fn find_jsonl_on_single_file_returns_just_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("one.jsonl");
        fs::write(&file, "{}").unwrap();

        let found = find_jsonl_files(&file).unwrap();
        assert_eq!(found, vec![file]);
    }

    #[test]
    fn find_jsonl_on_empty_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_jsonl_files(dir.path()).unwrap().is_empty());
    }
}
