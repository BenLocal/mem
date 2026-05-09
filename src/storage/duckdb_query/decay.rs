//! Bulk write — time-decay sweep over `memories`. Called by the
//! decay worker on its periodic tick. Lives outside the
//! "reads-only" surface of `DuckDbQuery` because writes via DuckDB
//! SQL automatically invalidate the connection's own snapshot cache,
//! so no `refresh()` round-trip is needed (LanceStore Rust API writes
//! do require the refresh — see [`super::DuckDbQuery::refresh`]).

use duckdb::params;

use super::{spawn_blocking_storage, DuckDbQuery};
use crate::storage::types::StorageError;

impl DuckDbQuery {
    /// Bulk decay sweep: increment `memories.decay_score` for every
    /// active row by a fraction of the days elapsed since
    /// `updated_at`, capped at 1.0, and bump `updated_at` to `now`.
    /// Used by the decay worker (called once per hour). Issued via
    /// DuckDB SQL through the lance extension — single statement,
    /// no Rust-side iteration.
    ///
    /// `now_ms` is the current timestamp in milliseconds (numeric);
    /// `now_ms_str` is the same value zero-padded to the 20-char
    /// string that mem uses for sortable timestamps.
    /// `decay_rate_per_day` is the per-day delta multiplier (e.g.
    /// `0.01` = 1% / day); `ms_per_day` is the time-base divisor
    /// (constant 86_400_000 in production but exposed for tests).
    ///
    /// Writes via DuckDB-side SQL invalidate the connection's own
    /// cache automatically (LanceStore Rust API writes do not — see
    /// [`super::DuckDbQuery::refresh`] doc), so no manual refresh is
    /// needed here.
    pub async fn apply_time_decay(
        &self,
        decay_rate_per_day: f64,
        now_ms: f64,
        ms_per_day: f64,
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn.clone();
        let now_ms_str = now_ms_str.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            conn.execute(
                "UPDATE ns.main.capability_capsules \
                 SET decay_score = least(1.0, decay_score + ?1 * ((?2 - updated_at::double) / ?3)), \
                     updated_at = ?4 \
                 WHERE status = 'active' AND decay_score < 1.0",
                params![decay_rate_per_day, now_ms, ms_per_day, now_ms_str],
            )
            .map_err(StorageError::DuckDb)?;
            Ok(())
        })
        .await
    }
}
