//! K9 potentiation worker (strategy B — in-memory channel).
//!
//! Retrieve enqueues graph-edge co-access events to an unbounded channel
//! (a cheap, non-blocking `send` — no DB write on the read path); this
//! worker drains the channel on an interval, dedups bursts, and applies
//! one Hebbian potentiation per unique edge via [`Store::potentiate_edge`]
//! — off the read path, so retrieves never write. Default OFF
//! (`MEM_EDGE_DYNAMICS_ENABLED`). mempalace `dynamics.py` analogue.
//!
//! Events are best-effort: a process restart drops whatever was queued.
//! That's acceptable — potentiation is a statistical salience signal,
//! not exact accounting, and an edge that keeps being co-accessed will
//! be re-enqueued on the next retrieve.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, warn};

use crate::config::EdgeDynamicsSettings;
use crate::storage::Store;

/// A co-access event: the identity of an active edge that retrieve
/// surfaced. Deduped within a drain window so a burst of the same edge
/// collapses to a single potentiation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EdgeAccess {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
}

/// Drain everything currently queued, dedup, and potentiate each unique
/// edge once at `now`. Returns the number of edges actually potentiated
/// (vanished edges are dropped). Factored out of [`run`] for testability.
pub async fn drain_once(store: &Store, rx: &mut UnboundedReceiver<EdgeAccess>, now: &str) -> usize {
    let mut batch: HashSet<EdgeAccess> = HashSet::new();
    while let Ok(ev) = rx.try_recv() {
        batch.insert(ev);
    }
    let mut potentiated = 0usize;
    for ev in batch {
        match store
            .potentiate_edge(&ev.from_node_id, &ev.to_node_id, &ev.relation, now)
            .await
        {
            Ok(true) => potentiated += 1,
            Ok(false) => {} // edge closed/gone between access and drain — drop
            Err(e) => warn!(
                error = %e,
                from = %ev.from_node_id,
                to = %ev.to_node_id,
                relation = %ev.relation,
                "edge potentiation failed",
            ),
        }
    }
    potentiated
}

/// Run the potentiation worker loop: every `batch_interval_secs`, drain
/// the access-event channel and apply batched potentiations. Runs for
/// the process lifetime (like the other maintenance workers).
pub async fn run(
    store: Arc<Store>,
    mut rx: UnboundedReceiver<EdgeAccess>,
    settings: EdgeDynamicsSettings,
) {
    let mut ticker =
        tokio::time::interval(Duration::from_secs(settings.batch_interval_secs.max(1)));
    debug!(
        batch_interval_secs = settings.batch_interval_secs,
        "potentiation worker started"
    );
    loop {
        ticker.tick().await;
        let now = crate::storage::current_timestamp();
        let n = drain_once(&store, &mut rx, &now).await;
        if n > 0 {
            debug!(potentiated = n, "edge potentiation batch applied");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::GraphEdge;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    #[tokio::test(flavor = "multi_thread")]
    async fn drain_once_potentiates_queued_edges_and_dedups_bursts() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("pw.lance")).await.unwrap();
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "entity:a".into(),
                to_node_id: "entity:b".into(),
                relation: "rel".into(),
                valid_from: "00000001780000000000".into(),
                valid_to: None,
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<EdgeAccess>();
        // A burst of 3 accesses to the same edge within one drain window.
        for _ in 0..3 {
            tx.send(EdgeAccess {
                from_node_id: "entity:a".into(),
                to_node_id: "entity:b".into(),
                relation: "rel".into(),
            })
            .unwrap();
        }
        // An access to a non-existent edge — must be dropped, not error.
        tx.send(EdgeAccess {
            from_node_id: "entity:x".into(),
            to_node_id: "entity:y".into(),
            relation: "rel".into(),
        })
        .unwrap();

        let n = drain_once(&store, &mut rx, "00000001780007200000").await;
        assert_eq!(
            n, 1,
            "the 3-access burst collapses to one potentiation; the phantom edge is dropped"
        );

        let edges = store.neighbors_within("entity:a", 1, None).await.unwrap();
        let e = edges
            .iter()
            .find(|e| e.from_node_id == "entity:a")
            .expect("edge present");
        // One potentiation → strength 1.05 and access_count 1 (NOT 3 — the
        // burst deduped, which is also the Cepeda anti-massing effect).
        assert!(
            (e.strength.unwrap() - 1.05).abs() < 1e-6,
            "{:?}",
            e.strength
        );
        assert_eq!(e.access_count, Some(1));
    }
}
