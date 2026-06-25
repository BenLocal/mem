//! `MaintenanceStore` for [`ClickHouseBackend`] (the 3 REQUIRED methods).
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P5).** The 3 default methods (`vacuum_old_versions`,
//! `ensure_query_indexes`, `rebuild_query_indexes`) keep their zero-stats
//! no-op trait defaults — NOT overridden here (overriding with a panic would
//! crash the vacuum worker). Only the required ones are implemented.
//!
//! `apply_time_decay` is the update-heavy worst case for an OLAP store: it's
//! issued as an `ALTER … UPDATE` mutation (async, rewrites parts — NOT a hot
//! path). A versioned read-modify-reinsert of every active row, or a TTL/
//! materialized-decay scheme, is the validation-phase alternative (§10).

use async_trait::async_trait;

use super::backend::ClickHouseBackend;
use super::capsule_store::{ch_err, enum_to_str, ChCapsuleRow};
use crate::domain::capability_capsule::{CapabilityCapsuleRecord, CapabilityCapsuleType};
use crate::storage::lance_store::VacuumStats;
use crate::storage::types::StorageError;
use crate::storage::MaintenanceStore;

#[async_trait]
impl MaintenanceStore for ClickHouseBackend {
    async fn apply_time_decay(
        &self,
        decay_rate_per_day: f64,
        now_ms: f64,
        ms_per_day: f64,
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        // Hard-expiry first: rows past `expires_at` (non-empty, <= now) leave
        // the active pool. Then decay active rows by the time since their
        // decay anchor (last_used_at if set, else updated_at), capped at 1.0.
        // ALTER … UPDATE is an async mutation — heavy, never on a read path.
        self.client
            .query(
                "ALTER TABLE capability_capsules UPDATE status = 'Archived' \
                 WHERE status = 'Active' AND expires_at != '' AND expires_at <= ?",
            )
            .bind(now_ms_str)
            .execute()
            .await
            .map_err(ch_err)?;
        self.client
            .query(
                "ALTER TABLE capability_capsules UPDATE decay_score = \
                 least(1.0, decay_score + ? * ((? - toFloat64OrZero( \
                 if(last_used_at != '', last_used_at, updated_at))) / ?)) \
                 WHERE status = 'Active' AND decay_score < 1.0",
            )
            .bind(decay_rate_per_day)
            .bind(now_ms)
            .bind(ms_per_day)
            .execute()
            .await
            .map_err(ch_err)?;
        Ok(())
    }

    async fn vacuum_old_versions_with(
        &self,
        _older_than_days: i64,
        _aggressive: bool,
    ) -> Result<VacuumStats, StorageError> {
        // CH has no Lance version manifests. The nearest analogue is forcing a
        // merge so ReplacingMergeTree collapses superseded row versions. We
        // OPTIMIZE the lifecycle tables FINAL; stats are zeroed (the Lance
        // manifest/fragment counters have no CH meaning — documented in §6/§10).
        for table in [
            "capability_capsules",
            "feedback_events",
            "graph_edges",
            "entities",
            "entity_aliases",
            "sessions",
            "episodes",
            "embedding_jobs",
            "transcript_embedding_jobs",
            "evolution_candidates",
        ] {
            self.client
                .query(&format!("OPTIMIZE TABLE {table} FINAL"))
                .execute()
                .await
                .map_err(ch_err)?;
        }
        Ok(VacuumStats::default())
    }

    async fn auto_promote_candidates(
        &self,
        tenant: &str,
        cutoff_updated_at: &str,
        types: &[CapabilityCapsuleType],
        max_decay_score: f32,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        if types.is_empty() {
            return Ok(Vec::new());
        }
        // Inline the (fixed-vocabulary, safe) enum strings for the IN clause.
        let in_list = types
            .iter()
            .map(|t| format!("'{}'", enum_to_str(t)))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT ?fields FROM capability_capsules FINAL \
             WHERE tenant = ? AND status = 'PendingConfirmation' \
             AND updated_at < ? AND decay_score <= ? \
             AND capability_capsule_type IN ({in_list}) \
             ORDER BY updated_at ASC"
        );
        let rows = self
            .client
            .query(&sql)
            .bind(tenant)
            .bind(cutoff_updated_at)
            .bind(max_decay_score)
            .fetch_all::<ChCapsuleRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().map(ChCapsuleRow::into_record).collect())
    }
}
