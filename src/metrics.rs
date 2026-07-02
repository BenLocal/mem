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
//! **Naming is scope-explicit on purpose.** mem runs two parallel pipelines —
//! capsules and the verbatim transcript archive — plus episodes. Each ingest /
//! search counter carries the pipeline as a prefix (`capsule_*` / `transcript_*`
//! / `episode_*`) so a reader never mistakes a capsule-only count for total
//! volume. (`redaction_hits` is intentionally a single cross-surface counter:
//! it fires from every `redact_secrets` call — capsule compress + both
//! embedding workers + transcript search — and the point is "is redaction
//! firing at all".)
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
    capsule_ingest_total: AtomicU64,
    capsule_search_total: AtomicU64,
    transcript_ingest_total: AtomicU64,
    transcript_search_total: AtomicU64,
    episode_ingest_total: AtomicU64,
    redaction_hits: AtomicU64,
    neardup_flags: AtomicU64,
    kg_auto_invalidated: AtomicU64,
    feedback_useful: AtomicU64,
    feedback_applies_here: AtomicU64,
    feedback_outdated: AtomicU64,
    feedback_does_not_apply_here: AtomicU64,
    feedback_incorrect: AtomicU64,
    feedback_auto_promoted: AtomicU64,
    feedback_system_reweight_up: AtomicU64,
    feedback_system_reweight_decay: AtomicU64,
}

impl Metrics {
    /// A new capsule row was persisted (idempotent re-ingests don't count —
    /// they never reach the insert). Used by both the single and batch path.
    pub fn inc_capsule_ingest(&self) {
        self.capsule_ingest_total.fetch_add(1, Ordering::Relaxed);
    }

    /// A capsule `search` was served (includes the wake-up fast path — every
    /// recall that can emit a redacted banner).
    pub fn inc_capsule_search(&self) {
        self.capsule_search_total.fetch_add(1, Ordering::Relaxed);
    }

    /// `n` transcript messages were persisted. The batch path passes the landed
    /// row count; the single path passes 1.
    pub fn add_transcript_ingest(&self, n: u64) {
        self.transcript_ingest_total.fetch_add(n, Ordering::Relaxed);
    }

    /// A transcript `search` was served (the `transcripts_search` path — a
    /// separate pipeline from capsule search, hence its own counter).
    pub fn inc_transcript_search(&self) {
        self.transcript_search_total.fetch_add(1, Ordering::Relaxed);
    }

    /// An episode row was persisted. (The workflow capsule an episode may spawn
    /// is counted separately under `capsule_ingest_total` via the ingest path.)
    pub fn inc_episode_ingest(&self) {
        self.episode_ingest_total.fetch_add(1, Ordering::Relaxed);
    }

