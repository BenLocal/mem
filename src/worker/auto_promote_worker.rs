//! Auto-promote sweep. Periodically scans `PendingConfirmation`
//! capsules whose `updated_at` is older than `age_days` and promotes
//! eligible types to `Active`, writing an audit row to
//! `feedback_events` with `feedback_kind=auto_promoted`.
//!
//! Spawned by `app::AppState::from_config` when
//! `config.auto_promote.enabled` is true; **default ON**, opt out
//! via `MEM_AUTO_PROMOTE_DISABLED=1`.
//!
//! **Tenant model.** The MVP sweeps a single tenant
//! (`MEM_TENANT` or `"local"`). A future iteration that needs
//! per-tenant promotion can either iterate distinct tenants from
//! `capability_capsules` per tick or expose `MEM_AUTO_PROMOTE_TENANTS`
//! as a CSV.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::AutoPromoteSettings;
use crate::domain::capability_capsule::{CapabilityCapsuleRecord, FeedbackKind};
use crate::storage::types::StorageError;
use crate::storage::{current_timestamp, Backend, FeedbackEvent};

/// Long-running loop. `tenant` is the namespace the sweep operates on
/// — for the local single-tenant setup this is just `"local"`.
pub async fn run(store: Arc<dyn Backend>, settings: AutoPromoteSettings, tenant: String) {
    if !settings.enabled {
        return;
    }
    let interval = Duration::from_secs(settings.interval_secs);
    info!(
        age_days = settings.age_days,
        interval_secs = settings.interval_secs,
        decay_threshold = settings.decay_threshold,
        types = ?settings.types,
        tenant = %tenant,
        "auto_promote_worker started",
    );
    loop {
        sleep(interval).await;
        match sweep_once(&*store, &settings, &tenant, /* dry_run */ false).await {
            Ok(promoted) => {
                if !promoted.is_empty() {
                    info!(
                        count = promoted.len(),
                        tenant = %tenant,
                        "auto_promote: promoted {} capsule(s)",
                        promoted.len(),
                    );
                }
            }
            Err(e) => warn!(error = %e, tenant = %tenant, "auto_promote sweep failed"),
        }
    }
}

/// One sweep pass. Returns the ids of capsules promoted (or that
/// *would* be promoted, when `dry_run=true`).
///
/// `dry_run=true` skips the `apply_feedback` write entirely — the
/// only side effect is a candidate query. Callers (the HTTP
/// endpoint) use this to preview "what would tomorrow's tick touch."
pub async fn sweep_once(
    store: &dyn Backend,
    settings: &AutoPromoteSettings,
    tenant: &str,
    dry_run: bool,
) -> Result<Vec<String>, StorageError> {
    let cutoff = cutoff_timestamp(settings.age_days);
    let candidates = store
        .auto_promote_candidates(tenant, &cutoff, &settings.types, settings.decay_threshold)
        .await?;

    if dry_run {
        return Ok(candidates
            .into_iter()
            .map(|c| c.capability_capsule_id)
            .collect());
    }

    let mut promoted = Vec::with_capacity(candidates.len());
    for capsule in candidates {
        match promote_one(store, &capsule).await {
            Ok(()) => promoted.push(capsule.capability_capsule_id),
            Err(e) => warn!(
                capability_capsule_id = %capsule.capability_capsule_id,
                error = %e,
                "auto_promote: row failed, continuing",
            ),
        }
    }
    Ok(promoted)
}

async fn promote_one(
    store: &dyn Backend,
    capsule: &CapabilityCapsuleRecord,
) -> Result<(), StorageError> {
    let event = FeedbackEvent {
        feedback_id: format!("fb_{}", uuid::Uuid::now_v7()),
        capability_capsule_id: capsule.capability_capsule_id.clone(),
        feedback_kind: FeedbackKind::AutoPromoted.as_str().to_string(),
        created_at: current_timestamp(),
        note: Some(format!(
            "auto-promote: idle since {}",
            capsule.updated_at.trim_start_matches('0')
        )),
    };
    store.apply_feedback(capsule, event).await?;
    Ok(())
}

