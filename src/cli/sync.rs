//! `mem sync` — verbatim store-to-store copy (any → any across Lance /
//! Postgres / ClickHouse). See docs/superpowers/specs/2026-06-25-store-sync-cli-design.md.

use std::sync::Arc;

use clap::Args;

use crate::config::{BackendKind, Config};
use crate::embedding::{arc_embedding_provider, EmbeddingProvider};
use crate::storage::types::EmbeddingJobInsert;
use crate::storage::{
    current_timestamp, Backend, CapsuleStore, ClickHouseBackend, PostgresCapsuleStore, Store,
};

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

/// Copy all capsules (every status, all version links) from `src` to `dst`
/// for one `tenant`. Already-present ids are skipped (idempotent). Enqueues
/// embedding jobs on the target so the destination `mem serve`'s worker can
/// rebuild vectors. Returns a per-tenant tally.
async fn copy_capsules(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    batch_size: usize,
    dry_run: bool,
    verbose: bool,
    provider_id: &str,
) -> DomainReport {
    let mut report = DomainReport::default();

    let head_ids = match src.list_capability_capsule_ids_for_tenant(tenant).await {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("capsules[{tenant}]: list ids failed: {e}");
            report.failed += 1;
            return report;
        }
    };

    // Collect EVERY version id (all statuses), not just active heads.
    let mut all_ids: Vec<String> = Vec::new();
    for head in &head_ids {
        match src
            .list_capability_capsule_versions_for_tenant(tenant, head)
            .await
        {
            Ok(links) => all_ids.extend(links.into_iter().map(|l| l.capability_capsule_id)),
            Err(e) => {
                eprintln!(
                    "capsules[{tenant}]: version-walk failed for {head}: {e} (copying head only)"
                );
                all_ids.push(head.clone());
            }
        }
    }
    all_ids.sort();
    all_ids.dedup();
    let total = all_ids.len();

    let present: std::collections::HashSet<String> = dst
        .list_capability_capsule_ids_for_tenant(tenant)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    let to_copy: Vec<String> = all_ids
        .into_iter()
        .filter(|id| !present.contains(id))
        .collect();
    report.skipped = (total - to_copy.len()) as u64;

    let now = current_timestamp();

    for chunk in to_copy.chunks(batch_size) {
        let id_refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
        let rows = match CapsuleStore::fetch_capability_capsules_by_ids(src, tenant, &id_refs).await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("capsules[{tenant}]: fetch failed: {e}");
                report.failed += chunk.len() as u64;
                continue;
            }
        };
        if dry_run {
            report.copied += rows.len() as u64;
            continue;
        }
        if let Err(e) = dst.insert_capability_capsules(&rows).await {
            eprintln!("capsules[{tenant}]: insert failed: {e}");
            report.failed += rows.len() as u64;
            continue;
        }
        let jobs: Vec<EmbeddingJobInsert> = rows
            .iter()
            .map(|r| EmbeddingJobInsert {
                job_id: uuid::Uuid::now_v7().to_string(),
                tenant: tenant.to_string(),
                capability_capsule_id: r.capability_capsule_id.clone(),
                target_content_hash: r.content_hash.clone(),
                provider: provider_id.to_string(),
                available_at: now.clone(),
                created_at: now.clone(),
                updated_at: now.clone(),
            })
            .collect();
        if let Err(e) = dst.enqueue_embedding_jobs(&jobs).await {
            eprintln!("capsules[{tenant}]: enqueue embed jobs failed: {e}");
            // Rows landed; vectors just won't rebuild for this batch. Not a row failure.
        }
        report.copied += rows.len() as u64;
        if verbose {
            println!(
                "  capsules[{tenant}]: +{} (total {})",
                rows.len(),
                report.copied
            );
        }
    }
    report
}

/// Test seam: integration tests call the capsule copier directly.
#[doc(hidden)]
pub async fn copy_capsules_for_test(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    batch_size: usize,
) -> DomainReport {
    copy_capsules(src, dst, tenant, batch_size, false, false, "fake").await
}

