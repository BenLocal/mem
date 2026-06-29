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

use lancedb::index::scalar::BTreeIndexBuilder;
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::index::Index;
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

/// Index type to build for a managed column.
#[derive(Debug, Clone, Copy)]
enum IndexKind {
    /// IVF_PQ ANN index on a vector column (semantic search). Without it
    /// a `nearest_to` query brute-force flat-scans the column.
    Vector,
    /// BTree scalar index on a high-cardinality column. Without it,
    /// equality / JOIN predicates on the column flat-scan the table.
    Scalar,
}

/// Every index `ensure_query_indexes` keeps current, as `(table, column,
/// kind)`. Lance does NOT auto-build these (the usearch sidecar that used
/// to maintain the vector index was removed in QW-4 and never replaced).
/// - The two `embedding` ANN indexes: an unindexed `nearest_to` scan over
///   `conversation_message_embeddings` (118MB, ~28k×1024-dim)
///   made transcript search 5–11s vs 0.6s for the tiny capsule table.
/// - `conversation_messages.message_block_id` scalar index: the transcript
///   semantic query JOINs the ANN hits back to `conversation_messages` on
///   `message_block_id`, and `fetch_conversation_messages_by_ids` filters
///   by it — both flat-scan the 106MB table without an index.
const MANAGED_INDEXES: &[(&str, &str, IndexKind)] = &[
    (
        "conversation_message_embeddings",
        "embedding",
        IndexKind::Vector,
    ),
    (
        "capability_capsule_embeddings",
        "embedding",
        IndexKind::Vector,
    ),
    (
        "conversation_messages",
        "message_block_id",
        IndexKind::Scalar,
    ),
];

/// Below this row count a brute-force flat scan is already sub-second, so
/// skip indexing — building an IVF/PQ index on a tiny table is pointless
/// and PQ training wants a few thousand rows anyway.
const MIN_ROWS_TO_INDEX: usize = 5_000;

/// A Lance ANN index does NOT cover rows appended after it was built —
/// those fall back to brute-force. Once the unindexed delta passes this,
/// rebuild so the whole table is covered again.
const REINDEX_DELTA_THRESHOLD: usize = 4_096;

/// Target rows-per-partition for IVF vector index sizing.
///
/// lancedb 0.30 derives `num_partitions = num_rows / target_partition_size`.
/// Its default over-partitioned our embedding tables — 256 partitions for the
/// ~49k-row `conversation_message_embeddings`, i.e. ~190 rows each. Pinning
/// ~1024 rows/partition (49k → 48 partitions, ~1025 rows each) keeps partitions
/// large and KMeans better-conditioned, and scales with the table as the
/// archive grows.
///
/// ROOT-CAUSE NOTE — reconciled with AGENTS.md "Lance STALE-INDEX ragged-batch"
/// (authoritative): the `IO Error: ... all columns in a record batch must have
/// the same length` on `/transcripts/search` was *originally* pinned on this
/// over-partitioning, and a ~1024-rows/partition sweep appeared to clear it.
/// That theory was **later superseded** — the authoritative root cause is a
/// **stale / partially-covering index** (a scan merging the indexed segment
/// with the unindexed append-tail yields unequal-length columns), a lance-core
/// bug that partition sizing only *reduces*, never eliminates. So this constant
/// is a **mitigation, not the fix**: keep it (large partitions still help), but
/// the live defenses are (a) the soft-degrade and (b) the self-healing
/// force-reindex-and-retry in `TranscriptService::search`; the true fix is
/// upstream (lance 7.0 / lancedb 0.30). Capsule recall is unaffected — that
/// table is flat-scanned, no IVF index.
const IVF_TARGET_ROWS_PER_PARTITION: usize = 1_024;

/// IVF partition count for a vector index over `row_count` rows. Scales with
/// the table (robust as it grows) and is clamped to Lance's own `[1, 4096]`
/// partition bound. At [`MIN_ROWS_TO_INDEX`] (5k) this is 4 partitions; at the
/// current ~49k embedding rows, 48 (~1025 rows each) — large enough that the
/// extension's ANN materialization no longer produces the ragged-batch 500.
fn ivf_num_partitions(row_count: usize) -> u32 {
    (row_count / IVF_TARGET_ROWS_PER_PARTITION).clamp(1, 4_096) as u32
}

