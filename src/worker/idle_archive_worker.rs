//! Idle-archive sweep — governance Step 2.
//!
//! Periodically archives `Active` capsules that have become dead weight:
//! the git-commit `experience` capsules that accumulate ~10/day, are never
//! recalled, never get feedback, and which nothing else ever prunes (decay
//! only down-ranks; auto_promote only *adds* to the active pool).
//!
//! A capsule is archived only when it fails to justify its shelf space on
//! EVERY axis at once:
//!   1. **never recalled since creation** — `last_recalled_at IS NULL`, the
//!      sweep-proof signal added in Step 1 (the decay sweep can't fake it);
//!   2. **aged out** — `created_at` older than `age_days`;
//!   3. **never positively reinforced** — `confidence <= default_confidence`
//!      (feedback only ever raises confidence, so equality = no `useful` /
//!      `applies_here` ever fired);
//!   4. **decayed** — `decay_score >= decay_threshold`;
//!   5. **structurally low-value** — `low_value_experience_reason` flags it
//!      (too short, or a bare single-line commit subject with no evidence /
//!      code_refs). This reuses the Step-3 ingest gate, and is the clause
//!      that keeps substantive lessons safe: a long / structured / ref-
//!      carrying experience is spared however idle it looks. Without it, a
//!      pool whose recall + feedback signals are blank retroactively (e.g.
//!      predating the `last_recalled_at` column) would see real memories
//!      archived purely for being old.
//!
//! Archival reuses the dedup path — `apply_feedback(FeedbackKind::Incorrect)`
//! — so the row is preserved **verbatim**; only search drops it. The action
//! is identical to a human clicking "archive" in the admin UI.
//!
//! **Default OFF** (see [`IdleArchiveSettings`]); opt in via
//! `MEM_IDLE_ARCHIVE_ENABLED=1`. A real (non-dry-run) sweep is a no-op while
//! disabled — only the dry-run *preview* runs regardless, so an operator can
//! inspect candidates before flipping the switch. Single-tenant per worker
//! instance, mirroring `dedup_worker` / `auto_promote_worker`.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::IdleArchiveSettings;
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, FeedbackKind,
};
use crate::storage::types::StorageError;
use crate::storage::{current_timestamp, Backend, CapsuleSearchStore, FeedbackEvent};

const MS_PER_DAY: u128 = 86_400_000;

/// Long-running loop. Returns immediately when `settings.enabled == false`
/// (the spawn guard in `app` also checks this; the redundant guard keeps a
/// future unconditional caller safe).
pub async fn run(store: Arc<dyn Backend>, settings: IdleArchiveSettings, tenant: String) {
    if !settings.enabled {
        return;
    }
    let interval = Duration::from_secs(settings.interval_secs);
    info!(
        interval_secs = settings.interval_secs,
        age_days = settings.age_days,
        decay_threshold = settings.decay_threshold,
        default_confidence = settings.default_confidence,
        scan_limit = settings.scan_limit,
        tenant = %tenant,
        "idle_archive_worker started",
    );
    loop {
        sleep(interval).await;
        match sweep_once(&*store, &settings, &tenant, /* dry_run */ false).await {
            Ok(archived) => {
                if !archived.is_empty() {
                    info!(
                        count = archived.len(),
                        tenant = %tenant,
                        "idle_archive: archived {} idle capsule(s)",
                        archived.len(),
                    );
                }
            }
            Err(e) => warn!(error = %e, tenant = %tenant, "idle_archive sweep failed"),
        }
    }
}

/// One sweep pass. Returns the ids that were (or, when `dry_run`, *would
/// be*) archived. Extracted from [`run`] so tests + the HTTP preview route
/// drive the same logic.
///
/// Safety gate: a real run (`dry_run=false`) while the worker is disabled
/// archives nothing — only `dry_run=true` previews run regardless of the
/// master switch.
pub async fn sweep_once(
    store: &dyn Backend,
    settings: &IdleArchiveSettings,
    tenant: &str,
    dry_run: bool,
) -> Result<Vec<String>, StorageError> {
    if !dry_run && !settings.enabled {
        return Ok(Vec::new());
    }

    let ids = store.list_capability_capsule_ids_for_tenant(tenant).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let id_slice: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .take(settings.scan_limit)
        .collect();
    let capsules =
        CapsuleSearchStore::fetch_capability_capsules_by_ids(store, tenant, &id_slice).await?;

    let now = current_timestamp();
    let now_ms = parse_ms(&now);
    // Saturating: if the clock is somehow behind a row's created_at, the
    // cutoff floors at 0 and that row simply isn't "old enough".
    let cutoff_ms = now_ms.saturating_sub(settings.age_days as u128 * MS_PER_DAY);

    let candidates: Vec<CapabilityCapsuleRecord> = capsules
        .into_iter()
        .filter(|c| is_idle_candidate(c, cutoff_ms, settings))
        .collect();

    let mut archived = Vec::with_capacity(candidates.len());
    for capsule in &candidates {
        let id = capsule.capability_capsule_id.clone();
        if dry_run {
            archived.push(id);
            continue;
        }
        let event = FeedbackEvent {
            feedback_id: format!("fb_{}", uuid::Uuid::now_v7()),
            capability_capsule_id: id.clone(),
            feedback_kind: FeedbackKind::Incorrect.as_str().to_string(),
            created_at: now.clone(),
            note: Some(format!(
                "idle_archive: never recalled, age ≥ {}d, confidence ≤ {}, decay ≥ {}, \
                 structurally low-value (content < {} chars or bare commit subject)",
                settings.age_days,
                settings.default_confidence,
                settings.decay_threshold,
                settings.min_content_len,
            )),
        };
        match store.apply_feedback(capsule, event).await {
            Ok(_) => archived.push(id),
            Err(e) => warn!(
                capability_capsule_id = %id,
                error = %e,
                "idle_archive: archive failed, continuing",
            ),
        }
    }
    Ok(archived)
}

