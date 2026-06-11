use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

use crate::storage::Backend;

const DECAY_INTERVAL_SECS: u64 = 3600;
const DECAY_RATE_PER_DAY: f64 = 0.01;
const MS_PER_DAY: f64 = 86_400_000.0;

pub async fn start_decay_worker(store: Arc<dyn Backend>) {
    loop {
        sleep(Duration::from_secs(DECAY_INTERVAL_SECS)).await;
        if let Err(e) = apply_time_decay(&*store).await {
            eprintln!("decay_worker error: {e}");
        }
    }
}

async fn apply_time_decay(store: &dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
    // `memories.updated_at` is a 20-digit zero-padded ms-since-epoch
    // string (see `storage::current_timestamp`). Same encoding here.
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let now_str = format!("{now_ms:020}");
    store
        .apply_time_decay(DECAY_RATE_PER_DAY, now_ms as f64, MS_PER_DAY, &now_str)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{CapabilityCapsuleRecord, CapabilityCapsuleStatus};
    use crate::storage::Store;
    use tempfile::tempdir;

    fn ms_string(ms: u128) -> String {
        format!("{ms:020}")
    }

    fn fixture(
        id: &str,
        status: CapabilityCapsuleStatus,
        decay: f32,
        updated_at: &str,
    ) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.into(),
            tenant: "t".into(),
            status,
            decay_score: decay,
            content_hash: "h".repeat(64),
            source_agent: "test".into(),
            created_at: updated_at.into(),
            updated_at: updated_at.into(),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn decay_advances_active_rows_and_skips_others() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("decay.lance");
        let store = Store::open(&db).await.unwrap();

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ten_days_ago = ms_string(now_ms - 10 * MS_PER_DAY as u128);

        store
            .insert_capability_capsule(fixture(
                "m-active",
                CapabilityCapsuleStatus::Active,
                0.0,
                &ten_days_ago,
            ))
            .await
            .unwrap();
        store
            .insert_capability_capsule(fixture(
                "m-prov",
                CapabilityCapsuleStatus::Provisional,
                0.0,
                &ten_days_ago,
            ))
            .await
            .unwrap();
        store
            .insert_capability_capsule(fixture(
                "m-sat",
                CapabilityCapsuleStatus::Active,
                1.0,
                &ten_days_ago,
            ))
            .await
            .unwrap();

        apply_time_decay(&store).await.expect("decay must run");

        let read = |id: &str| {
            let store = &store;
            let id = id.to_string();
            async move {
                store
                    .get_capability_capsule_for_tenant("t", &id)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };

        let m_active = read("m-active").await;
        // ~10 days * 0.01/day ≈ 0.1. last_used_at is NULL here, so the
        // decay clock falls back to updated_at (10 days ago).
        assert!(
            (0.05..=0.15).contains(&(m_active.decay_score as f64)),
            "active decay ≈ 0.1; got {}",
            m_active.decay_score
        );
        // O1: the sweep advances the decay clock via `last_used_at`, not
        // `updated_at`. `updated_at` is now the pure write clock and
        // stays put; `last_used_at` is stamped to the sweep's `now`.
        assert_eq!(m_active.updated_at, ten_days_ago);
        assert!(
            m_active.last_used_at.is_some(),
            "sweep must stamp last_used_at as the decay clock"
        );
        // Step-1 fix: the sweep stamps the *decay clock* (last_used_at) but
        // must NOT fabricate a *recall* signal. `last_recalled_at` is written
        // only on a real retrieval, so a never-recalled row stays None across
        // any number of hourly sweeps — this is the durable, sweep-proof
        // "was this ever recalled?" signal the idle-archive sweep relies on.
        assert!(
            m_active.last_recalled_at.is_none(),
            "decay sweep must not set last_recalled_at; got {:?}",
            m_active.last_recalled_at
        );

        let m_prov = read("m-prov").await;
        // Non-active row should not move (status filter).
        assert_eq!(m_prov.decay_score, 0.0);
        assert_eq!(m_prov.updated_at, ten_days_ago);

        let m_sat = read("m-sat").await;
        // Saturated row should not move (decay_score < 1.0 filter).
        assert_eq!(m_sat.decay_score, 1.0);
        assert_eq!(m_sat.updated_at, ten_days_ago);
    }

    /// O1 retrieval reinforcement: two same-age active rows decay
    /// differently when one has been *used* recently. The used row's
    /// `last_used_at` resets the decay clock, so the sweep accrues ~0
    /// for it while the untouched row accrues its full 10-day slice.
    #[tokio::test(flavor = "multi_thread")]
    async fn used_capsule_decays_slower_than_untouched() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("decay2.lance")).await.unwrap();

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ten_days_ago = ms_string(now_ms - 10 * MS_PER_DAY as u128);

        // Two active rows, both written 10 days ago, both decay 0.
        store
            .insert_capability_capsule(fixture(
                "m-used",
                CapabilityCapsuleStatus::Active,
                0.0,
                &ten_days_ago,
            ))
            .await
            .unwrap();
        store
            .insert_capability_capsule(fixture(
                "m-untouched",
                CapabilityCapsuleStatus::Active,
                0.0,
                &ten_days_ago,
            ))
            .await
            .unwrap();

        // Simulate a retrieval that emitted m-used: stamp last_used_at = now.
        let now_str = crate::storage::current_timestamp();
        store
            .bump_last_used_at("t", &["m-used".to_string()], &now_str)
            .await
            .unwrap();

        apply_time_decay(&store).await.expect("decay must run");

        let used = store
            .get_capability_capsule_for_tenant("t", "m-used")
            .await
            .unwrap()
            .unwrap();
        let untouched = store
            .get_capability_capsule_for_tenant("t", "m-untouched")
            .await
            .unwrap()
            .unwrap();

        // Recently-used → decay clock reset → accrues ~0. Untouched →
        // anchored on its 10-day-old updated_at → ~0.1.
        assert!(
            used.decay_score < 0.01,
            "used capsule barely decays; got {}",
            used.decay_score
        );
        assert!(
            (0.05..=0.15).contains(&(untouched.decay_score as f64)),
            "untouched ≈ 0.1; got {}",
            untouched.decay_score
        );
        assert!(
            used.decay_score < untouched.decay_score,
            "use must slow decay: used {} vs untouched {}",
            used.decay_score,
            untouched.decay_score
        );
        // Step-1 fix: a real recall stamped `last_recalled_at`, and it
        // SURVIVES the subsequent decay sweep — unlike `last_used_at`, the
        // sweep never overwrites it. The untouched row, never recalled, keeps
        // a None recall signal.
        assert!(
            used.last_recalled_at.is_some(),
            "recall must stamp last_recalled_at and survive the sweep"
        );
        assert!(
            untouched.last_recalled_at.is_none(),
            "never-recalled row must keep last_recalled_at = None"
        );
    }
}
