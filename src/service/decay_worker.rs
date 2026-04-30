use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

use duckdb::params;

use crate::storage::DuckDbRepository;

const DECAY_INTERVAL_SECS: u64 = 3600;
const DECAY_RATE_PER_DAY: f64 = 0.01;
const MS_PER_DAY: f64 = 86_400_000.0;

pub async fn start_decay_worker(repo: Arc<DuckDbRepository>) {
    loop {
        sleep(Duration::from_secs(DECAY_INTERVAL_SECS)).await;
        if let Err(e) = apply_time_decay(&repo).await {
            eprintln!("decay_worker error: {e}");
        }
    }
}

async fn apply_time_decay(repo: &DuckDbRepository) -> Result<(), Box<dyn std::error::Error>> {
    // `memories.updated_at` is a 20-digit zero-padded milliseconds-since-epoch string
    // (see `memory_service::current_timestamp`). Use the same encoding here.
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let now_str = format!("{now_ms:020}");

    let conn = repo.conn()?;
    conn.execute(
        "update memories
         set decay_score = least(1.0, decay_score + ?1 * ((?2 - updated_at::double) / ?3)),
             updated_at = ?4
         where status = 'active' and decay_score < 1.0",
        params![DECAY_RATE_PER_DAY, now_ms as f64, MS_PER_DAY, now_str],
    )?;
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

    #[tokio::test]
    async fn decay_advances_active_rows_and_skips_others() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("decay.duckdb");
        let repo = DuckDbRepository::open(&db).await.unwrap();

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ten_days_ago = ms_string(now_ms - 10 * MS_PER_DAY as u128);

        repo.insert_memory(fixture(
            "m-active",
            MemoryStatus::Active,
            0.0,
            &ten_days_ago,
        ))
        .await
        .unwrap();
        repo.insert_memory(fixture(
            "m-prov",
            MemoryStatus::Provisional,
            0.0,
            &ten_days_ago,
        ))
        .await
        .unwrap();
        repo.insert_memory(fixture("m-sat", MemoryStatus::Active, 1.0, &ten_days_ago))
            .await
            .unwrap();

        apply_time_decay(&repo).await.expect("decay sql must run");

        let conn = repo.conn().unwrap();
        let read = |id: &str| -> (f64, String) {
            conn.query_row(
                "select decay_score, updated_at from memories where memory_id = ?1",
                [id],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap()
        };

        let (active_score, active_updated) = read("m-active");
        // ~10 days * 0.01/day ≈ 0.1
        assert!(
            (0.05..=0.15).contains(&active_score),
            "active row decay should be ~0.1 after 10 days, got {active_score}"
        );
        assert_ne!(active_updated, ten_days_ago);

        let (prov_score, prov_updated) = read("m-prov");
        assert_eq!(prov_score, 0.0);
        assert_eq!(prov_updated, ten_days_ago);

        let (sat_score, sat_updated) = read("m-sat");
        assert_eq!(sat_score, 1.0);
        assert_eq!(sat_updated, ten_days_ago);
    }
}
