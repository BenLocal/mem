//! Online observability — lightweight in-process runtime counters.
//!
//! The eval framework (O6: `tests/golden_recall.rs` + `tests/mempalace_bench.rs`)
//! measures recall quality **offline**. These counters expose what the running
//! service is **doing** — redactions fired, near-duplicate proposals raised,
//! feedback by kind, ingest / search volume — so an operator can read live
//! behaviour off `GET /metrics` (JSON, like the other `*/stats` endpoints)
//! without attaching a debugger or grepping logs. They turn questions like
//! "is O5 redaction actually firing in prod?" / "is the O7(a) near-dup flag
//! over- or under-triggering?" / "what's the feedback-to-search ratio?" into a
//! single curl.
//!
//! Design: a `once_cell::Lazy` singleton of `std::sync::atomic` counters — zero
//! new dependencies, no `AppState` plumbing (choke points reach the registry
//! via [`metrics()`] directly). Counters are **process-local** — they reset on
//! restart and are NOT persisted (same lifetime semantics as the
//! `MEM_MAX_INGEST_PER_SESSION` throttle counter). Increments use `Relaxed`
//! ordering (these are counters, not synchronisation) and the read path
//! ([`Metrics::snapshot`]) is lock-free.

use crate::domain::capability_capsule::FeedbackKind;
use once_cell::sync::Lazy;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

static METRICS: Lazy<Metrics> = Lazy::new(Metrics::default);

/// The process-wide metrics registry. Call from any behaviour choke point.
pub fn metrics() -> &'static Metrics {
    &METRICS
}

/// Process-local runtime counters. Construct via `Metrics::default()` (the
/// global is created that way); tests build their own instance instead of
/// touching the shared global so counts stay isolated.
#[derive(Default)]
pub struct Metrics {
    ingest_total: AtomicU64,
    search_total: AtomicU64,
    redaction_hits: AtomicU64,
    neardup_flags: AtomicU64,
    feedback_useful: AtomicU64,
    feedback_applies_here: AtomicU64,
    feedback_outdated: AtomicU64,
    feedback_does_not_apply_here: AtomicU64,
    feedback_incorrect: AtomicU64,
    feedback_auto_promoted: AtomicU64,
}

impl Metrics {
    /// A new capsule row was persisted (idempotent re-ingests don't count —
    /// they never reach the insert).
    pub fn inc_ingest(&self) {
        self.ingest_total.fetch_add(1, Ordering::Relaxed);
    }

    /// A capsule `search` was served (includes the wake-up fast path — every
    /// recall that can emit a redacted banner).
    pub fn inc_search(&self) {
        self.search_total.fetch_add(1, Ordering::Relaxed);
    }

    /// An output text had at least one secret masked (O5). Counts
    /// texts-with-redactions, not individual secrets — the right granularity
    /// for a "is redaction firing" signal.
    pub fn inc_redaction_hit(&self) {
        self.redaction_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// A capsule was flipped to `PendingConfirmation` as a near-duplicate
    /// supersede proposal (O2 / O7(a)).
    pub fn inc_neardup_flag(&self) {
        self.neardup_flags.fetch_add(1, Ordering::Relaxed);
    }

    /// A feedback event was applied; routed to the per-kind counter. Typed on
    /// [`FeedbackKind`] so a new variant is a compile error here, not a silent
    /// miss.
    pub fn record_feedback(&self, kind: &FeedbackKind) {
        let counter = match kind {
            FeedbackKind::Useful => &self.feedback_useful,
            FeedbackKind::AppliesHere => &self.feedback_applies_here,
            FeedbackKind::Outdated => &self.feedback_outdated,
            FeedbackKind::DoesNotApplyHere => &self.feedback_does_not_apply_here,
            FeedbackKind::Incorrect => &self.feedback_incorrect,
            FeedbackKind::AutoPromoted => &self.feedback_auto_promoted,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A lock-free point-in-time read of every counter, ready to serialise.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        MetricsSnapshot {
            ingest_total: load(&self.ingest_total),
            search_total: load(&self.search_total),
            redaction_hits: load(&self.redaction_hits),
            neardup_flags: load(&self.neardup_flags),
            feedback_useful: load(&self.feedback_useful),
            feedback_applies_here: load(&self.feedback_applies_here),
            feedback_outdated: load(&self.feedback_outdated),
            feedback_does_not_apply_here: load(&self.feedback_does_not_apply_here),
            feedback_incorrect: load(&self.feedback_incorrect),
            feedback_auto_promoted: load(&self.feedback_auto_promoted),
        }
    }
}

/// Serialisable point-in-time snapshot of [`Metrics`]. Field names are the
/// JSON keys returned by `GET /metrics`.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub ingest_total: u64,
    pub search_total: u64,
    pub redaction_hits: u64,
    pub neardup_flags: u64,
    pub feedback_useful: u64,
    pub feedback_applies_here: u64,
    pub feedback_outdated: u64,
    pub feedback_does_not_apply_here: u64,
    pub feedback_incorrect: u64,
    pub feedback_auto_promoted: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero() {
        let m = Metrics::default();
        let s = m.snapshot();
        assert_eq!(s.ingest_total, 0);
        assert_eq!(s.search_total, 0);
        assert_eq!(s.redaction_hits, 0);
        assert_eq!(s.neardup_flags, 0);
        assert_eq!(s.feedback_useful, 0);
    }

    #[test]
    fn increments_accumulate() {
        let m = Metrics::default();
        m.inc_ingest();
        m.inc_ingest();
        m.inc_search();
        m.inc_redaction_hit();
        m.inc_neardup_flag();
        let s = m.snapshot();
        assert_eq!(s.ingest_total, 2);
        assert_eq!(s.search_total, 1);
        assert_eq!(s.redaction_hits, 1);
        assert_eq!(s.neardup_flags, 1);
    }

    #[test]
    fn feedback_routes_to_per_kind_counter() {
        let m = Metrics::default();
        m.record_feedback(&FeedbackKind::Useful);
        m.record_feedback(&FeedbackKind::Useful);
        m.record_feedback(&FeedbackKind::Incorrect);
        m.record_feedback(&FeedbackKind::AppliesHere);
        let s = m.snapshot();
        assert_eq!(s.feedback_useful, 2);
        assert_eq!(s.feedback_incorrect, 1);
        assert_eq!(s.feedback_applies_here, 1);
        assert_eq!(s.feedback_outdated, 0);
        assert_eq!(s.feedback_does_not_apply_here, 0);
        assert_eq!(s.feedback_auto_promoted, 0);
    }

    #[test]
    fn snapshot_serialises_to_flat_json() {
        let m = Metrics::default();
        m.inc_search();
        let json = serde_json::to_value(m.snapshot()).unwrap();
        assert_eq!(json["search_total"], 1);
        assert_eq!(json["ingest_total"], 0);
        // Per-kind feedback keys present.
        assert!(json.get("feedback_useful").is_some());
    }
}
