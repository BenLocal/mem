use async_trait::async_trait;
use sha2::{Digest, Sha256};

use super::provider::{EmbeddingError, EmbeddingProvider};

#[derive(Debug, Clone)]
pub struct FakeEmbeddingProvider {
    model: String,
    dim: usize,
}

impl FakeEmbeddingProvider {
    pub fn new(model: impl Into<String>, dim: usize) -> Self {
        Self {
            model: model.into(),
            dim,
        }
    }

    pub fn from_settings(settings: &crate::config::EmbeddingSettings) -> Self {
        Self::new(settings.model.clone(), settings.dim)
    }
}

#[async_trait]
impl EmbeddingProvider for FakeEmbeddingProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(deterministic_embedding(text, self.dim))
    }
}

/// Deterministic vectors for tests and offline tooling (matches [`FakeEmbeddingProvider`]).
pub fn deterministic_embedding(text: &str, dim: usize) -> Vec<f32> {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let hash = hasher.finalize();
    let mut out = Vec::with_capacity(dim);
    let mut state: u64 = u64::from_le_bytes([
        hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
    ]);
    for i in 0..dim {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let b = hash[i % hash.len()] as u64;
        let x = ((state ^ b) & 0xffff) as f32 / 32768.0 - 1.0;
        out.push(x);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_input_same_vector() {
        let p = FakeEmbeddingProvider::new("fake", 64);
        let a = p.embed_text("hello").await.unwrap();
        let b = p.embed_text("hello").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn different_inputs_differ() {
        let p = FakeEmbeddingProvider::new("fake", 64);
        let a = p.embed_text("alpha").await.unwrap();
        let b = p.embed_text("beta").await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn respects_dim() {
        let p = FakeEmbeddingProvider::new("fake", 128);
        let v = p.embed_text("x").await.unwrap();
        assert_eq!(v.len(), 128);
    }
}
