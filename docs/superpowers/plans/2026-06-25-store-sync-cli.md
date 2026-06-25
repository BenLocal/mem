# `mem sync` Store-to-Store CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `mem sync --from <spec> --to <spec> --tenant <t>` CLI that verbatim-copies all migratable data domains from one storage backend to another (any → any across Lance / Postgres / ClickHouse), rebuilding embeddings on the target.

**Architecture:** Single in-process command opens two `Arc<dyn Backend>` handles directly (bypassing `app::from_config`, which spawns workers). Per tenant, it copies each domain in dependency order through existing trait reads/writes, skipping ids already present in the target (re-run = resume). No new trait methods.

**Tech Stack:** Rust 2021, `clap` (Subcommand), the existing `Backend` umbrella trait + its 11 sub-traits, `anyhow` for the CLI error path, `tempfile` + `FakeEmbeddingProvider` for tests.

**Spec:** `docs/superpowers/specs/2026-06-25-store-sync-cli-design.md`

---

## Design refinements found while planning (read before executing)

These tighten the spec's "Known gaps" with facts confirmed from the code:

1. **`resolve_or_create` remints `entity_id`** (`lance_store/entities.rs`: lookup-miss → `uuid::now_v7()`). The entities **table** cannot be copied with original ids through the trait. Graph edges are copied **verbatim** (they carry `entity:<old-uuid>` / `mem:<id>` strings), so the KG itself survives; but migrated entity rows get fresh ids that won't match the copied edges' entity refs. Entity-table copy is therefore **best-effort** (canonical names + kinds land; ids differ). Documented in Task 6.
2. **Capsule graph node id is `mem:<capability_capsule_id>`** (`pipeline/ingest.rs`). The edge walk queries `neighbors("mem:<id>")` for every copied capsule. Edges with no capsule endpoint (entity↔entity only) are not reached — acceptable: memory edges are capsule-rooted.
3. **Source Lance open takes the advisory `open_lock`** (`Store::open_with_provider`). Syncing **from** a Lance dir that a live `mem serve` is using will fail the lock (single-writer). Operational note: stop the source `mem serve` (or sync from a copy) during migration. Different source/target paths are fine.
4. **Only `outcome = success` episodes are listable** (`list_successful_episodes_for_tenant`). Non-success episodes don't migrate. Acceptable.
5. **Active edges only** — `add_edge_direct` writes active edges; closed (`valid_to` set) edges aren't reconstructed.

If exact entity-id fidelity (gap 1) matters, that needs a v2 `insert_entity_with_id` write method — out of scope here.

---

## Shared types (defined in Task 2, referenced everywhere)

```rust
// src/cli/sync.rs

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

/// Per (domain, tenant) tally. `copied` = rows written, `skipped` =
/// already present in target, `failed` = batches that errored.
#[derive(Debug, Default, Clone, Copy)]
pub struct DomainReport {
    pub copied: u64,
    pub skipped: u64,
    pub failed: u64,
}
```

Each copier has the uniform signature:

```rust
async fn copy_<domain>(
    src: &dyn mem::storage::Backend,
    dst: &dyn mem::storage::Backend,
    tenant: &str,
    batch_size: usize,
    dry_run: bool,
    verbose: bool,
) -> DomainReport
```

---

## File structure

- **Create** `src/cli/sync.rs` — the whole feature: `SyncArgs`, `parse_spec`, `open_backend`, the five `copy_*` copiers, the per-tenant orchestrator `run`, and inline unit tests.
- **Modify** `src/cli/mod.rs` — add `pub mod sync;`.
- **Modify** `src/main.rs` — add `Sync(mem::cli::sync::SyncArgs)` to `Command` + dispatch.
- **Create** `tests/store_sync.rs` — integration: `lance→lance` round-trip (always runs); `lance→clickhouse` / `lance→postgres` (self-skip without `MEM_TEST_*_URL`).
- **Modify** `README.md` — document `mem sync` (Task 9).