    /// An output text had at least one secret masked (O5). Cross-surface: fires
    /// from capsule compress, both embedding workers, and transcript search.
    /// Counts texts-with-redactions, not individual secrets — the right
    /// granularity for a "is redaction firing" signal.
    pub fn inc_redaction_hit(&self) {
        self.redaction_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// A capsule was flipped to `PendingConfirmation` as a near-duplicate
    /// supersede proposal (O2 / O7(a)).
    pub fn inc_neardup_flag(&self) {
        self.neardup_flags.fetch_add(1, Ordering::Relaxed);
    }

    /// `n` stale graph edges were auto-closed by the G4 functional-predicate
    /// contradiction sweep (asserting a new single-valued fact superseded
    /// conflicting old ones).
    pub fn add_kg_auto_invalidated(&self, n: u64) {
        self.kg_auto_invalidated.fetch_add(n, Ordering::Relaxed);
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
            FeedbackKind::SystemReweightUp => &self.feedback_system_reweight_up,
            FeedbackKind::SystemReweightDecay => &self.feedback_system_reweight_decay,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A lock-free point-in-time read of every counter, ready to serialise.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        MetricsSnapshot {
            capsule_ingest_total: load(&self.capsule_ingest_total),
            capsule_search_total: load(&self.capsule_search_total),
            transcript_ingest_total: load(&self.transcript_ingest_total),
            transcript_search_total: load(&self.transcript_search_total),
            episode_ingest_total: load(&self.episode_ingest_total),
            redaction_hits: load(&self.redaction_hits),
            neardup_flags: load(&self.neardup_flags),
            kg_auto_invalidated: load(&self.kg_auto_invalidated),
            feedback_useful: load(&self.feedback_useful),
            feedback_applies_here: load(&self.feedback_applies_here),
            feedback_outdated: load(&self.feedback_outdated),
            feedback_does_not_apply_here: load(&self.feedback_does_not_apply_here),
            feedback_incorrect: load(&self.feedback_incorrect),
            feedback_auto_promoted: load(&self.feedback_auto_promoted),
            feedback_system_reweight_up: load(&self.feedback_system_reweight_up),
            feedback_system_reweight_decay: load(&self.feedback_system_reweight_decay),
        }
    }
}

/// Serialisable point-in-time snapshot of [`Metrics`]. Field names are the
/// JSON keys returned by `GET /metrics`.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub capsule_ingest_total: u64,
    pub capsule_search_total: u64,
    pub transcript_ingest_total: u64,
    pub transcript_search_total: u64,
    pub episode_ingest_total: u64,
    pub redaction_hits: u64,
    pub neardup_flags: u64,
    pub kg_auto_invalidated: u64,
    pub feedback_useful: u64,
    pub feedback_applies_here: u64,
    pub feedback_outdated: u64,
    pub feedback_does_not_apply_here: u64,
    pub feedback_incorrect: u64,
    pub feedback_auto_promoted: u64,
    pub feedback_system_reweight_up: u64,
    pub feedback_system_reweight_decay: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero() {
        let m = Metrics::default();
        let s = m.snapshot();
        assert_eq!(s.capsule_ingest_total, 0);
        assert_eq!(s.capsule_search_total, 0);
        assert_eq!(s.transcript_ingest_total, 0);
        assert_eq!(s.transcript_search_total, 0);
        assert_eq!(s.episode_ingest_total, 0);
        assert_eq!(s.redaction_hits, 0);
        assert_eq!(s.neardup_flags, 0);
        assert_eq!(s.feedback_useful, 0);
    }

    #[test]
    fn increments_accumulate_per_pipeline() {
        let m = Metrics::default();
        m.inc_capsule_ingest();
        m.inc_capsule_ingest();
        m.inc_capsule_search();
        m.add_transcript_ingest(5);
        m.inc_transcript_search();
        m.inc_episode_ingest();
        m.inc_redaction_hit();
        m.inc_neardup_flag();
        m.add_kg_auto_invalidated(3);
        let s = m.snapshot();
        assert_eq!(s.capsule_ingest_total, 2);
        assert_eq!(s.capsule_search_total, 1);
        // Pipelines are counted independently — no cross-contamination.
        assert_eq!(s.transcript_ingest_total, 5);
        assert_eq!(s.transcript_search_total, 1);
        assert_eq!(s.episode_ingest_total, 1);
        assert_eq!(s.redaction_hits, 1);
        assert_eq!(s.neardup_flags, 1);
        assert_eq!(s.kg_auto_invalidated, 3);
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
    fn snapshot_serialises_with_pipeline_prefixed_keys() {
        let m = Metrics::default();
        m.inc_capsule_search();
        m.inc_transcript_search();
        let json = serde_json::to_value(m.snapshot()).unwrap();
        assert_eq!(json["capsule_search_total"], 1);
        assert_eq!(json["transcript_search_total"], 1);
        assert_eq!(json["capsule_ingest_total"], 0);
        // Per-kind feedback keys present.
        assert!(json.get("feedback_useful").is_some());
    }
}
