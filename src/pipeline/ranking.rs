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

/// Float-valued RRF merge for two ranked candidate lists. Matches the
/// `1.0 / (60.0 + rank_lex) + 1.0 / (60.0 + rank_sem)` formula the
/// legacy fused-SQL `hybrid_candidates` used inline, so swapping the
/// implementation from SQL to Rust-compose doesn't change the scores
/// downstream rankers see.
///
/// Different from [`rrf_contribution`] (which is the scaled `i64`
/// form the memories `score_candidates_hybrid_rrf` pipeline uses).
/// Both forms coexist on purpose:
///
///   - `rrf_merge` matches what the fused SQL produced (`f32`, no
///     scale factor) — used by the storage layer's `hybrid_candidates`
///     compose path.
///   - `rrf_contribution` matches what the higher-level memories
///     pipeline produced (`i64`, ×1000 scale) — used by
///     `pipeline::retrieve::score_candidates_hybrid_rrf`.
///
/// They differ by a factor of `RRF_SCALE = 1000.0`. Don't unify
/// without a migration of every downstream test that hard-codes the
/// old magnitudes.
///
/// Returns `Vec<(id, score)>` sorted by score descending, ties broken
/// by id ascending — matches the legacy fused SQL's
/// `ORDER BY rrf_score DESC, ..., capability_capsule_id ASC` (the
/// `updated_at DESC` tiebreaker requires the full record and lives at
/// the hydration layer, not here).
pub fn rrf_merge(bm25: &[(String, i64)], ann: &[(String, i64)]) -> Vec<(String, f32)> {
    use std::collections::HashMap;
    let mut scores: HashMap<&str, f32> = HashMap::new();
    for (id, rank) in bm25 {
        *scores.entry(id.as_str()).or_insert(0.0) += 1.0 / (RRF_K as f32 + *rank as f32);
    }
    for (id, rank) in ann {
        *scores.entry(id.as_str()).or_insert(0.0) += 1.0 / (RRF_K as f32 + *rank as f32);
    }
    let mut out: Vec<(String, f32)> = scores
        .into_iter()
        .map(|(id, score)| (id.to_string(), score))
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
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
    fn rrf_merge_items_in_both_outrank_one_sided() {
        let bm25 = vec![("a".to_string(), 1), ("b".to_string(), 2)];
        let ann = vec![("a".to_string(), 1), ("c".to_string(), 1)];
        let merged = rrf_merge(&bm25, &ann);
        // `a` hits both channels at rank 1 → highest score.
        assert_eq!(merged[0].0, "a");
        // `c` (vec only, rank 1) and `b` (lex only, rank 2): `c`
        // outranks `b` because 1/61 > 1/62.
        assert_eq!(merged[1].0, "c");
        assert_eq!(merged[2].0, "b");
    }

    #[test]
    fn rrf_merge_score_matches_legacy_sql_formula() {
        // The legacy fused SQL computed
        //   1.0/(60.0+rank_lex) + 1.0/(60.0+rank_sem)
        // in DOUBLE then cast to FLOAT. We reproduce that in f32; the
        // magnitude must match to within float rounding so swapping
        // from fused-SQL to Rust-compose doesn't shift relative
        // rankings downstream.
        let bm25 = vec![("x".to_string(), 1)];
        let ann = vec![("x".to_string(), 1)];
        let merged = rrf_merge(&bm25, &ann);
        let expected = 1.0_f32 / 61.0 + 1.0_f32 / 61.0;
        assert!((merged[0].1 - expected).abs() < 1e-6);
    }

    #[test]
    fn rrf_merge_ties_broken_by_id_ascending() {
        // Two ids with identical scores → ascending id order.
        let bm25 = vec![("zebra".to_string(), 1), ("apple".to_string(), 1)];
        let ann = vec![];
        let merged = rrf_merge(&bm25, &ann);
        assert_eq!(merged[0].0, "apple");
        assert_eq!(merged[1].0, "zebra");
    }

    #[test]
    fn rrf_merge_empty_inputs_returns_empty() {
        let merged = rrf_merge(&[], &[]);
        assert!(merged.is_empty());
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
