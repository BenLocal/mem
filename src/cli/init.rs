//! `mem init` — first-run scaffolding for a new mem deployment.
//!
//! Closes mempalace-diff-v3 #31. Mempalace's `onboarding.py` analogue,
//! adapted to mem's shape — mem uses env vars (not YAML config), so we
//! emit:
//!
//! - `<path>/.mem/config.env` — `MEM_*` env-var defaults for the
//!   deployment. Operators source it from their shell (`set -a; source
//!   .mem/config.env; set +a`) or wire it into systemd / docker.
//! - `<path>/.mem/taxonomy.toml` — mode-specific starter list of
//!   suggested `project` / `repo` strings. Purely informational —
//!   mem doesn't load it at runtime, but the operator (or a follow-up
//!   tool) can use these as `project=...` hints when ingesting capsules
//!   so new tenants don't start with a blank-slate scope vocabulary.
//! - `<path>/.mem/README.md` — next-step guide (start `mem serve`,
//!   verify with `curl /health`, etc).
//!
//! The CLI refuses to overwrite an existing `<path>/.mem/` unless
//! `--force` is set — first-run scaffolding should never silently
//! clobber a hand-tuned config.

use std::fs;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

/// Operator personas the scaffold knows about. Each maps to a
/// distinct `taxonomy.toml` starter — the file the operator most
/// often wants to edit first.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum InitMode {
    /// Software project — wings = repos, rooms = modules.
    Code,
    /// Personal knowledge base — wings = life domains
    /// (family / hobbies / finance), no source-control metaphor.
    Personal,
    /// Research / literature corpus — wings = paper clusters,
    /// rooms = experiment runs / drafts.
    Research,
}

impl InitMode {
    pub fn as_str(self) -> &'static str {
        match self {
            InitMode::Code => "code",
            InitMode::Personal => "personal",
            InitMode::Research => "research",
        }
    }

    /// Suggested `project` strings for the taxonomy starter file.
    /// Operators edit these to match their actual scopes; the
    /// concrete defaults are just better than a blank file.
    fn suggested_projects(self) -> &'static [&'static str] {
        match self {
            InitMode::Code => &["mem", "your-main-project", "your-side-project"],
            InitMode::Personal => &["life", "family", "hobbies", "finance", "health"],
            InitMode::Research => &["literature", "experiments", "drafts", "references"],
        }
    }

    /// Suggested `repo` (mode=code) / room-equivalent strings —
    /// second-level grouping under a project. mempalace's "rooms"
    /// concept maps here.
    fn suggested_repos(self) -> &'static [&'static str] {
        match self {
            InitMode::Code => &["src", "tests", "docs", "scripts"],
            InitMode::Personal => &["daily-notes", "decisions", "references"],
            InitMode::Research => &["lit-review", "raw-data", "notes", "code"],
        }
    }
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Operator persona. Drives which taxonomy.toml starter ships.
    #[arg(long, value_enum, default_value_t = InitMode::Code)]
    pub mode: InitMode,

    /// Directory to scaffold under. The `.mem/` subdir is created
    /// inside this path. Default: current working directory.
    #[arg(long)]
    pub path: Option<PathBuf>,

    /// Overwrite an existing `.mem/` directory if present. Off by
    /// default — first-run scaffolding should refuse to clobber a
    /// hand-tuned config.
    #[arg(long)]
    pub force: bool,
}

/// Run the scaffold. Returns exit code (0 = ok, 2 = refused to
/// overwrite, 1 = I/O / encoding failure). Mirrors the other CLI
/// handlers' "return i32, main() exits with it" pattern.
pub fn run(args: InitArgs) -> i32 {
    let base = args
        .path
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mem_dir = base.join(".mem");

    if mem_dir.exists() && !args.force {
        eprintln!(
            "mem init: refused to scaffold — {} already exists. Pass --force to overwrite, or pick a different --path.",
            mem_dir.display(),
        );
        return 2;
    }

    if let Err(e) = scaffold(&mem_dir, args.mode) {
        eprintln!("mem init: scaffold failed at {}: {e}", mem_dir.display());
        return 1;
    }

    println!(
        "mem init: scaffolded {} (mode={})",
        mem_dir.display(),
        args.mode.as_str(),
    );
    println!("Next steps:");
    println!(
        "  1. Review {}/config.env and adjust MEM_DB_PATH if needed",
        mem_dir.display()
    );
    println!(
        "  2. Source it:  set -a; source {}/config.env; set +a",
        mem_dir.display()
    );
    println!("  3. Start the service:  mem serve");
    println!("  4. Verify:  curl http://127.0.0.1:3000/health");
    0
}

