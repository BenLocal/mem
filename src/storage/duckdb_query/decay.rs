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
            // Hard expiry (Supermemory-style auto-forget): archive any active
            // capsule whose `expires_at` deadline has passed. Deterministic —
            // the caller declared the deadline — so unlike idle-archive this
            // needs no gate / dry-run. Run it FIRST so expired rows leave the
            // active set before the decay passes touch it. String comparison
            // is valid because `expires_at` and `now_ms_str` share the
            // 20-digit zero-padded ms format. `expires_at IS NULL` (the
            // default) is never matched, so this is a no-op for almost every
            // capsule.
            conn.execute(
                "UPDATE ns.main.capability_capsules SET status = 'archived' \
                 WHERE status = 'active' AND expires_at IS NOT NULL AND expires_at <= ?1",
                params![now_ms_str],
            )
            .map_err(StorageError::DuckDb)?;
            // Roadmap O1 — retrieval reinforcement. The decay clock is
            // anchored on the capsule's *last touch* = `last_used_at` if
            // it has ever been used (emitted into a retrieval response,
            // which bumps `last_used_at`), else `updated_at` (legacy /
            // never-retrieved rows). A capsule used more recently than it
            // was written accrues a smaller per-tick slice, so
            // frequently-retrieved capsules decay slower than untouched
            // ones.
            //
            // The per-tick reset advances `last_used_at` (not
            // `updated_at`): `last_used_at` is the decay clock, and
            // `updated_at` reverts to its plain "last write" meaning
            // (a real freshness signal again, no longer flattened by the
            // hourly sweep). `decay_score` stays an additive accumulator,
            // so feedback's decay deltas are preserved (not clobbered).
            //
            // Two statements rather than `COALESCE(last_used_at,
            // updated_at)`: the lance DuckDB extension rejects COALESCE
            // inside an UPDATE SET expression ("Not implemented"). The
            // two WHERE-disjoint passes are equivalent — NOT-NULL first
            // so the IS-NULL pass can't re-hit a row the first pass just
            // stamped. Each eligible row is updated exactly once.
            conn.execute(
                "UPDATE ns.main.capability_capsules \
                 SET decay_score = least(1.0, decay_score + ?1 * ((?2 - last_used_at::double) / ?3)), \
                     last_used_at = ?4 \
                 WHERE status = 'active' AND decay_score < 1.0 AND last_used_at IS NOT NULL",
                params![decay_rate_per_day, now_ms, ms_per_day, now_ms_str],
            )
            .map_err(StorageError::DuckDb)?;
            conn.execute(
                "UPDATE ns.main.capability_capsules \
                 SET decay_score = least(1.0, decay_score + ?1 * ((?2 - updated_at::double) / ?3)), \
                     last_used_at = ?4 \
                 WHERE status = 'active' AND decay_score < 1.0 AND last_used_at IS NULL",
                params![decay_rate_per_day, now_ms, ms_per_day, now_ms_str],
            )
            .map_err(StorageError::DuckDb)?;
            Ok(())
        })
        .await
    }

    /// Mark a set of capsules as *used* by stamping `last_used_at = now`
    /// (the decay clock) **and** `last_recalled_at = now` (the durable,
    /// sweep-proof recall signal — only this path writes it).
    /// Called off the read path by the last-used worker
    /// (`crate::worker::last_used_worker`), which coalesces the
    /// capability_capsule_ids emitted into retrieval responses over a
    /// drain window and flushes them in one batched UPDATE. Resetting
    /// `last_used_at` pushes the decay clock forward, so the next
    /// `apply_time_decay` sweep accrues a smaller slice for these rows
    /// (see that method's anchor). Tenant-scoped. Returns the number of
    /// Empty `capability_capsule_ids` is a no-op.
    ///
    /// Like the decay sweep, this is a DuckDB-side UPDATE through the
    /// lance extension, which invalidates the connection's own snapshot
    /// cache automatically — no `Store::refresh` round-trip needed.
    /// Returns `()` rather than a rowcount: the lance extension reports
    /// an unreliable `rows_changed` for DML (it can be 0 even when rows
    /// were updated — same quirk as LanceDB's delete/update path), so a
    /// count here would be misleading. Reinforcement is best-effort.
    pub async fn bump_last_used_at(
        &self,
        tenant: &str,
        capability_capsule_ids: &[String],
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        if capability_capsule_ids.is_empty() {
            return Ok(());
        }
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let now = now_ms_str.to_string();
        let ids: Vec<String> = capability_capsule_ids.to_vec();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            // One UPDATE per id with a single-equality predicate. The
            // lance extension's UPDATE rejects a multi-value
            // `capability_capsule_id IN (...)` filter ("Not implemented:
            // Lance UPDATE does not support one or more pushed table
            // filters"), so a batched IN-list is not viable; per-id
            // equality (`tenant = ? AND capability_capsule_id = ?`) is
            // the supported shape. Batches are coalesced upstream, so the
            // id count per call is small.
            for id in &ids {
                // Stamp BOTH columns on a real recall: `last_used_at` is the
                // decay clock (the hourly sweep also writes it, for O1
                // reinforcement), while `last_recalled_at` is the durable,
                // sweep-proof recall signal — only this path ever writes it,
                // so `last_recalled_at IS NULL` reliably means "never
                // recalled since creation" (Step-1 governance fix).
                conn.execute(
                    "UPDATE ns.main.capability_capsules \
                     SET last_used_at = ?1, last_recalled_at = ?1 \
                     WHERE tenant = ?2 AND capability_capsule_id = ?3",
                    params![now, tenant, id],
                )
                .map_err(StorageError::DuckDb)?;
            }
            Ok(())
        })
        .await
    }
}
