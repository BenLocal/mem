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
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;
}
