use async_trait::async_trait;
use serde::Deserialize;

use crate::config::EmbeddingSettings;

use super::provider::{EmbeddingError, EmbeddingProvider};

#[derive(Debug, Clone)]
pub struct OpenAiEmbeddingProvider {
    api_key: String,
    model: String,
    dim: usize,
    client: reqwest::Client,
}

impl OpenAiEmbeddingProvider {
    pub fn from_settings(settings: &EmbeddingSettings) -> Result<Self, EmbeddingError> {
        let api_key = settings
            .openai_api_key
            .clone()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| EmbeddingError::Internal("OPENAI_API_KEY missing".to_string()))?;

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| EmbeddingError::Http(e.to_string()))?;

        Ok(Self {
            api_key,
            model: settings.model.clone(),
            dim: settings.dim,
            client,
        })
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingsResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddingProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut body = serde_json::json!({
            "model": self.model,
            "input": text,
        });
        // OpenAI supports reducing width for 3rd-gen embedding models; omit for ada-002 and others.
        if self.model.contains("text-embedding-3") {
            body["dimensions"] = serde_json::json!(self.dim);
        }

        let response = self
            .client
            .post("https://api.openai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| EmbeddingError::Http(e.to_string()))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| EmbeddingError::Http(e.to_string()))?;

        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).to_string();
            return Err(EmbeddingError::Http(format!("OpenAI {status}: {msg}")));
        }

        let parsed: OpenAiEmbeddingsResponse = serde_json::from_slice(&bytes).map_err(|e| {
            EmbeddingError::Internal(format!(
                "OpenAI embeddings JSON: {e}; body={}",
                String::from_utf8_lossy(&bytes)
            ))
        })?;

        let vec = parsed
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| EmbeddingError::Internal("OpenAI embeddings: empty data".to_string()))?;

        if vec.len() != self.dim {
            return Err(EmbeddingError::Internal(format!(
                "OpenAI embedding length {} does not match EMBEDDING_DIM {}",
                vec.len(),
                self.dim
            )));
        }

        Ok(vec)
    }

    /// Native batch path. OpenAI's `/v1/embeddings` accepts an array
    /// `input`, returning one element per input in `data` (preserving
    /// input order — documented contract). One HTTP roundtrip + one
    /// model invocation amortises auth + TLS + tokenisation overhead;
    /// at batch=8 typically ~3× faster than 8 sequential single calls.
    ///
    /// HTTP-level failures (non-2xx, parse error) collapse the whole
    /// batch into a single error — caller falls back to per-element
    /// retry by sending each input on its own next tick.
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Result<Vec<f32>, EmbeddingError>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "input": texts,
        });
        if self.model.contains("text-embedding-3") {
            body["dimensions"] = serde_json::json!(self.dim);
        }

        let response = self
            .client
            .post("https://api.openai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| EmbeddingError::Http(e.to_string()))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| EmbeddingError::Http(e.to_string()))?;

        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).to_string();
            return Err(EmbeddingError::Http(format!("OpenAI {status}: {msg}")));
        }

        let parsed: OpenAiEmbeddingsResponse = serde_json::from_slice(&bytes).map_err(|e| {
            EmbeddingError::Internal(format!(
                "OpenAI embeddings JSON: {e}; body={}",
                String::from_utf8_lossy(&bytes)
            ))
        })?;

        if parsed.data.len() != texts.len() {
            return Err(EmbeddingError::Internal(format!(
                "OpenAI batch returned {} vectors for {} inputs",
                parsed.data.len(),
                texts.len()
            )));
        }

        let dim = self.dim;
        let results = parsed
            .data
            .into_iter()
            .map(|d| {
                if d.embedding.len() != dim {
                    Err(EmbeddingError::Internal(format!(
                        "OpenAI batch element length {} != EMBEDDING_DIM {}",
                        d.embedding.len(),
                        dim
                    )))
                } else {
                    Ok(d.embedding)
                }
            })
            .collect();
        Ok(results)
    }
}
