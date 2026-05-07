use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbeddingError {
    #[error("embedding internal error: {0}")]
    Internal(String),
    #[error("embedding http error: {0}")]
    Http(String),
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn model(&self) -> &str;
    fn dim(&self) -> usize;

    /// Embed a single string. Used by the search read path (always one
    /// query) and as the default for `embed_batch` when the impl has no
    /// native batch path.
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Embed a batch of strings in a single call when the provider
    /// supports it. Returns one `Result` per input, in the same order, so
    /// a partial failure (one bad item in the batch) does not poison the
    /// whole batch — the worker can complete the successful jobs and
    /// reschedule the failures individually.
    ///
    /// Default impl is a sequential `embed_text` loop — correct, but no
    /// throughput benefit. Providers with native batch endpoints
    /// (embed_anything's `embed_query` over `&[&str]`, OpenAI's `inputs`
    /// array) override this for the real win: ~3-5× worker throughput
    /// at batch=8 for local Qwen3 / OpenAI.
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Result<Vec<f32>, EmbeddingError>>, EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed_text(t).await);
        }
        Ok(out)
    }
}
