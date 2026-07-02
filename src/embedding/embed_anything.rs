use std::sync::Arc;

use async_trait::async_trait;
use embed_anything::{
    config::TextEmbedConfig,
    embed_query,
    embeddings::embed::{Embedder, EmbedderBuilder},
};
use tokio::sync::Mutex;
use tracing::info;

use crate::config::EmbeddingSettings;

use super::provider::{EmbeddingError, EmbeddingProvider};

pub struct EmbedAnythingEmbeddingProvider {
    model: String,
    dim: usize,
    embedder: Arc<Mutex<Option<Arc<Embedder>>>>,
}

impl EmbedAnythingEmbeddingProvider {
    pub fn from_settings(settings: &EmbeddingSettings) -> Result<Self, EmbeddingError> {
        if settings.model.trim().is_empty() {
            return Err(EmbeddingError::Internal(
                "EMBEDDING_MODEL is required for embedanything".to_string(),
            ));
        }
        Ok(Self {
            model: settings.model.clone(),
            dim: settings.dim,
            embedder: Arc::new(Mutex::new(None)),
        })
    }

    async fn get_or_init_embedder(&self) -> Result<Arc<Embedder>, EmbeddingError> {
        let mut guard = self.embedder.lock().await;
        if let Some(embedder) = guard.as_ref() {
            return Ok(embedder.clone());
        }

        let model = self.model.clone();
        info!(model = %model, "embedanything loading model from HF");
        let embedder = tokio::task::spawn_blocking(move || {
            EmbedderBuilder::new()
                .model_id(Some(model.as_str()))
                .from_pretrained_hf()
        })
        .await
        .map_err(|e| EmbeddingError::Internal(format!("embedanything task join: {e}")))?
        .map_err(|e| EmbeddingError::Internal(format!("embedanything init: {e}")))?;

        let embedder = Arc::new(embedder);
        info!("embedanything model ready");
        *guard = Some(embedder.clone());
        Ok(embedder)
    }
}

/// Qwen3-Embedding query-side instruction — the model card's retrieval
/// scheme is asymmetric: documents embed RAW, queries embed behind this
/// template. mem stores documents raw, so adopting the template on the
/// query side needs no re-embedding of anything on disk.
const QWEN3_QUERY_INSTRUCTION: &str =
    "Instruct: Given a search query, retrieve relevant passages that answer the query\nQuery: ";

/// Query-side embed input for `model`: the Qwen3-Embedding family gets
/// the instructed template; every other model passes through untouched
/// (zero alloc).
fn query_embed_input<'a>(model: &str, text: &'a str) -> std::borrow::Cow<'a, str> {
    if model.contains("Qwen3-Embedding") {
        std::borrow::Cow::Owned(format!("{QWEN3_QUERY_INSTRUCTION}{text}"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

#[async_trait]
impl EmbeddingProvider for EmbedAnythingEmbeddingProvider {
    fn name(&self) -> &'static str {
        "embedanything"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let input = query_embed_input(&self.model, text);
        self.embed_text(&input).await
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let embedder = self.get_or_init_embedder().await?;
        let query = [text];
        let out = embed_query(&query, &embedder, Some(&TextEmbedConfig::default()))
            .await
            .map_err(|e| EmbeddingError::Internal(format!("embedanything query: {e}")))?;
        let first = out
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::Internal("embedanything empty output".to_string()))?;
        let dense = first
            .embedding
            .to_dense()
            .map_err(|e| EmbeddingError::Internal(format!("embedanything dense vector: {e}")))?;
        if dense.len() != self.dim {
            return Err(EmbeddingError::Internal(format!(
                "embedanything embedding length {} does not match EMBEDDING_DIM {}",
                dense.len(),
                self.dim
            )));
        }
        Ok(dense)
    }

    /// Native batch path. `embed_query` accepts `&[&str]` and processes
    /// the entire batch in a single forward pass (Qwen3-1024 batch=8 ≈
    /// 4-6× faster than 8 sequential single-input calls). Whole-batch
    /// failures collapse into a per-element error so the caller can
    /// still retry items individually.
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Result<Vec<f32>, EmbeddingError>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let embedder = self.get_or_init_embedder().await?;
        let dim = self.dim;
        let outs = embed_query(texts, &embedder, Some(&TextEmbedConfig::default()))
            .await
            .map_err(|e| EmbeddingError::Internal(format!("embedanything batch query: {e}")))?;
        if outs.len() != texts.len() {
            return Err(EmbeddingError::Internal(format!(
                "embedanything batch returned {} vectors for {} inputs",
                outs.len(),
                texts.len()
            )));
        }
        let mut results = Vec::with_capacity(outs.len());
        for out in outs {
            let r = match out.embedding.to_dense() {
                Ok(v) if v.len() == dim => Ok(v),
                Ok(v) => Err(EmbeddingError::Internal(format!(
                    "embedanything batch element length {} != EMBEDDING_DIM {}",
                    v.len(),
                    dim
                ))),
                Err(e) => Err(EmbeddingError::Internal(format!(
                    "embedanything dense vector: {e}"
                ))),
            };
            results.push(r);
        }
        Ok(results)
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