/// True when `c` fails every justification clause at once (see module doc).
/// Pure + total so it is trivially unit-testable without a store.
fn is_idle_candidate(
    c: &CapabilityCapsuleRecord,
    cutoff_ms: u128,
    settings: &IdleArchiveSettings,
) -> bool {
    // Tiny epsilon so float round-trips (0.6 stored as f32) still read as
    // "at the default" rather than slipping just above it.
    const EPS: f32 = 1e-6;
    c.status == CapabilityCapsuleStatus::Active
        && c.last_recalled_at.is_none()
        && parse_ms(&c.created_at) <= cutoff_ms
        && c.confidence <= settings.default_confidence + EPS
        && c.decay_score >= settings.decay_threshold
        // Structural-junk gate (clause 5): reuse the Step-3 ingest logic so
        // only an experience that is ALSO structurally low-value (too short
        // / bare commit subject) can be archived. A long, structured, or
        // reference-carrying lesson returns None here and is spared no
        // matter how idle — this is the clause that stops the sweep from
        // deleting substantive memories whose recall/feedback signals are
        // merely blank (e.g. a pool predating `last_recalled_at`).
        && crate::pipeline::ingest::low_value_experience_reason(
            &c.capability_capsule_type,
            &c.content,
            &c.evidence,
            &c.code_refs,
            settings.min_content_len,
        )
        .is_some()
}

/// Parse a 20-digit zero-padded ms-since-epoch string. A malformed value
/// reads as 0 (epoch) — for `created_at` that makes the row look ancient
/// (archivable on the age axis), but the other three clauses still guard it.
fn parse_ms(s: &str) -> u128 {
    s.trim_start_matches('0').parse::<u128>().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule(id: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.into(),
            capability_capsule_type:
                crate::domain::capability_capsule::CapabilityCapsuleType::Experience,
            status: CapabilityCapsuleStatus::Active,
            confidence: 0.6,
            decay_score: 0.6,
            content: "short".into(), // structurally low-value (< min_content_len)
            created_at: "00000000000000001000".into(), // small => old
            last_recalled_at: None,
            expires_at: None,
            ..Default::default()
        }
    }

    fn settings() -> IdleArchiveSettings {
        IdleArchiveSettings::development_defaults()
    }

    #[test]
    fn parse_ms_handles_zero_padding_and_garbage() {
        assert_eq!(parse_ms("00000000000000001000"), 1000);
        assert_eq!(parse_ms("00000001781106546457"), 1781106546457);
        assert_eq!(parse_ms("not-a-number"), 0);
        assert_eq!(parse_ms("00000000000000000000"), 0);
    }

    #[test]
    fn idle_candidate_requires_all_clauses() {
        let cutoff = 100_000_u128; // created_at 1000 < cutoff => old enough
        assert!(is_idle_candidate(&capsule("c"), cutoff, &settings()));

        // Recalled → spared.
        let mut recalled = capsule("c");
        recalled.last_recalled_at = Some("00000000000000002000".into());
        assert!(!is_idle_candidate(&recalled, cutoff, &settings()));

        // Too young (created_at above cutoff).
        let young = capsule("c");
        assert!(!is_idle_candidate(&young, 500, &settings()));

        // Reinforced (confidence above default).
        let mut reinforced = capsule("c");
        reinforced.confidence = 0.8;
        assert!(!is_idle_candidate(&reinforced, cutoff, &settings()));

        // Decay below threshold.
        let mut fresh = capsule("c");
        fresh.decay_score = 0.1;
        assert!(!is_idle_candidate(&fresh, cutoff, &settings()));

        // Non-active never qualifies.
        let mut archived = capsule("c");
        archived.status = CapabilityCapsuleStatus::Archived;
        assert!(!is_idle_candidate(&archived, cutoff, &settings()));

        // Clause 5 — a substantive lesson is spared even when idle on every
        // other axis. A long multi-line body returns None from the
        // structural test, so it is NOT a candidate.
        let mut substantive = capsule("c");
        substantive.content = format!("Symptom: X broke.\n{}", "detail ".repeat(40));
        assert!(
            !is_idle_candidate(&substantive, cutoff, &settings()),
            "a long, structured experience must be spared no matter how idle"
        );

        // And a single-line capsule that clears the length floor but
        // carries a code_ref is spared (the ref defeats the bare-title rule).
        let mut with_ref = capsule("c");
        with_ref.content = "a".repeat(50); // single line, ≥ min_content_len
        with_ref.code_refs = vec!["src/x.rs:1".to_string()];
        assert!(!is_idle_candidate(&with_ref, cutoff, &settings()));
    }
}