/// Pure scaffold — writes the three files under `mem_dir`. Extracted
/// so unit tests can drive it directly without a CLI dance.
pub fn scaffold(mem_dir: &Path, mode: InitMode) -> std::io::Result<()> {
    fs::create_dir_all(mem_dir)?;
    fs::write(mem_dir.join("config.env"), config_env(mode))?;
    fs::write(mem_dir.join("taxonomy.toml"), taxonomy_toml(mode))?;
    fs::write(mem_dir.join("README.md"), readme_md(mode))?;
    Ok(())
}

fn config_env(mode: InitMode) -> String {
    // The set of env vars listed here matches the ones documented in
    // CLAUDE.md's "Key env vars" section. We pick conservative
    // defaults: fake embedding provider so the first run doesn't
    // require an API key, vacuum + auto_promote on (their new
    // session defaults), dedup off (destructive — opt in once the
    // operator has a real corpus to dedup).
    format!(
        r#"# mem config — generated by `mem init --mode {mode}`
# Source from your shell:  set -a; source .mem/config.env; set +a
#
# Storage
MEM_DB_PATH="$PWD/.mem/mem.duckdb"
MEM_TENANT="local"

# HTTP service
BIND_ADDR="127.0.0.1:3000"

# Embedding provider — `fake` works offline (deterministic hash-based
# vectors). Swap to `embedanything` for local CPU inference once you
# have model weights cached, or `openai` after setting OPENAI_API_KEY.
EMBEDDING_PROVIDER="fake"

# Background workers — defaults match the in-process worker shapes
# (vacuum + auto_promote ON, dedup OFF). Uncomment to override.
# MEM_VACUUM_DISABLED=1
# MEM_AUTO_PROMOTE_DISABLED=1
# MEM_DEDUP_ENABLED=1            # opt in to near-duplicate sweep (destructive)
# MEM_DEDUP_THRESHOLD=0.92
"#,
        mode = mode.as_str(),
    )
}

fn taxonomy_toml(mode: InitMode) -> String {
    let projects = mode
        .suggested_projects()
        .iter()
        .map(|p| format!("  {{ name = \"{p}\" }},\n"))
        .collect::<String>();
    let repos = mode
        .suggested_repos()
        .iter()
        .map(|r| format!("  {{ name = \"{r}\" }},\n"))
        .collect::<String>();
    format!(
        r#"# mem taxonomy starter — generated by `mem init --mode {mode}`
#
# This file is INFORMATIONAL: mem doesn't load it at runtime. It exists
# to give a new deployment a non-blank set of `project` / `repo` strings
# to use as the `project=...` / `repo=...` fields on `capability_capsule_ingest`
# calls. Edit it to match your actual scopes — when you find yourself
# typing the same string into ingest calls repeatedly, add it here so
# the next operator (or future you) sees it as a known value.

mode = "{mode}"

# Top-level groupings (≈ mempalace "wings"). `project` field on capsules.
projects = [
{projects}]

# Second-level groupings (≈ mempalace "rooms"). `repo` field on capsules.
repos = [
{repos}]
"#,
        mode = mode.as_str(),
    )
}

fn readme_md(mode: InitMode) -> String {
    format!(
        r#"# `.mem/` — scaffolded by `mem init --mode {mode}`

This directory holds the deployment-specific configuration for one
`mem` instance. Three files:

- **`config.env`** — `MEM_*` env-var defaults. Source it from your
  shell or wire it into your process supervisor:

  ```bash
  set -a; source .mem/config.env; set +a
  mem serve
  ```

- **`taxonomy.toml`** — informational starter list of suggested
  `project` / `repo` strings for ingest calls. Edit it as your scope
  vocabulary stabilizes.

- **`README.md`** — this file. Safe to delete.

## Next steps

1. Open `config.env` and adjust `MEM_DB_PATH` if you want the DB
   somewhere other than `<this-dir>/mem.duckdb`.
2. If you'll use OpenAI embeddings, set `EMBEDDING_PROVIDER=openai`
   and export `OPENAI_API_KEY`. Otherwise the default `fake` provider
   is fine for trying things out — it produces deterministic vectors
   from content hashes (no network, no API key).
3. Start the service: `mem serve`.
4. Verify health: `curl http://127.0.0.1:3000/health`.
5. (Optional) Wire `mem` into Claude Code via the plugin manifest at
   `.claude-plugin/.mcp.json`.

## Mode: `{mode}`

The `--mode` flag at `mem init` time picked starter taxonomies suited
for **{mode_human}** corpora. If your actual use case drifts, just edit
`taxonomy.toml` — there's no "wrong" mode, the file is descriptive,
not prescriptive.
"#,
        mode = mode.as_str(),
        mode_human = mode_human(mode),
    )
}

