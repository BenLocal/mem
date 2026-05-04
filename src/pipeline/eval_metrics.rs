//! Information-retrieval evaluation metrics. Pure functions, no I/O.
//!
//! Used by the recall ablation bench (`tests/recall_bench.rs`) and
//! reusable for any future memories pipeline ablation.
//!
//! Conventions:
//! - All functions are tenant-agnostic; pass run + qrels as plain slices/sets.
//! - Generic over `I: Eq + Hash + Clone` so callers pass `String`, `&str`, or
//!   typed wrapper IDs without copying.
//! - Relevance is binary (0/1). gain = 1.0 if id ∈ qrels, else 0.0.

use std::collections::HashSet;
use std::hash::Hash;

/// Discounted cumulative gain over a `gains` list.
/// dcg = Σ gains[i] / log2(i + 2)  (i is 0-indexed)
pub fn dcg(gains: &[f64]) -> f64 {
    gains
        .iter()
        .enumerate()
        .map(|(i, g)| g / ((i + 2) as f64).log2())
        .sum()
}

/// Ideal DCG when there are `relevant_count` relevant docs and we cut at `k`.
pub fn ideal_dcg(relevant_count: usize, k: usize) -> f64 {
    let n = relevant_count.min(k);
    dcg(&vec![1.0; n])
}

/// NDCG@k. Returns 0.0 if qrels is empty (degenerate case).
pub fn ndcg_at_k<I: Eq + Hash + Clone>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    let actual: Vec<f64> = run
        .iter()
        .take(k)
        .map(|id| if qrels.contains(id) { 1.0 } else { 0.0 })
        .collect();
    let actual_dcg = dcg(&actual);
    let ideal = ideal_dcg(qrels.len(), k);
    if ideal == 0.0 {
        0.0
    } else {
        actual_dcg / ideal
    }
}

/// MRR — reciprocal rank of first relevant; 0 if none in run.
pub fn mrr<I: Eq + Hash + Clone>(run: &[I], qrels: &HashSet<I>) -> f64 {
    run.iter()
        .position(|id| qrels.contains(id))
        .map(|p| 1.0 / (p + 1) as f64)
        .unwrap_or(0.0)
}

/// Recall@k — fraction of relevant docs found in top-k.
pub fn recall_at_k<I: Eq + Hash + Clone>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    if qrels.is_empty() {
        return 0.0;
    }
    let hits = run.iter().take(k).filter(|id| qrels.contains(id)).count();
    hits as f64 / qrels.len() as f64
}

/// Precision@k — fraction of top-k that is relevant.
pub fn precision_at_k<I: Eq + Hash + Clone>(run: &[I], qrels: &HashSet<I>, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let n = run.len().min(k);
    if n == 0 {
        return 0.0;
    }
    let hits = run.iter().take(k).filter(|id| qrels.contains(id)).count();
    hits as f64 / k as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-4,
            "expected {expected}, got {actual}"
        );
    }

    fn qrels(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dcg_handles_empty() {
        assert_eq!(dcg(&[]), 0.0);
    }

    #[test]
    fn dcg_single_relevant_at_position_zero() {
        // gain at position 0 = 1 / log2(2) = 1.0
        approx(dcg(&[1.0]), 1.0);
    }

    #[test]
    fn dcg_three_relevant_handworked() {
        // [1,1,0,1] → 1/log2(2) + 1/log2(3) + 0 + 1/log2(5)
        //          = 1.0 + 0.6309297 + 0 + 0.4306765
        //          = 2.0616062
        approx(dcg(&[1.0, 1.0, 0.0, 1.0]), 2.0616062);
    }

    #[test]
    fn ideal_dcg_caps_at_k() {
        // 5 relevant, k=3 → top 3 all gain=1 → dcg of [1,1,1]
        // = 1/log2(2) + 1/log2(3) + 1/log2(4) = 1 + 0.6309 + 0.5 = 2.1309
        approx(ideal_dcg(5, 3), 2.1309297);
    }

    #[test]
    fn ndcg_at_k_handworked_partial_match() {
        // run=[a,b,c], qrels={a,c}, k=3
        // actual gains = [1,0,1] → dcg = 1/log2(2) + 0 + 1/log2(4) = 1.5
        // ideal:        [1,1]   → dcg = 1/log2(2) + 1/log2(3)     = 1.6309
        // ndcg = 1.5 / 1.6309 = 0.9197
        let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        approx(ndcg_at_k(&run, &qrels(&["a", "c"]), 3), 0.9196731);
    }

    #[test]
    fn ndcg_returns_zero_when_qrels_empty() {
        let run = vec!["a".to_string()];
        assert_eq!(ndcg_at_k(&run, &qrels(&[]), 5), 0.0);
    }

    #[test]
    fn ndcg_returns_one_when_run_is_perfect() {
        // run=[a,b,c], qrels={a,b,c}, k=3 → actual = ideal → 1.0
        let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        approx(ndcg_at_k(&run, &qrels(&["a", "b", "c"]), 3), 1.0);
    }

    #[test]
    fn mrr_first_relevant_at_position_zero() {
        let run = vec!["a".to_string(), "b".to_string()];
        approx(mrr(&run, &qrels(&["a"])), 1.0);
    }

    #[test]
    fn mrr_first_relevant_at_position_two() {
        let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        approx(mrr(&run, &qrels(&["c"])), 1.0 / 3.0);
    }

    #[test]
    fn mrr_no_relevant_returns_zero() {
        let run = vec!["a".to_string(), "b".to_string()];
        assert_eq!(mrr(&run, &qrels(&["x"])), 0.0);
    }

    #[test]
    fn recall_at_k_basic() {
        // run=[a,b], qrels={a,b,c,d}, k=2 → hits=2, denom=4 → 0.5
        let run = vec!["a".to_string(), "b".to_string()];
        approx(recall_at_k(&run, &qrels(&["a", "b", "c", "d"]), 2), 0.5);
    }

    #[test]
    fn recall_at_k_caps_at_k() {
        // run=[a,b,c,d,e], qrels={a,b,c}, k=2 → hits=2, denom=3 → 0.6667
        let run = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        approx(recall_at_k(&run, &qrels(&["a", "b", "c"]), 2), 2.0 / 3.0);
    }

    #[test]
    fn recall_returns_zero_when_qrels_empty() {
        let run = vec!["a".to_string()];
        assert_eq!(recall_at_k(&run, &qrels(&[]), 5), 0.0);
    }

    #[test]
    fn precision_at_k_basic() {
        // run=[a,b,c], qrels={a,c,e}, k=3 → hits=2, k=3 → 0.6667
        let run = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        approx(precision_at_k(&run, &qrels(&["a", "c", "e"]), 3), 2.0 / 3.0);
    }

    #[test]
    fn precision_at_k_zero_run_returns_zero() {
        let run: Vec<String> = vec![];
        assert_eq!(precision_at_k(&run, &qrels(&["a"]), 5), 0.0);
    }

    #[test]
    fn precision_at_k_zero_k_returns_zero() {
        let run = vec!["a".to_string()];
        assert_eq!(precision_at_k(&run, &qrels(&["a"]), 0), 0.0);
    }

    #[test]
    fn precision_handles_run_shorter_than_k() {
        // run=[a,b], qrels={a,b}, k=5 → hits=2, k=5 → 2/5 = 0.4
        let run = vec!["a".to_string(), "b".to_string()];
        approx(precision_at_k(&run, &qrels(&["a", "b"]), 5), 0.4);
    }
}
