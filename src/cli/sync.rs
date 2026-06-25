//! `mem sync` — verbatim store-to-store copy (any → any across Lance /
//! Postgres / ClickHouse). See docs/superpowers/specs/2026-06-25-store-sync-cli-design.md.

use std::sync::Arc;

use clap::Args;

use crate::config::{BackendKind, Config};
use crate::embedding::{arc_embedding_provider, EmbeddingProvider};
use crate::storage::{Backend, ClickHouseBackend, PostgresCapsuleStore, Store};

/// One data domain copied independently, in this dependency order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Domain {
    Entities,
    Capsules,
    Episodes,
    Transcripts,
    Graph,
}

/// Default domain set + order when `--domains` is omitted.
const DEFAULT_DOMAINS: &[Domain] = &[
    Domain::Entities,
    Domain::Capsules,
    Domain::Episodes,
    Domain::Transcripts,
    Domain::Graph,
];

/// Per (domain, tenant) tally. `copied` = rows written, `skipped` = already
/// present in target, `failed` = batches that errored.
#[derive(Debug, Default, Clone, Copy)]
pub struct DomainReport {
    pub copied: u64,
    pub skipped: u64,
    pub failed: u64,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Source backend spec: `<kind>:<locator>` (e.g. `lance:/root/.mem/mem.lance`).
    #[arg(long)]
    pub from: String,

    /// Target backend spec: `<kind>:<locator>` (e.g. `clickhouse:http://mem:mem@localhost:8123`).
    #[arg(long)]
    pub to: String,

    /// Tenant(s) to copy. Required, repeatable — there is no tenant-enumeration read.
    #[arg(long = "tenant", required = true)]
    pub tenants: Vec<String>,

    /// Domains to copy (default: all five, in dependency order).
    #[arg(long, value_delimiter = ',')]
    pub domains: Vec<Domain>,

    /// Rows per write batch.
    #[arg(long, default_value_t = 200)]
    pub batch_size: usize,

    /// Read + count only; write nothing.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub dry_run: bool,

    /// Per-batch / per-session progress lines.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub verbose: bool,
}

/// Open a backend handle from a parsed spec. Both source and target Lance
/// opens use the same real embedding provider so dims stay consistent; the
/// source never embeds (reads only) and the target enqueues jobs its own
/// `mem serve` worker drains. ClickHouse runs idempotent migrations on open.
async fn open_backend(
    kind: BackendKind,
    locator: &str,
    provider: Arc<dyn EmbeddingProvider>,
) -> anyhow::Result<Arc<dyn Backend>> {
    match kind {
        BackendKind::Lance => {
            let store = Store::open_with_provider(locator, provider)
                .await
                .map_err(|e| anyhow::anyhow!("open lance `{locator}`: {e}"))?;
            Ok(Arc::new(store))
        }
        BackendKind::Postgres => {
            let pg = PostgresCapsuleStore::connect(locator)
                .await
                .map_err(|e| anyhow::anyhow!("connect postgres: {e}"))?;
            Ok(Arc::new(pg))
        }
        BackendKind::Clickhouse => {
            let ch = ClickHouseBackend::connect(locator)
                .await
                .map_err(|e| anyhow::anyhow!("connect clickhouse: {e}"))?;
            ch.apply_migrations()
                .await
                .map_err(|e| anyhow::anyhow!("clickhouse migrate: {e}"))?;
            Ok(Arc::new(ch))
        }
    }
}

/// Entry point. Returns process exit code (`0` clean, `1` if any batch failed
/// or setup errored).
pub async fn run(args: SyncArgs) -> i32 {
    match run_inner(args).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sync: {e:#}");
            1
        }
    }
}

async fn run_inner(args: SyncArgs) -> anyhow::Result<i32> {
    let config = Config::from_env().map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let provider = arc_embedding_provider(&config.embedding)
        .map_err(|e| anyhow::anyhow!("embedding provider: {e}"))?;

    let (from_kind, from_loc) = parse_spec(&args.from)?;
    let (to_kind, to_loc) = parse_spec(&args.to)?;
    let src = open_backend(from_kind, &from_loc, provider.clone()).await?;
    let dst = open_backend(to_kind, &to_loc, provider.clone()).await?;

    let domains: Vec<Domain> = if args.domains.is_empty() {
        DEFAULT_DOMAINS.to_vec()
    } else {
        args.domains.clone()
    };

    // Copiers land in Task 3-7; orchestration in Task 8.
    let _ = (&src, &dst, &domains, &config);
    println!(
        "sync: opened {} → {} (orchestration in Task 8)",
        args.from, args.to
    );
    Ok(0)
}

/// Parse a `--from` / `--to` spec of the form `<kind>:<locator>` into a
/// `(BackendKind, locator)` pair. `kind` is `lance` | `postgres` |
/// `clickhouse`; `locator` is the remainder after the FIRST `:` (so URLs
/// keeping their own `://` survive intact). Errors on unknown kind or
/// empty locator.
pub fn parse_spec(spec: &str) -> anyhow::Result<(BackendKind, String)> {
    let (kind_str, locator) = spec
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("spec must be `<kind>:<locator>`, got `{spec}`"))?;
    let kind = match kind_str {
        "lance" => BackendKind::Lance,
        "postgres" => BackendKind::Postgres,
        "clickhouse" => BackendKind::Clickhouse,
        other => anyhow::bail!("unknown backend kind `{other}` (use lance|postgres|clickhouse)"),
    };
    if locator.is_empty() {
        anyhow::bail!("spec `{spec}` has an empty locator");
    }
    Ok((kind, locator.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lance_dir() {
        let (k, loc) = parse_spec("lance:/root/.mem/mem.lance").unwrap();
        assert_eq!(k, BackendKind::Lance);
        assert_eq!(loc, "/root/.mem/mem.lance");
    }

    #[test]
    fn parses_postgres_url_keeping_scheme() {
        let (k, loc) = parse_spec("postgres:postgres://u:p@h:5432/db").unwrap();
        assert_eq!(k, BackendKind::Postgres);
        assert_eq!(loc, "postgres://u:p@h:5432/db");
    }

    #[test]
    fn parses_clickhouse_url() {
        let (k, loc) = parse_spec("clickhouse:http://mem:mem@localhost:8123").unwrap();
        assert_eq!(k, BackendKind::Clickhouse);
        assert_eq!(loc, "http://mem:mem@localhost:8123");
    }

    #[test]
    fn rejects_unknown_kind() {
        assert!(parse_spec("mysql:whatever").is_err());
    }

    #[test]
    fn rejects_missing_locator() {
        assert!(parse_spec("lance:").is_err());
        assert!(parse_spec("lance").is_err());
    }
}
