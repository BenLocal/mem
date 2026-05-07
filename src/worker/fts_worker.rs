//! Background worker that drains the FTS dirty flags out-of-band from
//! the read path.
//!
//! mem's memories + transcripts FTS (DuckDB native, BM25) is
//! non-incremental: every write that affects BM25 inputs flips a dirty
//! bit on `DuckDbRepository`, and the next BM25 read had to issue
//! `pragma drop_fts_index(…)` + `pragma create_fts_index(…)` to refresh.
//! On non-trivial tables that's hundreds of ms to several seconds of
//! synchronous work blocking `/memories/search` —— the dominant
//! latency contributor noted in `docs/api-data-flow.md §4.1`.
//!
//! This worker takes the rebuild off the read path: it ticks every
//! `MEM_FTS_REBUILD_INTERVAL_MS` (default 2000 ms) and rebuilds either
//! index if its dirty flag is set. Reads still call
//! `ensure_fts_index_fresh` as a fallback for the small window between
//! a write and the next worker tick (the swap is atomic, so worker and
//! reader can't double-rebuild).
//!
//! Trade-off: BM25 results may lag a fresh write by up to one tick
//! interval. The semantic (HNSW) channel of the RRF fusion is
//! unaffected — newly-ingested memories are still discoverable
//! immediately via the embedding worker, just without the BM25 boost.

use std::time::Duration;

use tracing::{info, warn};

use crate::storage::DuckDbRepository;

/// Run the FTS rebuild loop until the parent runtime drops it.
///
/// Spawned by `AppState::from_config`. The function is `async` so it
/// can `tokio::time::interval`-tick, but the actual rebuild work runs
/// synchronously inside the connection mutex (same as the read-path
/// fallback) — typical rebuilds are short enough that running them on
/// the runtime's worker thread is fine; if profiling shows otherwise
/// we'd wrap each call in `spawn_blocking`.
pub async fn run(repo: DuckDbRepository, interval_ms: u64) {
    let interval_ms = interval_ms.max(100);
    info!(interval_ms, "fts rebuild worker started");
    let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
    loop {
        interval.tick().await;
        if let Err(e) = repo.ensure_fts_index_fresh() {
            warn!(error = %e, "fts worker memories rebuild failed");
        }
        if let Err(e) = repo.ensure_transcript_fts_index_fresh() {
            warn!(error = %e, "fts worker transcripts rebuild failed");
        }
    }
}
