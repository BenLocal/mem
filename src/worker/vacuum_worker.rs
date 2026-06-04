//! Periodic Lance vacuum sweep. Prunes version manifests older than
//! `older_than_days` across every managed Lance table on a fixed
//! cadence. Spawned by `app::AppState::from_config` unless
//! `MEM_VACUUM_DISABLED=1` is set.
//!
//! Why a worker exists at all: Lance is copy-on-write. The
//! `transcript_embedding_jobs` table sees one UPDATE per claim and
//! one per completion — by the time a few thousand transcript blocks
//! are processed, the table directory's `_versions/` subfolder
//! holds thousands of manifests totalling several GB. The actual
//! row data is tens of MB at most. Vacuum is pure maintenance —
//! query results are unchanged — so this worker mirrors
//! `decay_worker`'s always-on stance instead of `auto_promote_worker`'s
//! opt-in stance.

use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::VacuumSettings;
use crate::storage::{Backend, VacuumStats};

/// Long-running loop. Returns immediately when
/// `settings.disabled == true`, so callers can spawn unconditionally
/// and let the gate live in one place.
pub async fn run(store: Arc<dyn Backend>, settings: VacuumSettings) {
    if settings.disabled {
        return;
    }
    let interval = StdDuration::from_secs(settings.interval_secs);
    info!(
        interval_secs = settings.interval_secs,
        older_than_days = settings.older_than_days,
        aggressive = settings.aggressive,
        "vacuum_worker started",
    );
    // Build the query indexes (vector ANN + scalar BTree) promptly at
    // startup (before the first sleep) — without them `lance_vector_search`
    // brute-forces the transcript embeddings table and the transcript JOIN
    // flat-scans conversation_messages (together 5–11s). One-time on a fresh
    // build, skipped thereafter; the in-loop call below folds in later growth.
    ensure_query_indexes_once(&*store).await;
    loop {
        sleep(interval).await;
        match sweep_once(
            &*store,
            settings.older_than_days as i64,
            settings.aggressive,
        )
        .await
        {
            Ok(stats) if stats.bytes_removed > 0 => {
                info!(
                    bytes_removed = stats.bytes_removed,
                    old_versions_removed = stats.old_versions_removed,
                    tables_pruned = stats.tables_pruned,
                    tables_skipped = stats.tables_skipped,
                    "vacuum: reclaimed {} bytes across {} table(s)",
                    stats.bytes_removed,
                    stats.tables_pruned,
                );
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "vacuum sweep failed"),
        }
        ensure_query_indexes_once(&*store).await;
    }
}

/// Build/refresh the query indexes (vector ANN + scalar BTree) once,
/// logging the outcome. Errors are logged and swallowed — index
/// maintenance must never take down the worker loop (a failed build just
/// leaves the prior/flat behavior).
async fn ensure_query_indexes_once(store: &dyn Backend) {
    match store.ensure_query_indexes().await {
        Ok(stats) if stats.indexes_built + stats.indexes_rebuilt > 0 => info!(
            indexes_built = stats.indexes_built,
            indexes_rebuilt = stats.indexes_rebuilt,
            "vacuum: query index built/rebuilt",
        ),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "query-index maintenance failed"),
    }
}

/// One sweep pass. Extracted so `POST /admin/vacuum` and the
/// integration tests can drive the same logic without spinning up
/// the loop. `aggressive=true` bypasses the 7-day in-flight safety
/// floor (default for the single-writer local-first deploy).
pub async fn sweep_once(
    store: &dyn Backend,
    older_than_days: i64,
    aggressive: bool,
) -> Result<VacuumStats, crate::storage::StorageError> {
    store
        .vacuum_old_versions_with(older_than_days, aggressive)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType,
    };
    use crate::storage::Store;
    use tempfile::tempdir;

    fn fixture(id: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.into(),
            tenant: "t".into(),
            capability_capsule_type: CapabilityCapsuleType::Experience,
            status: CapabilityCapsuleStatus::Active,
            content_hash: "h".repeat(64),
            source_agent: "test".into(),
            created_at: "00000000000000000000".into(),
            updated_at: "00000000000000000000".into(),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_returns_stats_on_fresh_store() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("vac.lance")).await.unwrap();
        // No writes — everything except the create-table commits is
        // recent. With `older_than_days=0` the call still succeeds
        // and just reports zero-ish numbers, but the eagerly created
        // tables all exist so `tables_pruned` > 0.
        let stats = sweep_once(&store, 0, true).await.unwrap();
        assert!(
            stats.tables_pruned > 0,
            "expected to visit at least one eagerly-created table, got {stats:?}",
        );
        // Lazy embedding tables aren't open yet on a fresh store —
        // they bump the skipped counter.
        assert!(stats.tables_skipped > 0, "stats: {stats:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_reclaims_versions_after_writes() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("vac.lance")).await.unwrap();
        // Force many version manifests on the capability_capsules
        // table by inserting + updating a row repeatedly.
        store.insert_capability_capsule(fixture("a")).await.unwrap();
        for _ in 0..20 {
            // accept_pending is a no-op when already active, but
            // still writes a new manifest version. (Same as the
            // production workload from the embedding worker.)
            let _ = store.accept_pending("t", "a").await;
        }
        let before = sweep_once(&store, 999_999, true).await.unwrap();
        assert_eq!(before.bytes_removed, 0, "high cutoff must remove nothing");

        let after = sweep_once(&store, 0, true).await.unwrap();
        assert!(
            after.bytes_removed > 0 || after.old_versions_removed > 0,
            "older_than=0 should reclaim something; got {after:?}",
        );
    }
}
