//! Last-used worker (roadmap O1 — retrieval reinforcement).
//!
//! `search` enqueues a [`CapsuleUsed`] event for every capsule it emits
//! into a compressed retrieval response (a cheap, non-blocking `send` —
//! no DB write on the read path). This worker drains the channel on an
//! interval, coalesces bursts, and stamps `last_used_at = now` on each
//! distinct capsule via [`Store::bump_last_used_at`] — off the read
//! path, so retrieves never write.
//!
//! `last_used_at` is the decay clock: pushing it forward makes the next
//! `apply_time_decay` sweep accrue a smaller slice for these rows, so
//! capsules that keep getting retrieved decay slower than untouched
//! ones (see `decay_worker` / `LanceStore::apply_time_decay`).
//!
//! Events are best-effort: a process restart drops whatever was queued.
//! That's acceptable — "used" is a statistical salience signal, not
//! exact accounting, and a capsule that keeps being retrieved will be
//! re-enqueued on the next search. Coalescing within a drain window
//! bounds write pressure to one batched UPDATE per tenant per tick,
//! which is the pragmatic stand-in for the "at most once per session
//! per capsule" budget (the HTTP service is session-stateless).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, warn};

use crate::storage::Store;

/// A capsule-used event: a capsule that `search` surfaced into its
/// response. Deduped within a drain window so a burst of the same
/// capsule collapses to a single `last_used_at` stamp.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapsuleUsed {
    pub tenant: String,
    pub capability_capsule_id: String,
}

/// Drain everything currently queued, coalesce, and stamp
/// `last_used_at = now` on each distinct capsule. Groups by tenant so
/// each tenant's ids flush in one batched UPDATE. Returns the number of
/// distinct (tenant, capsule) events flushed — the count of attempted
/// stamps, not a DB rowcount (the lance UPDATE reports an unreliable
/// rowcount; see [`Store::bump_last_used_at`]). Factored out of [`run`]
/// for testability.
pub async fn drain_once(
    store: &Store,
    rx: &mut UnboundedReceiver<CapsuleUsed>,
    now: &str,
) -> usize {
    let mut batch: HashSet<CapsuleUsed> = HashSet::new();
    while let Ok(ev) = rx.try_recv() {
        batch.insert(ev);
    }
    if batch.is_empty() {
        return 0;
    }
    // Group ids by tenant — one UPDATE per tenant.
    let mut by_tenant: HashMap<String, Vec<String>> = HashMap::new();
    for ev in batch {
        by_tenant
            .entry(ev.tenant)
            .or_default()
            .push(ev.capability_capsule_id);
    }
    let mut flushed = 0usize;
    for (tenant, ids) in by_tenant {
        let n = ids.len();
        match store.bump_last_used_at(&tenant, &ids, now).await {
            Ok(()) => flushed += n,
            Err(e) => warn!(error = %e, %tenant, "last_used bump failed"),
        }
    }
    flushed
}

/// Run the last-used worker loop: every `batch_interval_secs`, drain the
/// used-event channel and flush batched `last_used_at` stamps. Runs for
/// the process lifetime (like the other maintenance workers).
pub async fn run(
    store: Arc<Store>,
    mut rx: UnboundedReceiver<CapsuleUsed>,
    batch_interval_secs: u64,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(batch_interval_secs.max(1)));
    debug!(batch_interval_secs, "last_used worker started");
    loop {
        ticker.tick().await;
        let now = crate::storage::current_timestamp();
        let n = drain_once(&store, &mut rx, &now).await;
        if n > 0 {
            debug!(bumped = n, "last_used batch applied");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    };
    use tokio::sync::mpsc;

    fn capsule(id: &str, tenant: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.into(),
            tenant: tenant.into(),
            capability_capsule_type: CapabilityCapsuleType::Experience,
            status: CapabilityCapsuleStatus::Active,
            scope: Scope::Repo,
            visibility: Visibility::Shared,
            version: 1,
            summary: format!("summary-{id}"),
            content: format!("content-{id}"),
            content_hash: format!("hash-{id}"),
            source_agent: "test".into(),
            created_at: "00000000000000000001".into(),
            updated_at: "00000000000000000001".into(),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn burst_collapses_to_one_stamp_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("lu.lance")).await.unwrap();
        store
            .insert_capability_capsule(capsule("c1", "local"))
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<CapsuleUsed>();
        // 3-event burst for the same capsule + one for a missing id.
        for _ in 0..3 {
            tx.send(CapsuleUsed {
                tenant: "local".into(),
                capability_capsule_id: "c1".into(),
            })
            .unwrap();
        }
        tx.send(CapsuleUsed {
            tenant: "local".into(),
            capability_capsule_id: "ghost".into(),
        })
        .unwrap();
        drop(tx);

        let now = "00000000000000009999";
        let flushed = drain_once(&store, &mut rx, now).await;
        // The 3-event c1 burst coalesces to one; ghost stays distinct →
        // 2 distinct events flushed (attempted), one tenant batch.
        assert_eq!(flushed, 2, "burst coalesces; c1 + ghost are distinct");

        // The real signal: c1's clock was actually stamped on disk
        // (ghost matches no row and is silently a no-op).
        let row = store
            .get_capability_capsule_for_tenant("local", "c1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.last_used_at.as_deref(),
            Some(now),
            "last_used_at must be stamped to now"
        );
        // updated_at is untouched by a use-bump (it's the write clock).
        assert_eq!(row.updated_at, "00000000000000000001");
    }
}