/// Copy all transcript messages from `src` to `dst` for one `tenant`.
/// Iterates sessions, reads messages per session, and bulk-inserts into dst.
/// The dst backend deduplicates on `(transcript_path, line_number, block_index)`,
/// so re-runs are safe. Returns a per-tenant tally.
async fn copy_transcripts(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    _batch_size: usize,
    dry_run: bool,
    verbose: bool,
) -> DomainReport {
    let mut report = DomainReport::default();
    let sessions = match src.list_transcript_sessions(tenant).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("transcripts[{tenant}]: list sessions failed: {e}");
            report.failed += 1;
            return report;
        }
    };
    for s in sessions {
        let msgs = match src
            .get_conversation_messages_by_session(tenant, &s.session_id)
            .await
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!("transcripts[{tenant}/{}]: read failed: {e}", s.session_id);
                report.failed += 1;
                continue;
            }
        };
        if dry_run {
            report.copied += msgs.len() as u64;
            continue;
        }
        match dst.create_conversation_messages(&msgs).await {
            Ok(n) => {
                // `create_conversation_messages` returns the count of newly-inserted
                // rows (input length minus dedup-skipped). Skipped = msgs.len() - n.
                report.skipped += (msgs.len() - n) as u64;
                report.copied += n as u64;
                if verbose {
                    println!("  transcripts[{tenant}/{}]: +{n}", s.session_id);
                }
            }
            Err(e) => {
                eprintln!("transcripts[{tenant}/{}]: write failed: {e}", s.session_id);
                report.failed += msgs.len() as u64;
            }
        }
    }
    report
}

/// Test seam: integration tests call the transcript copier directly.
#[doc(hidden)]
pub async fn copy_transcripts_for_test(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    batch_size: usize,
) -> DomainReport {
    copy_transcripts(src, dst, tenant, batch_size, false, false).await
}

/// Copy all successful episodes from `src` to `dst` for one `tenant`.
/// Already-present episode ids (by id) are skipped so re-runs are idempotent.
/// Returns a per-tenant tally.
async fn copy_episodes(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    _batch_size: usize,
    dry_run: bool,
    verbose: bool,
) -> DomainReport {
    let mut report = DomainReport::default();
    let eps = match src.list_successful_episodes_for_tenant(tenant).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("episodes[{tenant}]: list failed: {e}");
            report.failed += 1;
            return report;
        }
    };
    let present: std::collections::HashSet<String> = dst
        .list_successful_episodes_for_tenant(tenant)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.episode_id)
        .collect();
    for ep in eps {
        if present.contains(&ep.episode_id) {
            report.skipped += 1;
            continue;
        }
        if dry_run {
            report.copied += 1;
            continue;
        }
        match dst.insert_episode(ep).await {
            Ok(_) => report.copied += 1,
            Err(e) => {
                eprintln!("episodes[{tenant}]: insert failed: {e}");
                report.failed += 1;
            }
        }
    }
    if verbose {
        println!(
            "  episodes[{tenant}]: +{} (skip {})",
            report.copied, report.skipped
        );
    }
    report
}

/// Test seam: integration tests call the episode copier directly.
#[doc(hidden)]
pub async fn copy_episodes_for_test(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    batch_size: usize,
) -> DomainReport {
    copy_episodes(src, dst, tenant, batch_size, false, false).await
}

/// Copy entities from `src` to `dst` for one `tenant`, best-effort.
///
/// Uses `resolve_or_create`, which REMINTS `entity_id` on the target (no
/// insert-with-id path exists), so canonical names + kinds migrate but the
/// original ids do NOT. Copied graph edges keep their verbatim `entity:<uuid>`
/// refs and therefore won't link to these reminted rows — this is the accepted
/// v1 limitation. Idempotent: entities already present on the target (matched
/// by canonical_name) are skipped.
async fn copy_entities(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    _batch_size: usize,
    dry_run: bool,
    verbose: bool,
) -> DomainReport {
    let mut report = DomainReport::default();
    let entities = match src.list_entities(tenant, None, None, 1_000_000).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("entities[{tenant}]: list failed: {e}");
            report.failed += 1;
            return report;
        }
    };
    let present: std::collections::HashSet<String> = dst
        .list_entities(tenant, None, None, 1_000_000)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.canonical_name)
        .collect();
    let now = current_timestamp();
    for ent in entities {
        if present.contains(&ent.canonical_name) {
            report.skipped += 1;
            continue;
        }
        if dry_run {
            report.copied += 1;
            continue;
        }
        match dst
            .resolve_or_create(tenant, &ent.canonical_name, ent.kind, &now)
            .await
        {
            Ok(_) => report.copied += 1,
            Err(e) => {
                eprintln!("entities[{tenant}]: resolve_or_create failed: {e}");
                report.failed += 1;
            }
        }
    }
    if verbose {
        println!(
            "  entities[{tenant}]: +{} (skip {}, ids reminted)",
            report.copied, report.skipped
        );
    }
    report
}

