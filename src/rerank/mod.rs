//! I2 offline reranker lane (docs/offline-reranker-lane.md).
//!
//! Cross-encoder relevance scoring for WORKER-side relationship-quality
//! decisions — never on the query path (the I2 spike measured
//! ~650-800ms/pair on CPU, bandwidth-bound; interactive reranking is
//! explicitly out of scope). Consumers load a provider per batch and
//! drop it after (an f32 Qwen3-Reranker-0.6B materializes ~2.4GB —
//! never kept resident).
//!
//! Env surface (all read live, no restart needed):
//! - `MEM_RERANK_OFFLINE_ENABLED=1` — master opt-in, default OFF.
//! - `MEM_RERANK_PROVIDER` — `candle` (default) | `fake` (tests).
//! - `MEM_RERANK_MODEL_DIR` — local model directory (config.json +
//!   tokenizer.json + model.safetensors). No runtime HF download: this
//!   environment can't reach the hub directly; pre-warm per the deploy
//!   runbook. Default `~/.cache/huggingface/manual/Qwen3-Reranker-0.6B`.
//! - `MEM_RERANK_MERGE_FLOOR` — bidirectional-score floor for the
//!   evolution merge veto (P2), default 0.5, accepted range (0, 1].

pub mod candle_qwen3;
pub mod fake;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RerankError {
    #[error("rerank internal error: {0}")]
    Internal(String),
}

/// Cross-encoder relevance scorer. Synchronous by design: the candle
/// forward is CPU-bound for seconds — async callers must wrap calls in
/// `tokio::task::spawn_blocking` (constructing the provider inside the
/// closure, so the model load also stays off the executor).
pub trait RerankProvider: Send + Sync {
    fn model(&self) -> &str;

    /// Score `(query, document)` pairs → `P(relevant)` ∈ [0, 1], one per
    /// pair, input order preserved. Inputs are used verbatim — callers
    /// truncate with [`truncate_for_rerank`] first.
    fn score_pairs(&self, pairs: &[(String, String)]) -> Result<Vec<f32>, RerankError>;
}

/// Per-side input cap. ~1200 chars keeps a CJK-heavy pair around
/// 1.5-2.5k tokens (single-digit seconds per pair on CPU — fine for
/// worker cadence, and the discriminative signal lives in the head of a
/// capsule's content anyway). Truncation only shapes the SCORING input;
/// storage is never touched.
pub const RERANK_TRUNCATE_CHARS: usize = 1200;

/// UTF-8-safe truncation to [`RERANK_TRUNCATE_CHARS`] chars (not bytes —
/// slicing mid-codepoint on the Chinese-heavy corpus would panic).
pub fn truncate_for_rerank(text: &str) -> &str {
    match text.char_indices().nth(RERANK_TRUNCATE_CHARS) {
        Some((byte_idx, _)) => &text[..byte_idx],
        None => text,
    }
}

fn is_truthy(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

/// Master opt-in switch (default OFF), read live.
pub fn offline_enabled() -> bool {
    std::env::var("MEM_RERANK_OFFLINE_ENABLED")
        .map(|v| is_truthy(&v))
        .unwrap_or(false)
}

/// Merge-veto floor (P2): a merge loser whose bidirectional geometric
/// mean against the survivor falls below this is vetoed. Default 0.5;
/// out-of-range / unparseable values fall back silently (same lenient
/// posture as `MEM_RECALL_PER_SOURCE_CAP`).
pub fn merge_floor() -> f32 {
    std::env::var("MEM_RERANK_MERGE_FLOOR")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|f| *f > 0.0 && *f <= 1.0)
        .unwrap_or(0.5)
}

/// Model directory for the candle provider.
pub fn model_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("MEM_RERANK_MODEL_DIR") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    std::path::PathBuf::from(home).join(".cache/huggingface/manual/Qwen3-Reranker-0.6B")
}

/// Construct the provider selected by `MEM_RERANK_PROVIDER`. The candle
/// path LOADS THE MODEL (~1.5s + ~2.4GB) — call inside `spawn_blocking`
/// and drop the provider when the batch is done.
pub fn provider_from_env() -> Result<Box<dyn RerankProvider>, RerankError> {
    match std::env::var("MEM_RERANK_PROVIDER").as_deref() {
        Ok("fake") => Ok(Box::new(fake::FakeReranker)),
        _ => Ok(Box::new(candle_qwen3::CandleQwen3Reranker::load(
            &model_dir(),
        )?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        // A CJK string sliced at a byte boundary would panic; the helper
        // must cut on char boundaries and leave short inputs untouched.
        let short = "短文本";
        assert_eq!(truncate_for_rerank(short), short);
        let long: String = "汉".repeat(RERANK_TRUNCATE_CHARS + 50);
        let cut = truncate_for_rerank(&long);
        assert_eq!(cut.chars().count(), RERANK_TRUNCATE_CHARS);
        assert!(long.starts_with(cut));
    }

    #[test]
    fn merge_floor_parses_and_falls_back() {
        // No env in this test process for the happy default.
        std::env::remove_var("MEM_RERANK_MERGE_FLOOR");
        assert!((merge_floor() - 0.5).abs() < f32::EPSILON);
        std::env::set_var("MEM_RERANK_MERGE_FLOOR", "0.8");
        assert!((merge_floor() - 0.8).abs() < f32::EPSILON);
        // Out-of-range and garbage fall back to the default.
        std::env::set_var("MEM_RERANK_MERGE_FLOOR", "1.5");
        assert!((merge_floor() - 0.5).abs() < f32::EPSILON);
        std::env::set_var("MEM_RERANK_MERGE_FLOOR", "abc");
        assert!((merge_floor() - 0.5).abs() < f32::EPSILON);
        std::env::remove_var("MEM_RERANK_MERGE_FLOOR");
    }

    #[test]
    fn fake_provider_scores_by_marker() {
        use super::fake::FakeReranker;
        let p = FakeReranker;
        let scores = p
            .score_pairs(&[
                ("query".into(), "normal document".into()),
                ("query".into(), "contains rerank-low marker".into()),
            ])
            .unwrap();
        assert!(scores[0] > 0.9, "unmarked pair scores high: {scores:?}");
        assert!(scores[1] < 0.1, "marked pair scores low: {scores:?}");
    }
}
