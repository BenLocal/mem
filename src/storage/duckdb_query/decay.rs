//! Bulk writes issued through the DuckDB lance extension — the
//! time-decay sweep (`apply_time_decay`) and the retrieval-reinforcement
//! stamp (`bump_last_used_at`), both driven by background workers.
//!
//! **Why these refresh before writing (and retry on conflict).** The
//! same Lance dataset has *two independent writers*: the LanceDB Rust
//! API (`Store`'s `commit_lance_write` path — ingest, status, sessions)
//! and the DuckDB lance extension (these two bulk SQL `UPDATE`s). Each
//! holds its own view of the dataset version. A DuckDB-side `UPDATE`
//! commits as a Lance transaction whose *base version* is whatever the
//! connection currently has attached; if that base is stale (the Rust
//! API writer has advanced the dataset since) and the vacuum worker has
//! pruned the base version's manifest down to Lance's in-flight floor —
//! a floor that only protects the *Rust API* writer's in-flight commits,
//! not this connection's cached base — the commit aborts with
//! `TransactionContext Error: Failed to commit ... <ver>.manifest was
//! not found`. (An earlier comment here claimed DuckDB writes
//! "self-invalidate the snapshot cache so no refresh is needed"; that is
//! true for read-after-write *visibility* but NOT for the *write's base
//! version*, which is the bug this module now guards against.)
//!
//! Mitigation ([`DuckDbQuery::with_commit_retry`]): pull the base to the
//! latest committed version via `ensure_fresh()` *before* the write
//! (shrinking the prune-race window), and if the commit still loses the
//! race, `refresh()` to a current base and retry a bounded number of
//! times. The write is a single batched SQL `UPDATE` either way — no
//! Rust-side full-table rewrite. Lance commits are atomic, so a losing
//! attempt is a no-op (no partial write); the retry — or, in the worst
//! case, the worker's next tick — applies the cumulative delta, so no
//! decay/reinforcement tick is permanently lost.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use duckdb::{params, Connection};

use super::{spawn_blocking_storage, DuckDbQuery};
use crate::storage::types::StorageError;

/// Max attempts (initial + retries) for a DuckDB-extension write that
/// can lose a commit race to the vacuum worker's manifest pruning.
/// Small + bounded: the race is rare and a fresh base almost always
/// wins on the first retry; a hard cap guarantees we never spin.
const COMMIT_RETRY_MAX_ATTEMPTS: u32 = 3;

/// Classify an error as a retryable Lance commit race — the
/// pruned-base-manifest abort (`Failed to commit ... manifest ... not
/// found`) or the concurrent-writer commit conflict. Matched on the
/// rendered message because it crosses the DuckDB → lance-extension
/// boundary as an opaque `TransactionContext Error` string. Requires a
/// "commit" marker so an unrelated "not found" (e.g. a binder error) is
/// never retried.
fn is_lance_commit_conflict(err: &StorageError) -> bool {
    let m = err.to_string().to_lowercase();
    m.contains("commit")
        && (m.contains("manifest") || m.contains("conflict") || m.contains("not found"))
}

