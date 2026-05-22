//! Cross-table maintenance operations on the Lance dataset
//! collection — pruning old version manifests, future fragment
//! compaction, etc. Not tied to any one table's CRUD module.
//!
//! Lance is copy-on-write: every UPDATE writes a new manifest and
//! the old one stays on disk forever unless explicitly cleaned.
//! High-churn tables (`transcript_embedding_jobs`,
//! `conversation_message_embeddings`) accumulate gigabytes of
//! historical manifests within days. `vacuum_old_versions` is the
//! mechanical fix — driven on a daily cadence by
//! `crate::worker::vacuum_worker` and exposed on-demand via
//! `POST /admin/vacuum`.

use lancedb::table::{CompactionOptions, Duration, OptimizeAction, OptimizeStats};

use super::{lancedb_err, LanceStore};
use crate::storage::types::StorageError;

/// Aggregated outcome of one vacuum sweep across every Lance table.
///
/// `tables_pruned` is the count of *eagerly-created* tables that
/// the sweep actually touched. Lazy-created tables that haven't
/// been instantiated yet (e.g. `capability_capsule_embeddings`
/// before the first embedding upsert) are silently skipped — they
/// have no manifests to prune.
///
/// `fragments_removed` / `fragments_added` are the totals across all
/// tables of small data fragments merged during the compaction phase
/// (added 2026-05-21 — see method doc). High-churn tables like
/// `transcript_embedding_jobs` accumulate thousands of single-row
/// fragments from per-job state updates; compaction merges them into
/// far fewer larger fragments, cutting per-query scan cost by orders
/// of magnitude.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct VacuumStats {
    pub bytes_removed: u64,
    pub old_versions_removed: u64,
    pub tables_pruned: u64,
    pub tables_skipped: u64,
    pub fragments_removed: u64,
    pub fragments_added: u64,
}

/// All Lance tables managed by mem. Order matches `LanceStore::open_inner`'s
/// `ensure_*_table` block + the two lazy embedding tables.
const ALL_TABLES: &[&str] = &[
    "capability_capsules",
    "feedback_events",
    "embedding_jobs",
    "graph_edges",
    "entities",
    "entity_aliases",
    "conversation_messages",
    "transcript_embedding_jobs",
    "sessions",
    "episodes",
    "mine_cursors",
    // Lazy-created on first upsert; open_table fails if absent and
    // we skip without erroring.
    "capability_capsule_embeddings",
    "conversation_message_embeddings",
];

impl LanceStore {
    /// Compact small data fragments + prune old version manifests
    /// across every managed table. Idempotent and read-safe with
    /// concurrent queries on the same `LanceStore` instance (Lance
    /// datasets are MVCC under the hood).
    ///
    /// Two-pass: **compact first** (merges thousands of single-row
    /// fragments accumulated from per-job state updates on hot tables
    /// like `transcript_embedding_jobs` into a handful of larger ones),
    /// **then prune** (removes the old version manifests left behind
    /// by the compaction commits + any other prior churn). Order
    /// matters — pruning before compaction would leave the recently
    /// merged fragments' superseded predecessors in place for one
    /// more sweep cycle. Compaction itself produces new version
    /// manifests that prune can then immediately reclaim.
    ///
    /// Compaction was added 2026-05-21 after a runaway `mem` instance
    /// hit 500% CPU because `transcript_embedding_jobs.lance/data/`
    /// had accumulated **10,269 fragment files** — each
    /// `transcript_embedding_worker` tick query had to scan all of
    /// them. `OptimizeAction::Prune` (the pre-2026-05-21 only
    /// behavior) does **not** merge data fragments; only `Compact`
    /// does. Per-row writes against any high-churn table need this.
    ///
    /// `aggressive=true` bypasses Lance's hard 7-day in-flight
    /// safety floor on the **prune** phase (single-writer local-first
    /// default — see [`crate::config::VacuumSettings::aggressive`]).
    /// `aggressive=false` keeps the floor for multi-writer /
    /// shared-dataset deployments. Compaction itself ignores
    /// `aggressive` — it's always safe under MVCC.
    pub async fn vacuum_old_versions_with(
        &self,
        older_than_days: i64,
        aggressive: bool,
    ) -> Result<VacuumStats, StorageError> {
        let older_than = Duration::try_days(older_than_days).ok_or_else(|| {
            StorageError::InvalidInput(format!(
                "older_than_days {older_than_days} cannot be converted to Duration",
            ))
        })?;
        // `Some(true)` ↔ bypass the 7-day floor (single-writer
        // local-first deploy); `None` ↔ Lance default (`false` =
        // keep the floor, safe for shared/multi-writer setups).
        let delete_unverified = if aggressive { Some(true) } else { None };
        let mut agg = VacuumStats::default();
        for name in ALL_TABLES {
            let table = match self.conn.open_table(*name).execute().await {
                Ok(t) => t,
                Err(lancedb::Error::TableNotFound { .. }) => {
                    // Lazy table — embedding tables before first
                    // upsert. Expected, count it but don't fail.
                    agg.tables_skipped += 1;
                    continue;
                }
                Err(e) => return Err(lancedb_err(e)),
            };
            // Pass 1: compact small fragments. `CompactionOptions::default()`
            // targets ~1M rows per fragment, ~10% deletion materialization
            // threshold, no max-source-fragments cap (everything eligible
            // gets compacted in one pass). No-op when fragments are already
            // at target size.
            let compact_stats: OptimizeStats = table
                .optimize(OptimizeAction::Compact {
                    options: CompactionOptions::default(),
                    remap_options: None,
                })
                .await
                .map_err(lancedb_err)?;
            if let Some(c) = compact_stats.compaction {
                agg.fragments_removed += c.fragments_removed as u64;
                agg.fragments_added += c.fragments_added as u64;
            }
            // Pass 2: prune old version manifests (including the ones
            // compaction just superseded).
            let prune_stats: OptimizeStats = table
                .optimize(OptimizeAction::Prune {
                    older_than: Some(older_than),
                    delete_unverified,
                    error_if_tagged_old_versions: None,
                })
                .await
                .map_err(lancedb_err)?;
            if let Some(prune) = prune_stats.prune {
                agg.bytes_removed += prune.bytes_removed;
                agg.old_versions_removed += prune.old_versions;
            }
            agg.tables_pruned += 1;
        }
        Ok(agg)
    }
}
