use std::sync::Arc;

use async_trait::async_trait;
use embed_anything::{
    config::TextEmbedConfig,
    embed_query,
    embeddings::embed::{Embedder, EmbedderBuilder},
};
use tokio::sync::Mutex;

use crate::config::EmbeddingSettings;

use super::provider::{EmbeddingError, EmbeddingProvider};

#[derive(Debug)]
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
        let embedder = tokio::task::spawn_blocking(move || {
            EmbedderBuilder::new()
                .model_id(Some(model.as_str()))
                .from_pretrained_hf()
        })
        .await
        .map_err(|e| EmbeddingError::Internal(format!("embedanything task join: {e}")))?
        .map_err(|e| EmbeddingError::Internal(format!("embedanything init: {e}")))?;

        let embedder = Arc::new(embedder);
        *guard = Some(embedder.clone());
        Ok(embedder)
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
}