impl DuckDbQuery {
    /// Run a DuckDB-extension write with base-version freshening and
    /// bounded retry on a Lance commit race (see module docs).
    ///
    /// `run` is handed the (possibly just-refreshed) connection handle
    /// and performs the actual `spawn_blocking` SQL `UPDATE`(s). It is
    /// called once per attempt: the first attempt after `ensure_fresh()`
    /// (cheap — refreshes only if a Rust-API write marked the snapshot
    /// dirty), each retry after a forced `refresh()` (re-ATTACH at the
    /// now-latest version) plus a short backoff. Only
    /// [`is_lance_commit_conflict`] errors are retried; anything else
    /// surfaces immediately. Bounded by [`COMMIT_RETRY_MAX_ATTEMPTS`].
    async fn with_commit_retry<F, Fut>(&self, mut run: F) -> Result<(), StorageError>
    where
        F: FnMut(Arc<Mutex<Connection>>) -> Fut,
        Fut: std::future::Future<Output = Result<(), StorageError>>,
    {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            if attempt == 1 {
                // Pull the write's base version up to the latest committed
                // version (only does work if a Rust-API write marked the
                // snapshot dirty), shrinking the window in which a
                // concurrent vacuum prune of an older base manifest aborts
                // the commit.
                self.ensure_fresh().await?;
            } else {
                // A prior attempt lost the commit race: its base manifest
                // was pruned out from under it. Force a full rebuild
                // (re-ATTACH at the now-latest version) regardless of the
                // dirty flag, back off briefly, then retry.
                self.refresh().await?;
                tokio::time::sleep(Duration::from_millis(50 * attempt as u64)).await;
            }
            match run(self.conn.clone()).await {
                Ok(()) => return Ok(()),
                Err(e) if attempt < COMMIT_RETRY_MAX_ATTEMPTS && is_lance_commit_conflict(&e) => {
                    continue
                }
                Err(e) => return Err(e),
            }
        }
    }

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
    /// Refreshes the connection's base version before writing and
    /// retries on a Lance commit race — see [`Self::with_commit_retry`]
    /// and the module docs for why (dual-writer + vacuum prune). Still a
    /// single batched SQL `UPDATE` per pass, no Rust-side iteration.
    pub async fn apply_time_decay(
        &self,
        decay_rate_per_day: f64,
        now_ms: f64,
        ms_per_day: f64,
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        let now_ms_str = now_ms_str.to_string();
        self.with_commit_retry(move |conn| {
            let now_ms_str = now_ms_str.clone();
            async move {
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
    /// lance extension, so it refreshes its base version before writing
    /// and retries on a Lance commit race — see
    /// [`Self::with_commit_retry`] and the module docs.
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
        let tenant = tenant.to_string();
        let now = now_ms_str.to_string();
        let ids: Vec<String> = capability_capsule_ids.to_vec();
        self.with_commit_retry(move |conn| {
            let tenant = tenant.clone();
            let now = now.clone();
            let ids = ids.clone();
            async move {
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
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::lance_store::LanceStore;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::tempdir;

    /// Verbatim shape of the production DuckDB lance-extension commit
    /// failure observed when a concurrent vacuum prunes the base
    /// manifest out from under a stale-base write (see module docs).
    fn conflict_err() -> StorageError {
        StorageError::InvalidInput(
            "duckdb error: TransactionContext Error: Failed to commit: Failed to commit \
             Lance append transaction for '/x/capability_capsules.lance' (Lance error: \
             Dataset at path .../_versions/3445.manifest was not found: Not found: \
             .../_versions/3445.manifest (code=25))"
                .to_string(),
        )
    }

    #[test]
    fn is_lance_commit_conflict_matches_prune_and_conflict_but_not_benign() {
        // The pruned-base-manifest commit failure → retryable.
        assert!(is_lance_commit_conflict(&conflict_err()));
        // The concurrent-writer commit-conflict family → retryable.
        assert!(is_lance_commit_conflict(&StorageError::InvalidInput(
            "Commit conflict: concurrent writer advanced the dataset".into()
        )));
        // Benign errors must NOT be retried — even one that mentions
        // "not found" but is unrelated to a commit.
        assert!(!is_lance_commit_conflict(&StorageError::InvalidInput(
            "Binder Error: column \"foo\" not found".into()
        )));
        assert!(!is_lance_commit_conflict(&StorageError::NotFound(
            "capsule"
        )));
        assert!(!is_lance_commit_conflict(&StorageError::InvalidData(
            "bad data"
        )));
    }

    async fn fixture_query() -> (tempfile::TempDir, DuckDbQuery) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        // LanceStore::open creates the dir + datasets so ATTACH (and the
        // refresh()/ensure_fresh() the retry wrapper runs) succeed.
        let _lance = LanceStore::open(&path).await.unwrap();
        let q = DuckDbQuery::open(&path).await.unwrap();
        (dir, q)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_commit_retry_retries_conflict_then_succeeds() {
        let (_d, q) = fixture_query().await;
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        // Fails the first attempt with a commit conflict, succeeds the
        // second — the "decay/last_used is not lost a tick" guarantee:
        // the operation ultimately commits after a refresh.
        let res = q
            .with_commit_retry(move |_conn| {
                let c = c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        Err(conflict_err())
                    } else {
                        Ok(())
                    }
                }
            })
            .await;
        assert!(res.is_ok(), "should succeed on the retry: {res:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "ran once (conflict) then retried once (success)"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_commit_retry_does_not_retry_non_conflict() {
        let (_d, q) = fixture_query().await;
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let res = q
            .with_commit_retry(move |_conn| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err(StorageError::InvalidData("unrelated failure"))
                }
            })
            .await;
        assert!(res.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a non-conflict error must surface immediately, no retry"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_commit_retry_exhausts_on_persistent_conflict() {
        let (_d, q) = fixture_query().await;
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let res = q
            .with_commit_retry(move |_conn| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err(conflict_err())
                }
            })
            .await;
        assert!(res.is_err(), "persistent conflict ultimately fails");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            COMMIT_RETRY_MAX_ATTEMPTS,
            "bounded: stops after COMMIT_RETRY_MAX_ATTEMPTS, no infinite loop"
        );
    }
}
