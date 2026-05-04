//! Oracle "perfect filter" reranker. Partitions a run into relevant +
//! irrelevant; relevant comes first, in original score order; irrelevant
//! follows, in original order. Spec §7.

use std::collections::HashSet;
use std::hash::Hash;

pub fn oracle_rerank<I: Eq + Hash + Clone>(run: Vec<I>, qrels: &HashSet<I>) -> Vec<I> {
    let (rel, irrel): (Vec<I>, Vec<I>) = run.into_iter().partition(|id| qrels.contains(id));
    [rel, irrel].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qrels(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn oracle_promotes_relevant_to_front() {
        let run = vec![
            "x".to_string(),
            "a".to_string(),
            "y".to_string(),
            "b".to_string(),
        ];
        let result = oracle_rerank(run, &qrels(&["a", "b"]));
        assert_eq!(result, vec!["a", "b", "x", "y"]);
    }

    #[test]
    fn oracle_preserves_relative_order_within_partitions() {
        let run = vec![
            "a".to_string(),
            "x".to_string(),
            "b".to_string(),
            "y".to_string(),
        ];
        let result = oracle_rerank(run, &qrels(&["a", "b"]));
        assert_eq!(result, vec!["a", "b", "x", "y"]); // a before b; x before y
    }

    #[test]
    fn oracle_handles_all_relevant() {
        let run = vec!["a".to_string(), "b".to_string()];
        let result = oracle_rerank(run, &qrels(&["a", "b"]));
        assert_eq!(result, vec!["a", "b"]);
    }

    #[test]
    fn oracle_handles_none_relevant() {
        let run = vec!["x".to_string(), "y".to_string()];
        let result = oracle_rerank(run, &qrels(&["a"]));
        assert_eq!(result, vec!["x", "y"]);
    }

    #[test]
    fn oracle_handles_empty_run() {
        let run: Vec<String> = vec![];
        let result = oracle_rerank(run, &qrels(&["a"]));
        assert!(result.is_empty());
    }
}
