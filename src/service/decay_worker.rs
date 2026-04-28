use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::storage::DuckDbRepository;

const DECAY_INTERVAL_SECS: u64 = 3600;
const DECAY_RATE_PER_DAY: f32 = 0.01;

pub async fn start_decay_worker(repo: Arc<DuckDbRepository>) {
    loop {
        sleep(Duration::from_secs(DECAY_INTERVAL_SECS)).await;
        if let Err(e) = apply_time_decay(&repo).await {
            eprintln!("decay_worker error: {e}");
        }
    }
}

async fn apply_time_decay(repo: &DuckDbRepository) -> Result<(), Box<dyn std::error::Error>> {
    let conn = repo.conn()?;
    conn.execute(
        "update memories
         set decay_score = least(1.0, decay_score + ?1 * (julianday('now') - julianday(updated_at))),
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         where status = 'active' and decay_score < 1.0",
        [DECAY_RATE_PER_DAY],
    )?;
    Ok(())
}