/// Test seam: integration tests call the entity copier directly.
#[doc(hidden)]
pub async fn copy_entities_for_test(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    batch_size: usize,
) -> DomainReport {
    copy_entities(src, dst, tenant, batch_size, false, false).await
}

/// Copy active graph edges from `src` to `dst` for one `tenant`. Walks
/// `neighbors("mem:<id>")` for every capsule in the tenant, dedupes by
/// `(from, to, relation)`, and writes via `add_edge_direct` (preserves
/// `valid_from`). ACTIVE edges only — closed (valid_to set) edges are not
/// reconstructed. Edges with no capsule endpoint are not reached (memory
/// edges are capsule-rooted). Idempotent: an active duplicate on the target
/// returns `false` from `add_edge_direct` and is counted as skipped.
async fn copy_graph_edges(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    _batch_size: usize,
    dry_run: bool,
    verbose: bool,
) -> DomainReport {
    let mut report = DomainReport::default();
    let capsule_ids = match src.list_capability_capsule_ids_for_tenant(tenant).await {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("edges[{tenant}]: list capsule ids failed: {e}");
            report.failed += 1;
            return report;
        }
    };

    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    let mut edges: Vec<crate::domain::capability_capsule::GraphEdge> = Vec::new();
    for id in &capsule_ids {
        let node = format!("mem:{id}");
        match src.neighbors(&node).await {
            Ok(es) => {
                for e in es {
                    let k = (
                        e.from_node_id.clone(),
                        e.to_node_id.clone(),
                        e.relation.clone(),
                    );
                    if seen.insert(k) {
                        edges.push(e);
                    }
                }
            }
            Err(e) => {
                eprintln!("edges[{tenant}/{node}]: neighbors failed: {e}");
                report.failed += 1;
            }
        }
    }

    if dry_run {
        report.copied = edges.len() as u64;
        return report;
    }

    for e in edges {
        match dst.add_edge_direct(&e).await {
            Ok(true) => report.copied += 1,
            Ok(false) => report.skipped += 1,
            Err(err) => {
                eprintln!("edges[{tenant}]: add failed: {err}");
                report.failed += 1;
            }
        }
    }
    if verbose {
        println!(
            "  edges[{tenant}]: +{} (skip {})",
            report.copied, report.skipped
        );
    }
    report
}

/// Test seam: integration tests call the graph edge copier directly.
#[doc(hidden)]
pub async fn copy_edges_for_test(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    batch_size: usize,
) -> DomainReport {
    copy_graph_edges(src, dst, tenant, batch_size, false, false).await
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

    let provider_id = config.embedding.job_provider_id();
    let mut grand = DomainReport::default();
    for tenant in &args.tenants {
        for domain in &domains {
            let r = match domain {
                Domain::Entities => {
                    copy_entities(
                        src.as_ref(),
                        dst.as_ref(),
                        tenant,
                        args.batch_size,
                        args.dry_run,
                        args.verbose,
                    )
                    .await
                }
                Domain::Capsules => {
                    copy_capsules(
                        src.as_ref(),
                        dst.as_ref(),
                        tenant,
                        args.batch_size,
                        args.dry_run,
                        args.verbose,
                        provider_id,
                    )
                    .await
                }
                Domain::Episodes => {
                    copy_episodes(
                        src.as_ref(),
                        dst.as_ref(),
                        tenant,
                        args.batch_size,
                        args.dry_run,
                        args.verbose,
                    )
                    .await
                }
                Domain::Transcripts => {
                    copy_transcripts(
                        src.as_ref(),
                        dst.as_ref(),
                        tenant,
                        args.batch_size,
                        args.dry_run,
                        args.verbose,
                    )
                    .await
                }
                Domain::Graph => {
                    copy_graph_edges(
                        src.as_ref(),
                        dst.as_ref(),
                        tenant,
                        args.batch_size,
                        args.dry_run,
                        args.verbose,
                    )
                    .await
                }
            };
            println!(
                "{:?}[{tenant}]: copied={} skipped={} failed={}",
                domain, r.copied, r.skipped, r.failed
            );
            grand.copied += r.copied;
            grand.skipped += r.skipped;
            grand.failed += r.failed;
        }
    }
    println!(
        "sync done{}: copied={} skipped={} failed={}",
        if args.dry_run { " (dry-run)" } else { "" },
        grand.copied,
        grand.skipped,
        grand.failed
    );
    Ok(if grand.failed > 0 { 1 } else { 0 })
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