/// 20-digit ms timestamp `age_days` in the past. Capsules with
/// `updated_at` *strictly less than* this value qualify.
fn cutoff_timestamp(age_days: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let age_ms = (age_days as u128).saturating_mul(86_400_000);
    let cutoff = now_ms.saturating_sub(age_ms);
    format!("{cutoff:020}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{CapabilityCapsuleStatus, CapabilityCapsuleType};
    use crate::storage::Store;
    use tempfile::tempdir;

    fn ms_string(ms: u128) -> String {
        format!("{ms:020}")
    }

    fn fixture(
        id: &str,
        capability_capsule_type: CapabilityCapsuleType,
        status: CapabilityCapsuleStatus,
        updated_at: &str,
        decay_score: f32,
    ) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.into(),
            tenant: "t".into(),
            capability_capsule_type,
            status,
            decay_score,
            content_hash: "h".repeat(64),
            source_agent: "test".into(),
            created_at: updated_at.into(),
            updated_at: updated_at.into(),
            ..Default::default()
        }
    }

    fn default_settings() -> AutoPromoteSettings {
        AutoPromoteSettings {
            enabled: true,
            age_days: 7,
            interval_secs: 3600,
            types: vec![
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleType::Implementation,
            ],
            decay_threshold: 0.5,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_promotes_old_pending_experience() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("p.lance")).await.unwrap();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ten_days_ago = ms_string(now_ms - 10 * 86_400_000);

        // Eligible: old + experience + low decay + pending
        store
            .insert_capability_capsule(fixture(
                "eligible",
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleStatus::PendingConfirmation,
                &ten_days_ago,
                0.0,
            ))
            .await
            .unwrap();
        // Too young
        let one_day_ago = ms_string(now_ms - 86_400_000);
        store
            .insert_capability_capsule(fixture(
                "too-young",
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleStatus::PendingConfirmation,
                &one_day_ago,
                0.0,
            ))
            .await
            .unwrap();
        // Wrong type — Preference is excluded by default
        store
            .insert_capability_capsule(fixture(
                "wrong-type",
                CapabilityCapsuleType::Preference,
                CapabilityCapsuleStatus::PendingConfirmation,
                &ten_days_ago,
                0.0,
            ))
            .await
            .unwrap();
        // Already active — not in candidate set
        store
            .insert_capability_capsule(fixture(
                "already-active",
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleStatus::Active,
                &ten_days_ago,
                0.0,
            ))
            .await
            .unwrap();
        // Decay too high — was marked outdated; don't auto-promote
        store
            .insert_capability_capsule(fixture(
                "decayed",
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleStatus::PendingConfirmation,
                &ten_days_ago,
                0.6,
            ))
            .await
            .unwrap();

        let promoted = sweep_once(&store, &default_settings(), "t", false)
            .await
            .unwrap();
        assert_eq!(promoted, vec!["eligible".to_string()]);

        // Verify the actual status transition.
        let row = store
            .get_capability_capsule_for_tenant("t", "eligible")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, CapabilityCapsuleStatus::Active);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_dry_run_does_not_write() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("p.lance")).await.unwrap();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ten_days_ago = ms_string(now_ms - 10 * 86_400_000);

        store
            .insert_capability_capsule(fixture(
                "candidate",
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleStatus::PendingConfirmation,
                &ten_days_ago,
                0.0,
            ))
            .await
            .unwrap();

        let preview = sweep_once(&store, &default_settings(), "t", true)
            .await
            .unwrap();
        assert_eq!(preview, vec!["candidate".to_string()]);

        // Status must still be pending after a dry run.
        let row = store
            .get_capability_capsule_for_tenant("t", "candidate")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, CapabilityCapsuleStatus::PendingConfirmation);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_empty_types_promotes_nothing() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("p.lance")).await.unwrap();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ten_days_ago = ms_string(now_ms - 10 * 86_400_000);
        store
            .insert_capability_capsule(fixture(
                "would-be-eligible",
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleStatus::PendingConfirmation,
                &ten_days_ago,
                0.0,
            ))
            .await
            .unwrap();

        let mut settings = default_settings();
        settings.types.clear();
        let promoted = sweep_once(&store, &settings, "t", false).await.unwrap();
        assert!(promoted.is_empty());
    }
}
