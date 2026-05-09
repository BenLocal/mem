use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

use crate::storage::Store;

const DECAY_INTERVAL_SECS: u64 = 3600;
const DECAY_RATE_PER_DAY: f64 = 0.01;
const MS_PER_DAY: f64 = 86_400_000.0;

pub async fn start_decay_worker(store: Arc<Store>) {
    loop {
        sleep(Duration::from_secs(DECAY_INTERVAL_SECS)).await;
        if let Err(e) = apply_time_decay(&store).await {
            eprintln!("decay_worker error: {e}");
        }
    }
}

async fn apply_time_decay(store: &Store) -> Result<(), Box<dyn std::error::Error>> {
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
    use crate::domain::memory::{MemoryRecord, MemoryStatus};
    use tempfile::tempdir;

    fn ms_string(ms: u128) -> String {
        format!("{ms:020}")
    }

    fn fixture(id: &str, status: MemoryStatus, decay: f32, updated_at: &str) -> MemoryRecord {
        MemoryRecord {
            memory_id: id.into(),
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
            .insert_memory(fixture(
                "m-active",
                MemoryStatus::Active,
                0.0,
                &ten_days_ago,
            ))
            .await
            .unwrap();
        store
            .insert_memory(fixture(
                "m-prov",
                MemoryStatus::Provisional,
                0.0,
                &ten_days_ago,
            ))
            .await
            .unwrap();
        store
            .insert_memory(fixture("m-sat", MemoryStatus::Active, 1.0, &ten_days_ago))
            .await
            .unwrap();

        apply_time_decay(&store).await.expect("decay must run");

        let read = |id: &str| {
            let store = &store;
            let id = id.to_string();
            async move {
                store
                    .get_memory_for_tenant("t", &id)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };

        let m_active = read("m-active").await;
        // ~10 days * 0.01/day ≈ 0.1
        assert!(
            (0.05..=0.15).contains(&(m_active.decay_score as f64)),
            "active decay ≈ 0.1; got {}",
            m_active.decay_score
        );
        assert_ne!(m_active.updated_at, ten_days_ago);

        let m_prov = read("m-prov").await;
        // Non-active row should not move (status filter).
        assert_eq!(m_prov.decay_score, 0.0);
        assert_eq!(m_prov.updated_at, ten_days_ago);

        let m_sat = read("m-sat").await;
        // Saturated row should not move (decay_score < 1.0 filter).
        assert_eq!(m_sat.decay_score, 1.0);
        assert_eq!(m_sat.updated_at, ten_days_ago);
    }
}