fn mode_human(mode: InitMode) -> &'static str {
    match mode {
        InitMode::Code => "software-project",
        InitMode::Personal => "personal-knowledge-base",
        InitMode::Research => "research-literature",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn each_mode_produces_distinct_taxonomy() {
        let code = taxonomy_toml(InitMode::Code);
        let personal = taxonomy_toml(InitMode::Personal);
        let research = taxonomy_toml(InitMode::Research);
        // All three differ — no accidental sharing of starter projects.
        assert_ne!(code, personal);
        assert_ne!(code, research);
        assert_ne!(personal, research);
        // Each mode's signature project appears in its taxonomy.
        assert!(code.contains("your-main-project"));
        assert!(personal.contains("family"));
        assert!(research.contains("literature"));
    }

    #[test]
    fn scaffold_writes_three_files() {
        let dir = tempdir().unwrap();
        let mem_dir = dir.path().join(".mem");
        scaffold(&mem_dir, InitMode::Code).expect("scaffold");
        assert!(mem_dir.join("config.env").is_file());
        assert!(mem_dir.join("taxonomy.toml").is_file());
        assert!(mem_dir.join("README.md").is_file());
    }

    #[test]
    fn config_env_contains_required_keys() {
        let env = config_env(InitMode::Code);
        for key in [
            "MEM_DB_PATH",
            "MEM_TENANT",
            "BIND_ADDR",
            "EMBEDDING_PROVIDER",
        ] {
            assert!(
                env.contains(key),
                "config.env must mention {key}, got:\n{env}",
            );
        }
        // Destructive worker is commented-out by default.
        let dedup_line = env
            .lines()
            .find(|l| l.contains("MEM_DEDUP_ENABLED"))
            .expect("dedup line present");
        assert!(
            dedup_line.trim_start().starts_with('#'),
            "MEM_DEDUP_ENABLED line should be commented out by default: {dedup_line}",
        );
    }

    #[test]
    fn run_refuses_to_overwrite_without_force() {
        let dir = tempdir().unwrap();
        let mem_dir = dir.path().join(".mem");
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(mem_dir.join("touched"), "user data").unwrap();

        let code = run(InitArgs {
            mode: InitMode::Code,
            path: Some(dir.path().to_path_buf()),
            force: false,
        });
        assert_eq!(code, 2, "should refuse overwrite without --force");
        // Pre-existing file is untouched.
        let body = std::fs::read_to_string(mem_dir.join("touched")).unwrap();
        assert_eq!(body, "user data");
    }

    #[test]
    fn run_overwrites_when_force_is_set() {
        let dir = tempdir().unwrap();
        let mem_dir = dir.path().join(".mem");
        std::fs::create_dir_all(&mem_dir).unwrap();
        let code = run(InitArgs {
            mode: InitMode::Personal,
            path: Some(dir.path().to_path_buf()),
            force: true,
        });
        assert_eq!(code, 0);
        // taxonomy.toml is the mode-discriminating file (config.env
        // only mentions mode in a header comment). `mode = "personal"`
        // is the TOML field set by `taxonomy_toml(InitMode::Personal)`.
        let taxonomy = std::fs::read_to_string(mem_dir.join("taxonomy.toml")).unwrap();
        assert!(
            taxonomy.contains("mode = \"personal\""),
            "taxonomy.toml should encode mode=personal: {taxonomy}",
        );
    }

    #[test]
    fn readme_references_chosen_mode_name() {
        for mode in [InitMode::Code, InitMode::Personal, InitMode::Research] {
            let body = readme_md(mode);
            assert!(
                body.contains(mode.as_str()),
                "README should mention mode name {}: {body}",
                mode.as_str(),
            );
        }
    }
}
