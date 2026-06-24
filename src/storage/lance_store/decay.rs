//! Bulk decay sweep (`apply_time_decay`) and retrieval-reinforcement
//! stamp (`bump_last_used_at`), both driven by background workers and
//! issued through the **LanceDB Rust API** (`table.update()`), the same
//! writer ingest uses.
//!
//! **History — why these moved off the DuckDB extension (route-B
//! Phase 2, 2026-06-24).** These two writes used to be the *only* code
//! that wrote *through the DuckDB lance extension* (every other write
//! already went through the LanceDB Rust API). That made the dataset a
//! **dual-writer**: the Rust API (ingest/status) and the DuckDB
//! extension (these decay UPDATEs) each held their own cached base
//! version. A DuckDB-side write on a stale base could lose a commit
//! race to the vacuum worker pruning that base's manifest, aborting
//! with `... <ver>.manifest was not found`. The old
//! `DuckDbQuery::with_commit_retry` (3 attempts) guarded exactly that.
//!
//! Routing these writes through `table.update()` makes decay and ingest
//! the **same single writer** (the Rust API), so the dual-writer +
//! pruned-stale-base race no longer exists. What remains is lance's own
//! optimistic-concurrency commit conflict between two Rust-API writers
//! committing against the same base — and **lance handles that natively
//! inside `table.update().execute()`**: lance 7.0's `UpdateBuilder`
//! runs `execute_with_retry` with a default budget of 10 retries / 30 s,
//! re-snapshotting (`checkout_latest()`) and backing off between
//! attempts. On exhaustion it surfaces `TooMuchWriteContention`.
//!
//! [`LanceStore::with_lance_commit_retry`] is a thin *outer* safety net
//! around lance's internal retry: if lance's own budget is ever
//! exhausted under extreme contention and a commit-conflict error
//! surfaces, we re-open the table (a fresh, latest-version handle) and
//! retry a small bounded number of times. It is also the deterministic
//! seam the unit tests drive. Benign (non-conflict) errors surface
//! immediately and are never retried.

use std::time::Duration;

use super::{lancedb_err, sql_quote, LanceStore};
use crate::storage::types::StorageError;

/// Max attempts (initial + retries) for the *outer* commit-conflict
/// safety net wrapping lance's own internal retry. Small + bounded:
/// lance already retries 10× internally, so reaching this layer means
/// sustained contention; a couple more fresh-handle attempts is plenty
/// and a hard cap guarantees we never spin.
const COMMIT_RETRY_MAX_ATTEMPTS: u32 = 3;

/// Classify an error as a retryable Lance commit conflict — the
/// optimistic-concurrency conflict between two Rust-API writers
/// (`Commit conflict ...` / `Retryable commit conflict ...`) or the
/// retry-budget-exhausted `TooMuchWriteContention` (`Too many
/// concurrent writers ...` / `Attempted N retries`). Matched on the
/// rendered message because it crosses the `lancedb::Error` →
/// [`StorageError`] boundary as an opaque `InvalidInput` string.
/// Requires a "commit"/"conflict"/"contention" marker so an unrelated
/// error (e.g. a binder/"not found") is never retried.
fn is_lance_commit_conflict(err: &StorageError) -> bool {
    let m = err.to_string().to_lowercase();
    m.contains("commit conflict")
        || m.contains("conflict")
        || m.contains("write contention")
        || m.contains("concurrent writer")
}