/// What [`LanceStore::ensure_query_indexes`] should do for one table,
/// factored out as a pure decision so it can be unit-tested without a
/// live Lance dataset.
#[derive(Debug, PartialEq, Eq)]
enum IndexAction {
    Skip,
    Build,
    Rebuild,
}

/// Pure policy: given a table's row count and (if an index exists) its
/// unindexed-row delta, decide whether to build, rebuild, or skip.
///
/// `force` rebuilds an existing index regardless of its unindexed delta —
/// used by `rebuild_query_indexes` (`POST /admin/reindex`) after an index
/// *parameter* change (e.g. the IVF partition-count fix), where the on-disk
/// index is stale in shape, not in coverage, so the delta check would
/// otherwise Skip it. Tables below [`MIN_ROWS_TO_INDEX`] stay skipped even
/// under force — a flat scan is already sub-second and PQ can't train.
fn decide_index_action(row_count: usize, unindexed: Option<usize>, force: bool) -> IndexAction {
    if row_count < MIN_ROWS_TO_INDEX {
        return IndexAction::Skip;
    }
    match unindexed {
        None => IndexAction::Build,
        Some(_) if force => IndexAction::Rebuild,
        Some(n) if n > REINDEX_DELTA_THRESHOLD => IndexAction::Rebuild,
        Some(_) => IndexAction::Skip,
    }
}

