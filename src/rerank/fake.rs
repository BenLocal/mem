//! Deterministic test provider — CI and integration tests never load
//! the real model (1.19GB, not in the repo).

use super::{RerankError, RerankProvider};

/// Marker the tests plant to force a low score.
const LOW_MARKER: &str = "rerank-low";

/// Scores are keyed on a content marker so tests control the verdict
/// deterministically: any pair whose either side contains `rerank-low`
/// scores 0.05, everything else 0.95. Test-only semantics — the real
/// provider is [`super::candle_qwen3::CandleQwen3Reranker`].
pub struct FakeReranker;

impl RerankProvider for FakeReranker {
    fn model(&self) -> &str {
        "fake-reranker"
    }

    fn score_pairs(&self, pairs: &[(String, String)]) -> Result<Vec<f32>, RerankError> {
        Ok(pairs
            .iter()
            .map(|(q, d)| {
                if q.contains(LOW_MARKER) || d.contains(LOW_MARKER) {
                    0.05
                } else {
                    0.95
                }
            })
            .collect())
    }
}
