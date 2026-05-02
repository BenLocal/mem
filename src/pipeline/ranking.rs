//! Pure ranking helpers shared by the memories and transcripts retrieval
//! pipelines. No pipeline-specific types — only rank/timestamp arithmetic.
//! Adding code here is a deliberate decision to share math; do NOT add types
//! that name memory or transcript domain concepts.

/// Reciprocal Rank Fusion constant. Tuned for the "rank 1 hit ≈ 16" baseline
/// the memories pipeline already encodes; keep memories and transcripts in
/// lock-step so RRF magnitudes are comparable across pipelines if anyone
/// later builds a unified analytics view.
pub const RRF_K: usize = 60;

/// Multiplier applied to the raw `1 / (K + rank)` value before rounding to
/// `i64`. Combined with `RRF_K = 60`, a rank-1 hit yields:
///   `(1.0 / 61.0) * 1000.0 ≈ 16.39 → 16`.
pub const RRF_SCALE: f64 = 1000.0;

/// RRF contribution for a single appearance at the given rank in one channel
/// (lexical or semantic). Returns the same `i64` value the memories pipeline
/// has been using since the BM25 hybrid retrieval landed; transcripts share
/// the formula so RRF magnitudes are directly comparable.
///
/// **Asymmetry with `pipeline::retrieve::score_candidates_hybrid_rrf`**:
/// memories' inline formula sums per-channel `f64` contributions before
/// a single `.round()`, while this helper rounds per-channel before
/// summing. For a candidate that hits both lex and sem at rank 1:
/// memories computes `((1.0/61.0 + 1.0/61.0) * 1000.0).round() = 33`,
/// while `rrf_contribution(1) + rrf_contribution(1) = 16 + 16 = 32`.
/// The difference is bounded at ±1 in mixed-rank scenarios.
///
/// Memories preserves sum-then-round to keep its existing test scores
/// stable (the integer values are pinned in tests like
/// `rrf_both_paths_top_rank` at `pipeline::retrieve`). Transcripts use
/// this helper directly and accept round-then-sum; do NOT migrate
/// memories to call this helper without re-baselining
/// `pipeline::retrieve::tests::rrf_*`.
pub fn rrf_contribution(rank: usize) -> i64 {
    ((RRF_SCALE / (RRF_K as f64 + rank as f64)).round()) as i64
}

/// Freshness curve: how much to credit a candidate based on how close its
/// timestamp is to the newest in the candidate pool. Range `[-14, 6]`:
/// returns `6` when `current >= newest` (i.e., this candidate is the newest
/// or in the future), then decays linearly in 10_000-ms buckets, saturating
/// at `-14` after 200 s of staleness.
///
/// The curve is intentionally tight (not a long-tail decay) — this signal
/// only acts as a tiebreaker behind RRF and lifecycle bonuses, not as a
/// dominant ranking factor.
pub fn freshness_score(newest: u128, current: u128) -> i64 {
    if newest <= current {
        return 6;
    }

    let delta = newest - current;
    let bucket = (delta / 10_000).min(20);
    6 - bucket as i64
}

/// Parse a timestamp string into a `u128` of milliseconds since epoch.
/// The codebase encodes timestamps as zero-padded 20-digit decimal strings
/// produced by `crate::storage::time::current_timestamp`. This helper
/// strips non-digit characters defensively (handles RFC-3339 sloppiness)
/// and returns `0` on parse failure (caller treats as "very old").
pub fn timestamp_score(value: &str) -> u128 {
    let digits = value
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse::<u128>().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_contribution_rank_1_is_16() {
        // Magnitude guard: (1.0 / 61.0) * 1000.0 ≈ 16.39 → round → 16.
        // Changing RRF_K or RRF_SCALE will fail this test, alerting the
        // engineer to update both pipelines' magnitude tables.
        assert_eq!(rrf_contribution(1), 16);
    }

    #[test]
    fn rrf_contribution_decreases_with_rank() {
        let r1 = rrf_contribution(1);
        let r10 = rrf_contribution(10);
        let r100 = rrf_contribution(100);
        assert!(r1 > r10);
        assert!(r10 > r100);
        assert!(r100 >= 0);
    }

    #[test]
    fn freshness_score_returns_max_when_current_is_newest_or_future() {
        // Early-return branch: current >= newest.
        assert_eq!(freshness_score(1_000, 1_000), 6);
        assert_eq!(freshness_score(1_000, 5_000), 6);
    }

    #[test]
    fn freshness_score_within_first_bucket_returns_max() {
        // Bucket-path branch: delta < 10_000ms still rounds to bucket 0 → 6.
        assert_eq!(freshness_score(1_000, 999), 6);
        assert_eq!(freshness_score(200_000, 195_000), 6);
    }

    #[test]
    fn freshness_decays_in_10000ms_buckets() {
        // newest = 200_000ms, current = 195_000ms → delta = 5000 → bucket 0 → 6.
        assert_eq!(freshness_score(200_000, 195_000), 6);
        // delta = 15_000 → bucket 1 → 5.
        assert_eq!(freshness_score(200_000, 185_000), 5);
        // delta = 200_000 → bucket 20 (capped) → -14.
        assert_eq!(freshness_score(200_000, 0), -14);
        // delta = 1_000_000 → still capped at bucket 20 → -14.
        assert_eq!(freshness_score(1_000_000, 0), -14);
    }

    #[test]
    fn timestamp_score_extracts_digits() {
        assert_eq!(timestamp_score("00000000001234567890"), 1234567890);
        // RFC-3339 sloppy: digits get concatenated. "2026-04-30T00:00:00Z" has
        // 14 digits (2026 04 30 00 00 00) → 20_260_430_000_000.
        assert_eq!(timestamp_score("2026-04-30T00:00:00Z"), 20_260_430_000_000);
        assert_eq!(timestamp_score("not a number"), 0);
    }
}
