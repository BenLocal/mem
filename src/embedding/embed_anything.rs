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

use super::provider::{query_embed_input, EmbeddingError, EmbeddingProvider};

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
        let model = self.model.clone();
        get_or_init_cancel_safe(&self.embedder, move || {
            info!(model = %model, "embedanything loading model from HF");
            let embedder = EmbedderBuilder::new()
                .model_id(Some(model.as_str()))
                .from_pretrained_hf()
                .map_err(|e| EmbeddingError::Internal(format!("embedanything init: {e}")))?;
            info!("embedanything model ready");
            Ok(embedder)
        })
        .await
    }
}

/// Lazily initialize a shared `Arc<T>` slot in a way that SURVIVES caller
/// cancellation.
///
/// The naive shape — take the lock, `spawn_blocking(load).await`, store into
/// the slot — has a cancellation hole: if the caller's future is dropped
/// while awaiting the load (a request deadline like
/// `MEM_RECALL_SEMANTIC_TIMEOUT_MS`, or the HTTP client disconnecting and
/// axum dropping the handler), the completed load's result is discarded and
/// the store line never runs. The cache stays empty, so the NEXT caller
/// reloads from scratch and — if the load takes longer than the deadline —
/// is cancelled the same way, forever: every request pays a full model load,
/// none ever caches it (observed live 2026-07-24: every search pinned at
/// deadline+ε with the semantic channel permanently degraded).
///
/// Fix: run `load` inside a **detached** `tokio::spawn` whose own task stores
/// the result into the slot. Dropping a `JoinHandle` does not abort a tokio
/// task, so a cancelled caller abandons only its `.await` — the load still
/// completes and populates the cache for every later caller. Concurrent
/// first-callers may race and load twice (no cross-caller single-flight);
/// the first store wins via the `is_none` check and the duplicate is dropped
/// — acceptable for a once-per-process model load, and strictly better than
/// re-introducing a lock held across the load (the cancellation hole above).
async fn get_or_init_cancel_safe<T, F>(
    slot: &Arc<Mutex<Option<Arc<T>>>>,
    load: F,
) -> Result<Arc<T>, EmbeddingError>
where
    T: Send + Sync + 'static,
    F: FnOnce() -> Result<T, EmbeddingError> + Send + 'static,
{
    if let Some(cached) = slot.lock().await.as_ref() {
        return Ok(cached.clone());
    }
    let slot = slot.clone();
    let task = tokio::spawn(async move {
        let loaded = tokio::task::spawn_blocking(load)
            .await
            .map_err(|e| EmbeddingError::Internal(format!("embedanything task join: {e}")))??;
        let mut guard = slot.lock().await;
        if guard.is_none() {
            *guard = Some(Arc::new(loaded));
        }
        // Serve whatever won the store — first load wins, duplicates dropped.
        Ok::<_, EmbeddingError>(guard.as_ref().expect("just stored").clone())
    });
    task.await
        .map_err(|e| EmbeddingError::Internal(format!("embedanything init join: {e}")))?
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
mod cancel_safe_init_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Regression: the search-path deadline (MEM_RECALL_SEMANTIC_TIMEOUT_MS)
    /// cancels the caller's embed future. Before this fix the model lazy-load
    /// ran inside the caller's future, so a deadline shorter than the load
    /// (~2.5s) cancelled the load BEFORE the cache-store line ran — the cache
    /// stayed empty and EVERY subsequent search re-loaded the model from
    /// scratch and got cut again, forever (observed live: 35/35 searches
    /// degraded, all pinned at deadline+ε). The load must survive caller
    /// cancellation and populate the cache from its own detached task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn init_survives_caller_cancellation_and_caches_once() {
        let slot: std::sync::Arc<Mutex<Option<std::sync::Arc<String>>>> = Default::default();
        let loads = std::sync::Arc::new(AtomicUsize::new(0));

        // First caller is cancelled mid-load (deadline shorter than load).
        let l = loads.clone();
        let first = get_or_init_cancel_safe(&slot, move || {
            l.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            Ok("model".to_string())
        });
        let cut = tokio::time::timeout(Duration::from_millis(50), first).await;
        assert!(
            cut.is_err(),
            "first caller must be cancelled by its deadline"
        );

        // The detached load still completes and populates the cache.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            slot.lock().await.is_some(),
            "cache must be populated despite the caller's cancellation"
        );

        // A later caller reuses the cache — no second load.
        let l = loads.clone();
        let got = get_or_init_cancel_safe(&slot, move || {
            l.fetch_add(1, Ordering::SeqCst);
            Ok("reloaded".to_string())
        })
        .await
        .expect("cached init");
        assert_eq!(
            *got, "model",
            "must serve the FIRST load's value from cache"
        );
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "model must be loaded exactly once"
        );
    }
}
