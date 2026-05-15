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

use lancedb::table::{Duration, OptimizeAction, OptimizeStats};

use super::{lancedb_err, LanceStore};
use crate::storage::types::StorageError;

/// Aggregated outcome of one vacuum sweep across every Lance table.
///
/// `tables_pruned` is the count of *eagerly-created* tables that
/// the sweep actually touched. Lazy-created tables that haven't
/// been instantiated yet (e.g. `capability_capsule_embeddings`
/// before the first embedding upsert) are silently skipped — they
/// have no manifests to prune.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct VacuumStats {
    pub bytes_removed: u64,
    pub old_versions_removed: u64,
    pub tables_pruned: u64,
    pub tables_skipped: u64,
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
    // Lazy-created on first upsert; open_table fails if absent and
    // we skip without erroring.
    "capability_capsule_embeddings",
    "conversation_message_embeddings",
];

impl LanceStore {
    /// Prune Lance version manifests older than `older_than_days`
    /// across every managed table. Idempotent and read-safe with
    /// concurrent queries (Lance datasets are MVCC under the hood).
    ///
    /// `older_than_days = 0` is a valid operator override that
    /// reclaims everything except the current version — the config
    /// layer rejects it for the periodic worker, but the
    /// `POST /admin/vacuum` HTTP path can pass it through for
    /// immediate-relief sweeps on a developer machine. Always uses
    /// `delete_unverified=false` (the Lance default), so the 7-day
    /// safety margin against in-flight transactions still applies —
    /// only versions strictly older than the `older_than_days`
    /// cutoff AND committed at least 7 days ago will be removed
    /// when the override goes below 7.
    pub async fn vacuum_old_versions(
        &self,
        older_than_days: i64,
    ) -> Result<VacuumStats, StorageError> {
        let older_than = Duration::try_days(older_than_days).ok_or_else(|| {
            StorageError::InvalidInput(format!(
                "older_than_days {older_than_days} cannot be converted to Duration",
            ))
        })?;
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
            let stats: OptimizeStats = table
                .optimize(OptimizeAction::Prune {
                    older_than: Some(older_than),
                    delete_unverified: None,
                    error_if_tagged_old_versions: None,
                })
                .await
                .map_err(lancedb_err)?;
            if let Some(prune) = stats.prune {
                agg.bytes_removed += prune.bytes_removed;
                agg.old_versions_removed += prune.old_versions;
            }
            agg.tables_pruned += 1;
        }
        Ok(agg)
    }
}