impl LanceStore {
    /// Outer safety net around lance's *internal* commit-conflict retry
    /// (see module docs). `run` is the actual `table.update().execute()`
    /// closure; it is called once per attempt. Only
    /// [`is_lance_commit_conflict`] errors are retried, each after a
    /// short backoff that lets the competing writer's commit land;
    /// anything else surfaces immediately. Bounded by
    /// [`COMMIT_RETRY_MAX_ATTEMPTS`]. Lance has already re-snapshotted
    /// to the latest version internally on each of its own retries, and
    /// `run` re-opens the table each call, so every attempt here starts
    /// from a fresh base.
    async fn with_lance_commit_retry<F, Fut>(&self, mut run: F) -> Result<(), StorageError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<(), StorageError>>,
    {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match run().await {
                Ok(()) => return Ok(()),
                Err(e) if attempt < COMMIT_RETRY_MAX_ATTEMPTS && is_lance_commit_conflict(&e) => {
                    tokio::time::sleep(Duration::from_millis(50 * attempt as u64)).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Bulk decay sweep over the `capability_capsules` table, issued as
    /// three `table.update()` statements via the LanceDB Rust API.
    /// Driven by the decay worker (once per hour).
    ///
    /// Semantics are preserved verbatim from the prior DuckDB-extension
    /// implementation:
    /// 1. **Hard expiry FIRST** — archive any active capsule whose
    ///    `expires_at` deadline has passed, so expired rows leave the
    ///    active set before the decay passes touch them. String
    ///    comparison is valid because `expires_at` and `now_ms_str`
    ///    share the 20-digit zero-padded ms format.
    /// 2. **Decay, used rows** — `decay_score = least(1.0, decay_score +
    ///    rate * ((now - last_used_at) / ms_per_day))`, anchored on
    ///    `last_used_at` (the decay clock), `WHERE ... last_used_at IS
    ///    NOT NULL`.
    /// 3. **Decay, never-used rows** — same, anchored on `updated_at`,
    ///    `WHERE ... last_used_at IS NULL`.
    ///
    /// Two WHERE-disjoint decay passes (not `COALESCE(last_used_at,
    /// updated_at)`): NOT-NULL first so the IS-NULL pass can't re-hit a
    /// row the first pass just stamped. Each eligible row is updated
    /// exactly once. Both passes advance `last_used_at` (the decay
    /// clock), NOT `updated_at` (which reverts to its plain "last write"
    /// meaning). `decay_score` stays an additive accumulator, so
    /// feedback's decay deltas are preserved.
    ///
    /// `now_ms` is the current ms timestamp (numeric, for the arithmetic);
    /// `now_ms_str` is the same value zero-padded to the 20-char sortable
    /// string (the new `last_used_at`). `decay_rate_per_day` is the
    /// per-day delta multiplier; `ms_per_day` is the time-base divisor.
    ///
    /// Wrapped in [`Self::with_lance_commit_retry`]; lance's own
    /// `table.update()` additionally retries commit conflicts internally
    /// (see module docs).
    pub async fn apply_time_decay(
        &self,
        decay_rate_per_day: f64,
        now_ms: f64,
        ms_per_day: f64,
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        let now_ms_str = now_ms_str.to_string();
        self.with_lance_commit_retry(|| {
            let now_ms_str = now_ms_str.clone();
            async move {
                let table = self
                    .conn
                    .open_table("capability_capsules")
                    .execute()
                    .await
                    .map_err(lancedb_err)?;

                // (a) Hard expiry — archive expired active rows first.
                table
                    .update()
                    .only_if(format!(
                        "status = 'active' AND expires_at IS NOT NULL AND expires_at <= {}",
                        sql_quote(&now_ms_str)
                    ))
                    .column("status", "'archived'")
                    .execute()
                    .await
                    .map_err(lancedb_err)?;

                // (b) Decay, used rows — anchored on last_used_at.
                table
                    .update()
                    .only_if("status = 'active' AND decay_score < 1.0 AND last_used_at IS NOT NULL")
                    .column(
                        "decay_score",
                        format!(
                            "least(1.0, decay_score + {decay_rate_per_day} * (({now_ms} - CAST(last_used_at AS double)) / {ms_per_day}))"
                        ),
                    )
                    .column("last_used_at", sql_quote(&now_ms_str))
                    .execute()
                    .await
                    .map_err(lancedb_err)?;

                // (c) Decay, never-used rows — anchored on updated_at.
                table
                    .update()
                    .only_if("status = 'active' AND decay_score < 1.0 AND last_used_at IS NULL")
                    .column(
                        "decay_score",
                        format!(
                            "least(1.0, decay_score + {decay_rate_per_day} * (({now_ms} - CAST(updated_at AS double)) / {ms_per_day}))"
                        ),
                    )
                    .column("last_used_at", sql_quote(&now_ms_str))
                    .execute()
                    .await
                    .map_err(lancedb_err)?;

                Ok(())
            }
        })
        .await
    }

    /// Mark a set of capsules as *used* by stamping `last_used_at = now`
    /// (the decay clock) **and** `last_recalled_at = now` (the durable,
    /// sweep-proof recall signal — only this path writes it). Issued via
    /// `table.update()` (LanceDB Rust API). Driven off the read path by
    /// `crate::worker::last_used_worker`. Tenant-scoped. Empty `ids` is
    /// a no-op.
    ///
    /// One UPDATE per id with a single-equality predicate (`tenant = ?
    /// AND capability_capsule_id = ?`). Batches are coalesced upstream,
    /// so the id count per call is small. Returns `()` rather than a
    /// rowcount — reinforcement is best-effort.
    ///
    /// Wrapped in [`Self::with_lance_commit_retry`]; lance's own
    /// `table.update()` additionally retries commit conflicts internally
    /// (see module docs).
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
        self.with_lance_commit_retry(|| {
            let tenant = tenant.clone();
            let now = now.clone();
            let ids = ids.clone();
            async move {
                let table = self
                    .conn
                    .open_table("capability_capsules")
                    .execute()
                    .await
                    .map_err(lancedb_err)?;
                for id in &ids {
                    // Stamp BOTH columns on a real recall: `last_used_at`
                    // is the decay clock (the hourly sweep also writes
                    // it), while `last_recalled_at` is the durable,
                    // sweep-proof recall signal — only this path ever
                    // writes it, so `last_recalled_at IS NULL` reliably
                    // means "never recalled since creation".
                    table
                        .update()
                        .only_if(format!(
                            "tenant = {} AND capability_capsule_id = {}",
                            sql_quote(&tenant),
                            sql_quote(id),
                        ))
                        .column("last_used_at", sql_quote(&now))
                        .column("last_recalled_at", sql_quote(&now))
                        .execute()
                        .await
                        .map_err(lancedb_err)?;
                }
                Ok(())
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
    use std::sync::Arc;
    use tempfile::tempdir;

    /// Verbatim shape of the lance commit-conflict family that surfaces
    /// when lance's *internal* retry budget is exhausted under sustained
    /// contention (the outer safety net this module adds catches it).
    fn conflict_err() -> StorageError {
        StorageError::InvalidInput(
            "lancedb: Too many concurrent writers. Attempted 10 retries., \
             /x/dataset/write/retry.rs:130"
                .to_string(),
        )
    }

    /// The retryable-commit-conflict variant lance renders before it
    /// exhausts its internal budget — also retryable here.
    fn retryable_conflict_err() -> StorageError {
        StorageError::InvalidInput(
            "lancedb: Commit conflict for version 42: concurrent writer advanced \
             the dataset"
                .to_string(),
        )
    }

    #[test]
    fn is_lance_commit_conflict_matches_conflict_and_contention_but_not_benign() {
        // Retry-budget-exhausted contention → retryable.
        assert!(is_lance_commit_conflict(&conflict_err()));
        // The optimistic-concurrency commit-conflict family → retryable.
        assert!(is_lance_commit_conflict(&retryable_conflict_err()));
        assert!(is_lance_commit_conflict(&StorageError::InvalidInput(
            "lancedb: Retryable commit conflict for version 7: ...".into()
        )));
        // Benign errors must NOT be retried — even one mentioning
        // "not found" that is unrelated to a commit.
        assert!(!is_lance_commit_conflict(&StorageError::InvalidInput(
            "lancedb: column \"foo\" not found".into()
        )));
        assert!(!is_lance_commit_conflict(&StorageError::NotFound(
            "capsule"
        )));
        assert!(!is_lance_commit_conflict(&StorageError::InvalidData(
            "bad data"
        )));
    }

    async fn fixture_store() -> (tempfile::TempDir, LanceStore) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();
        (dir, lance)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_lance_commit_retry_retries_conflict_then_succeeds() {
        let (_d, lance) = fixture_store().await;
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        // Fails the first attempt with a commit conflict, succeeds the
        // second — the "decay/last_used is not lost a tick" guarantee:
        // the operation ultimately commits after a fresh-handle retry.
        let res = lance
            .with_lance_commit_retry(|| {
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
    async fn with_lance_commit_retry_does_not_retry_non_conflict() {
        let (_d, lance) = fixture_store().await;
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let res = lance
            .with_lance_commit_retry(|| {
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
    async fn with_lance_commit_retry_exhausts_on_persistent_conflict() {
        let (_d, lance) = fixture_store().await;
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let res = lance
            .with_lance_commit_retry(|| {
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