/// Outcome of one [`LanceStore::ensure_query_indexes`] pass.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IndexMaintenanceStats {
    pub indexes_built: u64,
    pub indexes_rebuilt: u64,
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

    /// Ensure every [`MANAGED_INDEXES`] entry (vector ANN + scalar BTree)
    /// is up to date. Builds the index on first run; rebuilds once the
    /// unindexed delta grows past [`REINDEX_DELTA_THRESHOLD`]. Idempotent
    /// and read-safe with concurrent queries (Lance datasets are MVCC):
    /// readers keep using the prior version until the new index commits.
    /// Lazy/absent tables and tables below [`MIN_ROWS_TO_INDEX`] are
    /// skipped without erroring.
    ///
    /// The build itself can take seconds on a large table, so callers run
    /// this off the request path (the vacuum worker). The lance-native
    /// read path (`query().nearest_to(...)`) picks up the committed index
    /// transparently on its next query.
    pub async fn ensure_query_indexes(&self) -> Result<IndexMaintenanceStats, StorageError> {
        self.ensure_query_indexes_inner(false).await
    }

    /// Force-rebuild every managed index regardless of its unindexed delta.
    /// Used by `POST /admin/reindex` after an index *parameter* change (e.g.
    /// the IVF partition-count fix) where [`Self::ensure_query_indexes`]'s
    /// delta check would Skip an index that is stale in shape, not coverage.
    pub async fn rebuild_query_indexes(&self) -> Result<IndexMaintenanceStats, StorageError> {
        self.ensure_query_indexes_inner(true).await
    }

    async fn ensure_query_indexes_inner(
        &self,
        force: bool,
    ) -> Result<IndexMaintenanceStats, StorageError> {
        let mut agg = IndexMaintenanceStats::default();
        for (table_name, column, kind) in MANAGED_INDEXES {
            let table = match self.conn.open_table(*table_name).execute().await {
                Ok(t) => t,
                Err(lancedb::Error::TableNotFound { .. }) => {
                    agg.tables_skipped += 1;
                    continue;
                }
                Err(e) => return Err(lancedb_err(e)),
            };
            let row_count = table.count_rows(None).await.map_err(lancedb_err)?;
            // Find an existing index on the column and read its unindexed-row
            // delta (rows appended since the last build).
            let indices = table.list_indices().await.map_err(lancedb_err)?;
            let existing = indices
                .iter()
                .find(|c| c.columns.iter().any(|col| col == column));
            let unindexed = match existing {
                Some(cfg) => Some(
                    table
                        .index_stats(&cfg.name)
                        .await
                        .map_err(lancedb_err)?
                        .map(|s| s.num_unindexed_rows)
                        .unwrap_or(0),
                ),
                None => None,
            };
            let action = decide_index_action(row_count, unindexed, force);
            if action == IndexAction::Skip {
                agg.tables_skipped += 1;
                continue;
            }
            // `replace(true)` creates when absent and overwrites when
            // rebuilding. Vector indexes pin `num_partitions` explicitly —
            // lancedb 0.30's default derivation over-partitions our embedding
            // tables and the resulting empty KMeans clusters made lance's
            // reader return ragged record batches (see
            // [`ivf_num_partitions`]). PQ sub-vectors + the BTree scalar
            // index keep Lance's derived defaults (never the problem).
            let index = match kind {
                IndexKind::Vector => {
                    let num_partitions = ivf_num_partitions(row_count);
                    tracing::info!(
                        table = *table_name,
                        column = *column,
                        row_count,
                        num_partitions,
                        ?action,
                        "building IVF_PQ vector index"
                    );
                    Index::IvfPq(IvfPqIndexBuilder::default().num_partitions(num_partitions))
                }
                IndexKind::Scalar => Index::BTree(BTreeIndexBuilder::default()),
            };
            table
                .create_index(&[*column], index)
                .replace(true)
                .execute()
                .await
                .map_err(lancedb_err)?;
            match action {
                IndexAction::Build => agg.indexes_built += 1,
                IndexAction::Rebuild => agg.indexes_rebuilt += 1,
                IndexAction::Skip => unreachable!("skip handled above"),
            }
        }
        // Rebuild the Tantivy capsule + transcript BM25 indexes from the
        // live corpus (startup full-rebuild strategy — see
        // `crate::storage::fts`). Unlike the lance IVF/scalar indexes there's
        // no per-table delta tracking — the rebuild is cheap (<1s at real
        // scale) and a full rebuild is the whole design. The Tantivy index
        // backs the only BM25 read path (`bm25_candidate_ids` /
        // `bm25_transcript_candidates`); the eager rebuild here keeps it
        // fresh, and if it's ever skipped both readers still lazy-build on
        // first query (the `*_fts_built` latch), so the index is never
        // missing. This is what makes the "seed → rebuild_query_indexes →
        // query" test flow cover the capsule_fts AND transcript_fts buckets;
        // each counts as one rebuilt index in the stats.
        self.rebuild_capsule_fts().await?;
        agg.indexes_rebuilt += 1;
        self.rebuild_transcript_fts().await?;
        agg.indexes_rebuilt += 1;
        Ok(agg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_tables_are_skipped() {
        assert_eq!(decide_index_action(0, None, false), IndexAction::Skip);
        assert_eq!(
            decide_index_action(MIN_ROWS_TO_INDEX - 1, None, false),
            IndexAction::Skip
        );
        // Force does not override the floor — flat scan is sub-second and PQ
        // can't train on a tiny table.
        assert_eq!(
            decide_index_action(MIN_ROWS_TO_INDEX - 1, Some(0), true),
            IndexAction::Skip
        );
    }

    #[test]
    fn large_unindexed_table_builds() {
        assert_eq!(
            decide_index_action(MIN_ROWS_TO_INDEX, None, false),
            IndexAction::Build
        );
        assert_eq!(
            decide_index_action(1_000_000, None, false),
            IndexAction::Build
        );
    }

    #[test]
    fn fresh_index_is_left_alone_until_delta_grows() {
        assert_eq!(
            decide_index_action(50_000, Some(0), false),
            IndexAction::Skip
        );
        assert_eq!(
            decide_index_action(50_000, Some(REINDEX_DELTA_THRESHOLD), false),
            IndexAction::Skip
        );
        assert_eq!(
            decide_index_action(50_000, Some(REINDEX_DELTA_THRESHOLD + 1), false),
            IndexAction::Rebuild
        );
    }

    #[test]
    fn force_rebuilds_existing_index_below_delta() {
        // Stale-in-shape (parameter change), not stale-in-coverage: a small
        // delta would Skip without force, but rebuild_query_indexes forces it.
        assert_eq!(
            decide_index_action(50_000, Some(0), true),
            IndexAction::Rebuild
        );
        // No index yet → Build regardless of force.
        assert_eq!(decide_index_action(50_000, None, true), IndexAction::Build);
    }

    #[test]
    fn ivf_partitions_scale_with_rows_and_avoid_empty_clusters() {
        // The bug: lancedb's default gave 256 partitions for ~48k rows
        // (~187 rows/partition) → empty clusters. The fix targets ~1024
        // rows/partition, so 48k rows yields far fewer, well-trained ones.
        assert_eq!(ivf_num_partitions(48_000), 46);
        // Never zero (clamped to ≥1) even at the index floor.
        assert_eq!(ivf_num_partitions(MIN_ROWS_TO_INDEX), 4);
        assert!(ivf_num_partitions(1) >= 1);
        // Clamped to Lance's 4096 partition ceiling on huge tables.
        assert_eq!(ivf_num_partitions(usize::MAX), 4_096);
    }
}