All copiers live in one file because they share `DomainReport`, the skip-set pattern, and the batch loop, and they change together. If `sync.rs` grows past ~600 lines, split the copiers into `src/cli/sync/` submodules in a follow-up — not now (YAGNI).

---

## Task 1: `parse_spec` — backend spec parser (pure, TDD)

**Files:**
- Create: `src/cli/sync.rs`
- Modify: `src/cli/mod.rs`

- [ ] **Step 1: Add the module declaration**

In `src/cli/mod.rs`, add (alphabetical order, after `pub mod serve;` / before `pub mod wake_up;`):

```rust
pub mod sync;
```

- [ ] **Step 2: Write the failing test**

Create `src/cli/sync.rs` with only:

```rust
//! `mem sync` — verbatim store-to-store copy (any → any across Lance /
//! Postgres / ClickHouse). See docs/superpowers/specs/2026-06-25-store-sync-cli-design.md.

use crate::config::BackendKind;

/// Parse a `--from` / `--to` spec of the form `<kind>:<locator>` into a
/// `(BackendKind, locator)` pair. `kind` is `lance` | `postgres` |
/// `clickhouse`; `locator` is the remainder after the FIRST `:` (so URLs
/// keeping their own `://` survive intact). Errors on unknown kind or
/// empty locator.
pub fn parse_spec(spec: &str) -> anyhow::Result<(BackendKind, String)> {
    unimplemented!()
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
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib cli::sync::tests 2>&1 | tail -20`
Expected: FAIL (panics with `not implemented`).

- [ ] **Step 4: Implement `parse_spec`**

Replace the `unimplemented!()` body:

```rust
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
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --lib cli::sync::tests 2>&1 | tail -20`
Expected: PASS (5 passed).

- [ ] **Step 6: Commit**

```bash
git add src/cli/sync.rs src/cli/mod.rs
git commit -m "feat(cli): mem sync — backend spec parser (parse_spec)"
```

---

## Task 2: CLI args + `open_backend` + dispatch skeleton

**Files:**
- Modify: `src/cli/sync.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add `SyncArgs`, `Domain`, `DomainReport`, `open_backend`, and a `run` stub**

In `src/cli/sync.rs`, add the imports + types above the `parse_spec` fn:

```rust
use std::sync::Arc;

use clap::Args;

use crate::config::{BackendKind, Config};
use crate::embedding::{arc_embedding_provider, EmbeddingProvider};
use crate::storage::{Backend, ClickHouseBackend, PostgresCapsuleStore, Store};
```

Add the shared types (`Domain`, `DEFAULT_DOMAINS`, `DomainReport`) exactly as in the "Shared types" section above, then:

```rust
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
    println!("sync: opened {} → {} (orchestration in Task 8)", args.from, args.to);
    Ok(0)
}
```

- [ ] **Step 2: Wire into `main.rs`**

In `src/main.rs`, add to `enum Command` (after the `Mine` / `Import` arms):

```rust
    /// Verbatim-copy all data domains from one storage backend to another
    /// (any → any across Lance / Postgres / ClickHouse). Rebuilds embeddings
    /// on the target. See README «mem sync».
    Sync(mem::cli::sync::SyncArgs),
```

And in `async_main`'s `match command`:

```rust
        Command::Sync(args) => {
            let code = mem::cli::sync::run(args).await;
            std::process::exit(code);
        }
```

- [ ] **Step 3: Verify it compiles + CLI shape**

Run: `cargo build 2>&1 | tail -5`
Expected: builds clean.
Run: `cargo run -q -- sync --help 2>&1 | head -20`
Expected: shows `--from --to --tenant --domains --batch-size --dry-run --verbose`.

- [ ] **Step 4: Commit**

```bash
git add src/cli/sync.rs src/main.rs
git commit -m "feat(cli): mem sync — args, open_backend, dispatch skeleton"
```

---

## Task 3: Capsule copier (+ enqueue embedding jobs) — TDD

**Files:**
- Modify: `src/cli/sync.rs`
- Test: `tests/store_sync.rs` (created here, extended later)

This is the template every other copier follows: read source ids → subtract ids already in target → fetch full rows → write in batches → tally.

- [ ] **Step 1: Write the failing integration test (lance→lance round-trip for capsules)**

Create `tests/store_sync.rs`:

```rust
//! `mem sync` integration tests. The lance→lance round-trip always runs;
//! lance→{clickhouse,postgres} self-skip without MEM_TEST_*_URL.

use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
};
use mem::embedding::FakeEmbeddingProvider;
use mem::storage::{CapsuleStore, CapsuleSearchStore, Store};

async fn temp_lance() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeEmbeddingProvider::new("fake", 64));
    let store = Store::open_with_provider(dir.path(), provider).await.unwrap();
    (dir, store)
}

fn sample_capsule(id: &str, tenant: &str) -> CapabilityCapsuleRecord {
    // Use the project's existing constructor/helpers if present; otherwise
    // build the struct literally. Fields per src/domain/capability_capsule.rs.
    CapabilityCapsuleRecord {
        capability_capsule_id: id.to_string(),
        tenant: tenant.to_string(),
        content_hash: format!("hash-{id}"),
        supersedes_capability_capsule_id: None,
        ..mem::test_support::capsule_fixture() // see note below
    }
}

#[tokio::test]
async fn syncs_capsules_lance_to_lance() {
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;

    src.insert_capability_capsules(&[
        sample_capsule("c1", "local"),
        sample_capsule("c2", "local"),
    ])
    .await
    .unwrap();

    let report = mem::cli::sync::copy_capsules_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 2);

    let ids = dst.list_capability_capsule_ids_for_tenant("local").await.unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"c1".to_string()));

    // Re-run is idempotent: second pass copies nothing.
    let again = mem::cli::sync::copy_capsules_for_test(&src, &dst, "local", 100).await;
    assert_eq!(again.copied, 0);
    assert_eq!(again.skipped, 2);
}
```

> **Note on `sample_capsule`:** if the codebase has no `test_support::capsule_fixture`, build the full `CapabilityCapsuleRecord` literal here by copying the field set from `src/domain/capability_capsule.rs` (the parity suites in `tests/capsule_store_parity.rs` already construct one — reuse that constructor pattern verbatim). Do **not** invent fields.

Add a thin test-only re-export at the bottom of `src/cli/sync.rs` so the integration test can call the copier without exposing internals broadly:

```rust
/// Test seam: integration tests call the capsule copier directly.
#[doc(hidden)]
pub async fn copy_capsules_for_test(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    batch_size: usize,
) -> DomainReport {
    copy_capsules(src, dst, tenant, batch_size, false, false).await
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test store_sync syncs_capsules_lance_to_lance 2>&1 | tail -20`
Expected: FAIL to compile (`copy_capsules` / `copy_capsules_for_test` not found).

- [ ] **Step 3: Implement `copy_capsules`**

In `src/cli/sync.rs` add (the imports it needs: `use crate::storage::{CapsuleStore, CapsuleSearchStore, EmbeddingJobStore}; use crate::storage::types::EmbeddingJobInsert; use crate::storage::current_timestamp;`):

```rust
async fn copy_capsules(
    src: &dyn Backend,
    dst: &dyn Backend,
    tenant: &str,
    batch_size: usize,
    dry_run: bool,
    verbose: bool,
) -> DomainReport {
    let mut report = DomainReport::default();

    // Collect EVERY version id (all statuses), not just active heads.
    let head_ids = match src.list_capability_capsule_ids_for_tenant(tenant).await {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("capsules[{tenant}]: list ids failed: {e}");
            report.failed += 1;
            return report;
        }
    };
    let mut all_ids: Vec<String> = Vec::new();
    for head in &head_ids {
        match src.list_capability_capsule_versions_for_tenant(tenant, head).await {
            Ok(links) => all_ids.extend(links.into_iter().map(|l| l.capability_capsule_id)),
            Err(_) => all_ids.push(head.clone()),
        }
    }
    all_ids.sort();
    all_ids.dedup();

    // Skip-set: ids already in the target (resume).
    let present: std::collections::HashSet<String> = dst
        .list_capability_capsule_ids_for_tenant(tenant)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    let todo: Vec<String> = all_ids.into_iter().filter(|id| !present.contains(id)).collect();
    report.skipped = (head_ids.len().saturating_sub(todo.len())) as u64; // approximate; refined below

    let now = current_timestamp();
    let provider_id = crate::config::Config::from_env()
        .map(|c| c.embedding.job_provider_id().to_string())
        .unwrap_or_else(|_| "fake".to_string());

    for chunk in todo.chunks(batch_size) {
        let id_refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
        let rows = match src.fetch_capability_capsules_by_ids(tenant, &id_refs).await {
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
        // Rebuild embeddings on the target: enqueue one job per capsule.
        let jobs: Vec<EmbeddingJobInsert> = rows
            .iter()
            .map(|r| EmbeddingJobInsert {
                job_id: uuid::Uuid::now_v7().to_string(),
                tenant: tenant.to_string(),
                capability_capsule_id: r.capability_capsule_id.clone(),
                target_content_hash: r.content_hash.clone(),
                provider: provider_id.clone(),
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
            println!("  capsules[{tenant}]: +{} (total {})", rows.len(), report.copied);
        }
    }
    report
}
```

> **Skipped count note:** `report.skipped` is set to the count of source ids filtered out by the skip-set. Compute it precisely as `present-intersection` size:
> replace the approximate line with — after building `todo` —
> `report.skipped = present.iter().filter(|id| ...).count()` is wrong direction; instead track `let total = <len of all_ids before filter>; report.skipped = (total - todo.len()) as u64;` Capture `total` right after `all_ids.dedup();`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test store_sync syncs_capsules_lance_to_lance 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/sync.rs tests/store_sync.rs
git commit -m "feat(cli): mem sync — capsule copier (verbatim + enqueue target embeds)"
```

---

## Task 4: Transcript copier — TDD

**Files:** Modify `src/cli/sync.rs`, `tests/store_sync.rs`

- [ ] **Step 1: Write the failing test** — append to `tests/store_sync.rs`:

```rust
#[tokio::test]
async fn syncs_transcripts_lance_to_lance() {
    use mem::domain::{ConversationMessage, MessageRole, BlockType};
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;
    // Build one message via the parity-suite constructor pattern
    // (see tests/clickhouse_backend.rs for a ConversationMessage literal).
    let msg = mem::test_support::conversation_message_fixture("sess1", "local");
    use mem::storage::TranscriptStore;
    src.create_conversation_messages(&[msg]).await.unwrap();

    let report = mem::cli::sync::copy_transcripts_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 1);
    let got = dst.get_conversation_messages_by_session("local", "sess1").await.unwrap();
    assert_eq!(got.len(), 1);
}
```

> Reuse an existing `ConversationMessage` constructor from `tests/clickhouse_backend.rs` if no `test_support` helper exists — build the literal, don't invent fields.

- [ ] **Step 2: Run → fail** (`copy_transcripts_for_test` not found).

- [ ] **Step 3: Implement `copy_transcripts` + test seam** (imports: `use crate::storage::TranscriptStore;`):

```rust
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
        let msgs = match src.get_conversation_messages_by_session(tenant, &s.session_id).await {
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
        // create_conversation_messages dedups server-side by
        // (transcript_path, line_number, block_index) and auto-enqueues
        // transcript embedding jobs for embed_eligible blocks → resume-safe.
        match dst.create_conversation_messages(&msgs).await {
            Ok(n) => {
                report.copied += n as u64;
                report.skipped += (msgs.len() - n) as u64;
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

#[doc(hidden)]
pub async fn copy_transcripts_for_test(
    src: &dyn Backend, dst: &dyn Backend, tenant: &str, batch_size: usize,
) -> DomainReport { copy_transcripts(src, dst, tenant, batch_size, false, false).await }
```

> **Verify at impl time:** that `create_conversation_messages` returns the count of *newly inserted* rows (so `skipped` is correct). If it returns the input length regardless, set `report.copied += n` and drop the `skipped` line.

- [ ] **Step 4: Run → pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(cli): mem sync — transcript copier"`

---

## Task 5: Episode copier — TDD

**Files:** Modify `src/cli/sync.rs`, `tests/store_sync.rs`

- [ ] **Step 1: Failing test** — append:

```rust
#[tokio::test]
async fn syncs_episodes_lance_to_lance() {
    use mem::storage::SessionStore;
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;
    let ep = mem::test_support::episode_fixture("local"); // outcome="success"
    src.insert_episode(ep).await.unwrap();
    let report = mem::cli::sync::copy_episodes_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 1);
    assert_eq!(dst.list_successful_episodes_for_tenant("local").await.unwrap().len(), 1);
}
```

> Build the `EpisodeRecord` literal from `src/domain/episode.rs` (fields: `episode_id, tenant, goal, steps, outcome="success", evidence, scope, visibility, …`) if no fixture helper exists.

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement** (imports: `use crate::storage::SessionStore;`):

```rust
async fn copy_episodes(
    src: &dyn Backend, dst: &dyn Backend, tenant: &str,
    _batch_size: usize, dry_run: bool, verbose: bool,
) -> DomainReport {
    let mut report = DomainReport::default();
    let eps = match src.list_successful_episodes_for_tenant(tenant).await {
        Ok(e) => e,
        Err(e) => { eprintln!("episodes[{tenant}]: list failed: {e}"); report.failed += 1; return report; }
    };
    for ep in eps {
        if dry_run { report.copied += 1; continue; }
        match dst.insert_episode(ep).await {
            Ok(_) => { report.copied += 1; }
            Err(e) => { eprintln!("episodes[{tenant}]: insert failed: {e}"); report.failed += 1; }
        }
    }
    if verbose { println!("  episodes[{tenant}]: +{}", report.copied); }
    report
}

#[doc(hidden)]
pub async fn copy_episodes_for_test(
    src: &dyn Backend, dst: &dyn Backend, tenant: &str, batch_size: usize,
) -> DomainReport { copy_episodes(src, dst, tenant, batch_size, false, false).await }
```

> Episodes have no per-id skip read; `insert_episode` re-inserting the same `episode_id` on re-run may duplicate. **Verify at impl time** whether `insert_episode` upserts on `episode_id`; if it appends, add a pre-read of `list_successful_episodes_for_tenant(dst)` into a skip-set keyed on `episode_id` (same pattern as capsules).

- [ ] **Step 4: Run → pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(cli): mem sync — episode copier"`

---

## Task 6: Entity copier (best-effort, id-not-preserved) — TDD

**Files:** Modify `src/cli/sync.rs`, `tests/store_sync.rs`

> **Caveat baked into the doc-comment:** `resolve_or_create` remints `entity_id`, so this lands canonical names + kinds but NOT original ids; copied graph edges keep their verbatim `entity:<old-uuid>` refs and won't link to these rows. See plan refinement #1.

- [ ] **Step 1: Failing test** — append:

```rust
#[tokio::test]
async fn syncs_entities_lance_to_lance() {
    use mem::storage::EntityRegistry;
    use mem::domain::EntityKind;
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;
    src.resolve_or_create("local", "InvoiceService", EntityKind::Component, "20260625T000000000")
        .await.unwrap();
    let report = mem::cli::sync::copy_entities_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 1);
    let got = dst.list_entities("local", None, None, 100).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].canonical_name, "InvoiceService");
}
```

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement** (imports: `use crate::storage::EntityRegistry;`):

```rust
/// Best-effort: copies canonical names + kinds via `resolve_or_create`.
/// NOTE: `entity_id` is reminted on the target (no insert-with-id read), so
/// these rows won't match the verbatim `entity:<uuid>` refs in copied edges.
async fn copy_entities(
    src: &dyn Backend, dst: &dyn Backend, tenant: &str,
    _batch_size: usize, dry_run: bool, verbose: bool,
) -> DomainReport {
    let mut report = DomainReport::default();
    let entities = match src.list_entities(tenant, None, None, 1_000_000).await {
        Ok(e) => e,
        Err(e) => { eprintln!("entities[{tenant}]: list failed: {e}"); report.failed += 1; return report; }
    };
    let now = crate::storage::current_timestamp();
    for ent in entities {
        if dry_run { report.copied += 1; continue; }
        match dst.resolve_or_create(tenant, &ent.canonical_name, ent.kind, &now).await {
            Ok(_) => { report.copied += 1; }
            Err(e) => { eprintln!("entities[{tenant}]: resolve_or_create failed: {e}"); report.failed += 1; }
        }
    }
    if verbose { println!("  entities[{tenant}]: +{} (ids reminted)", report.copied); }
    report
}

#[doc(hidden)]
pub async fn copy_entities_for_test(
    src: &dyn Backend, dst: &dyn Backend, tenant: &str, batch_size: usize,
) -> DomainReport { copy_entities(src, dst, tenant, batch_size, false, false).await }
```

- [ ] **Step 4: Run → pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(cli): mem sync — entity copier (best-effort, id reminted)"`

---

## Task 7: Graph-edge copier — TDD

**Files:** Modify `src/cli/sync.rs`, `tests/store_sync.rs`

- [ ] **Step 1: Failing test** — append:

```rust
#[tokio::test]
async fn syncs_active_edges_lance_to_lance() {
    use mem::storage::GraphStore;
    use mem::domain::capability_capsule::GraphEdge;
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;
    // A capsule must exist so the walk enumerates its node id.
    use mem::storage::CapsuleStore;
    src.insert_capability_capsules(&[sample_capsule("c1", "local")]).await.unwrap();
    let edge = GraphEdge {
        from_node_id: "mem:c1".to_string(),
        to_node_id: "entity:abc".to_string(),
        relation: "mentions".to_string(),
        valid_from: "20260625T000000000".to_string(),
        valid_to: None,
        confidence: None,
        ..mem::test_support::graph_edge_defaults() // or build the literal
    };
    src.sync_memory_edges(&[edge], "20260625T000000000").await.unwrap();

    let report = mem::cli::sync::copy_edges_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 1);
    assert_eq!(dst.neighbors("mem:c1").await.unwrap().len(), 1);
}
```

> Build the `GraphEdge` literal fully from `src/domain/capability_capsule.rs` if no defaults helper exists.

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement** (imports: `use crate::storage::GraphStore;`):

```rust
/// Walks `neighbors("mem:<id>")` for every capsule in the tenant, collects
/// active edges, dedupes by (from, to, relation), and writes via
/// `add_edge_direct` (preserves `valid_from`). Active edges only; edges with
/// no capsule endpoint are not reached (memory edges are capsule-rooted).
async fn copy_graph_edges(
    src: &dyn Backend, dst: &dyn Backend, tenant: &str,
    _batch_size: usize, dry_run: bool, verbose: bool,
) -> DomainReport {
    let mut report = DomainReport::default();
    let capsule_ids = src
        .list_capability_capsule_ids_for_tenant(tenant)
        .await
        .unwrap_or_default();

    let mut seen: std::collections::HashSet<(String, String, String)> = std::collections::HashSet::new();
    let mut edges: Vec<mem::domain::capability_capsule::GraphEdge> = Vec::new();
    for id in &capsule_ids {
        let node = format!("mem:{id}");
        match src.neighbors(&node).await {
            Ok(es) => {
                for e in es {
                    let k = (e.from_node_id.clone(), e.to_node_id.clone(), e.relation.clone());
                    if seen.insert(k) {
                        edges.push(e);
                    }
                }
            }
            Err(e) => { eprintln!("edges[{tenant}/{node}]: neighbors failed: {e}"); report.failed += 1; }
        }
    }

    if dry_run { report.copied = edges.len() as u64; return report; }

    for e in edges {
        match dst.add_edge_direct(&e).await {
            Ok(true) => report.copied += 1,
            Ok(false) => report.skipped += 1, // active duplicate already present
            Err(err) => { eprintln!("edges[{tenant}]: add failed: {err}"); report.failed += 1; }
        }
    }
    if verbose { println!("  edges[{tenant}]: +{} (skip {})", report.copied, report.skipped); }
    report
}

#[doc(hidden)]
pub async fn copy_edges_for_test(
    src: &dyn Backend, dst: &dyn Backend, tenant: &str, batch_size: usize,
) -> DomainReport { copy_graph_edges(src, dst, tenant, batch_size, false, false).await }
```

> **Note:** `mem::domain::capability_capsule::GraphEdge` is the type returned by `neighbors`; confirm the import path. The fake test edge uses `from_node_id: "mem:c1"` so the walk over capsule `c1` finds it.

- [ ] **Step 4: Run → pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(cli): mem sync — graph edge copier (active edges)"`

---

## Task 8: Orchestration — wire copiers into `run_inner` + full round-trip test

**Files:** Modify `src/cli/sync.rs`, `tests/store_sync.rs`

- [ ] **Step 1: Replace the Task-2 placeholder body in `run_inner`**

Swap the `let _ = (&src, &dst, &domains, &config); println!(...)` lines for:

```rust
    let mut grand = DomainReport::default();
    for tenant in &args.tenants {
        for domain in &domains {
            let r = match domain {
                Domain::Entities => copy_entities(src.as_ref(), dst.as_ref(), tenant, args.batch_size, args.dry_run, args.verbose).await,
                Domain::Capsules => copy_capsules(src.as_ref(), dst.as_ref(), tenant, args.batch_size, args.dry_run, args.verbose).await,
                Domain::Episodes => copy_episodes(src.as_ref(), dst.as_ref(), tenant, args.batch_size, args.dry_run, args.verbose).await,
                Domain::Transcripts => copy_transcripts(src.as_ref(), dst.as_ref(), tenant, args.batch_size, args.dry_run, args.verbose).await,
                Domain::Graph => copy_graph_edges(src.as_ref(), dst.as_ref(), tenant, args.batch_size, args.dry_run, args.verbose).await,
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
        grand.copied, grand.skipped, grand.failed
    );
    Ok(if grand.failed > 0 { 1 } else { 0 })
```

- [ ] **Step 2: Write the full multi-domain round-trip test** — append to `tests/store_sync.rs`:

```rust
#[tokio::test]
async fn full_roundtrip_lance_to_lance() {
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;
    // Seed one of each domain (reuse the fixtures above).
    use mem::storage::{CapsuleStore, EntityRegistry};
    use mem::domain::EntityKind;
    src.insert_capability_capsules(&[sample_capsule("c1", "local")]).await.unwrap();
    src.resolve_or_create("local", "Svc", EntityKind::Component, "20260625T000000000").await.unwrap();

    // Drive the public copiers in order; assert each landed.
    let caps = mem::cli::sync::copy_capsules_for_test(&src, &dst, "local", 100).await;
    let ents = mem::cli::sync::copy_entities_for_test(&src, &dst, "local", 100).await;
    assert_eq!(caps.copied, 1);
    assert_eq!(ents.copied, 1);
    assert_eq!(caps.failed + ents.failed, 0);
}
```

- [ ] **Step 3: Add lance→clickhouse / lance→postgres self-skipping tests** — append:

```rust
#[tokio::test]
async fn syncs_capsules_lance_to_clickhouse() {
    let Ok(url) = std::env::var("MEM_TEST_CLICKHOUSE_URL") else {
        eprintln!("MEM_TEST_CLICKHOUSE_URL unset — skipping lance→clickhouse");
        return;
    };
    let (_sd, src) = temp_lance().await;
    use mem::storage::CapsuleStore;
    src.insert_capability_capsules(&[sample_capsule("c1", "local")]).await.unwrap();
    let ch = mem::storage::ClickHouseBackend::connect(&url).await.unwrap();
    ch.apply_migrations().await.unwrap();
    let report = mem::cli::sync::copy_capsules_for_test(&src, &ch, "local", 100).await;
    assert_eq!(report.failed, 0);
    assert!(report.copied >= 1);
}
```

(Mirror with `MEM_TEST_POSTGRES_URL` + `PostgresCapsuleStore::connect`.)

- [ ] **Step 4: Run the whole suite**

Run: `cargo test --test store_sync 2>&1 | tail -25`
Expected: lance→lance tests PASS; clickhouse/postgres tests print skip + PASS (no URL).

- [ ] **Step 5: Commit** — `git commit -m "feat(cli): mem sync — orchestration + round-trip tests"`

---

## Task 9: README docs + full gate + final commit

**Files:** Modify `README.md`, `src/cli/sync.rs` (only if gate flags lints)

- [ ] **Step 1: Document `mem sync`** in `README.md` under «Storage backends» (after the Postgres run example), covering: the `<kind>:<locator>` spec, `--tenant` (required/repeatable), `--domains`, `--dry-run`, the embeddings-rebuilt-on-target behavior, and the five known gaps (tenant enum, entity-id remint, active-edges-only, async embed tail, operational tables). Add a runnable example:

````markdown
```bash
# Migrate a local Lance store into ClickHouse (one tenant), dry-run first.
mem sync --from lance:/root/.mem/mem.lance \
         --to clickhouse:http://mem:mem@localhost:8123 \
         --tenant local --dry-run --verbose
```
````

- [ ] **Step 2: Run the full gate**

Run: `cargo fmt`
Run: `cargo fmt --check` → expect clean.
Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -10` → expect clean (fix any lints inline).
Run: `cargo test 2>&1 | tail -15` → expect all green, store_sync included, ch/pg self-skipped.

- [ ] **Step 3: Commit**

```bash
git add README.md src/cli/sync.rs
git commit -m "docs(readme): document mem sync; chore: gate green"
```

---

## Self-review

**Spec coverage:** every spec section maps to a task — CLI surface (T2), open_backend/architecture (T2), capsules+embeddings (T3), transcripts (T4), episodes (T5), entities (T6), graph edges (T7), resume/idempotency (skip-sets in T3/T4/T7; episode skip flagged in T5), dry-run + reports + exit code (T8), testing matrix (T3–T8), README (T9). The five known gaps are documented (plan refinements + T6/T9). ✅

**Placeholder scan:** the only deferred items are explicit "verify at impl time" notes tied to real, named methods (`create_conversation_messages` return semantics, `insert_episode` upsert vs append, fixture constructors) — each says exactly what to check and the fallback. No "TODO/TBD/add error handling" placeholders. Fixture helpers (`mem::test_support::*`) are flagged with "reuse the parity-suite constructor / build the literal" because the codebase may not expose a `test_support` module — the executor must confirm and use the existing `tests/*_parity.rs` / `tests/clickhouse_backend.rs` constructors rather than invent fields.

**Type consistency:** `DomainReport { copied, skipped, failed }`, `Domain` variants, and the `copy_*(src, dst, tenant, batch_size, dry_run, verbose) -> DomainReport` signature are uniform across T3–T8. Each copier ships a `copy_*_for_test` seam used by the matching test. `EmbeddingJobInsert` fields match `src/storage/types.rs`; capsule fields (`capability_capsule_id`, `content_hash`) match `src/domain/capability_capsule.rs`.

**Executor must confirm before/while coding (low-risk):**
- `mem::test_support` may not exist → use existing parity-suite fixtures or build struct literals.
- `create_conversation_messages` return value (newly-inserted count vs input len) → adjust `skipped`.
- `insert_episode` upsert vs append → add a skip-set if append.
- `GraphEdge` import path for the test (`mem::domain::capability_capsule::GraphEdge`).
