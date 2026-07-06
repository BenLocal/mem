use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbeddingError {
    #[error("embedding internal error: {0}")]
    Internal(String),
    #[error("embedding http error: {0}")]
    Http(String),
}

/// Qwen3-Embedding query-side instruction — the model card's retrieval
/// scheme is asymmetric: documents embed RAW, queries embed behind this
/// template. mem stores documents raw, so adopting the template on the
/// query side needs no re-embedding of anything on disk.
const QWEN3_QUERY_INSTRUCTION: &str =
    "Instruct: Given a search query, retrieve relevant passages that answer the query\nQuery: ";

/// Query-side embed input for `model`: the Qwen3-Embedding family gets
/// the instructed template; every other model passes through untouched
/// (zero alloc). Shared across providers — the template is keyed on the
/// model NAME, so the same model must behave identically no matter
/// which provider serves it (audit 2026-07-03 ⑮: the OpenAI-compatible
/// provider silently skipped it, degrading Qwen3-through-gateway
/// retrieval).
pub(crate) fn query_embed_input<'a>(model: &str, text: &'a str) -> std::borrow::Cow<'a, str> {
    if model.contains("Qwen3-Embedding") {
        std::borrow::Cow::Owned(format!("{QWEN3_QUERY_INSTRUCTION}{text}"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
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

    /// Embed a retrieval QUERY, as opposed to a document. Defaults to
    /// [`Self::embed_text`]; providers whose model was trained with an
    /// asymmetric query/document scheme (Qwen3-Embedding's instructed
    /// queries) override this to apply the query-side template. Storage
    /// and worker paths must keep using `embed_text` / `embed_batch` —
    /// only the query side is templated, so every stored vector stays
    /// valid without re-embedding.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.embed_text(text).await
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen3_queries_get_the_instructed_template_docs_stay_raw() {
        // Qwen3-Embedding is an asymmetric retrieval model: the model
        // card's scheme is instructed QUERIES over raw documents. The
        // helper must template exactly the Qwen3 family and leave every
        // other model's queries untouched (zero-alloc passthrough).
        let q = query_embed_input("Qwen/Qwen3-Embedding-0.6B", "where did Ann adopt her dog");
        assert!(q.starts_with("Instruct: "), "got: {q}");
        assert!(q.contains("\nQuery: where did Ann adopt her dog"));

        let other = query_embed_input("text-embedding-3-small", "same question");
        assert_eq!(other, "same question");
        assert!(matches!(other, std::borrow::Cow::Borrowed(_)));
    }
}
