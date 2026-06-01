//! Deterministic designed-geometry embedding provider for the recall
//! bench. Each topic owns an orthogonal basis vector; a text embeds to
//! the normalized sum of the bases for the topics whose canonical term
//! appears, plus tiny deterministic jitter. Same function embeds content
//! and queries, so same-topic items are nearest neighbours by
//! construction.
use async_trait::async_trait;
use mem::embedding::{EmbeddingError, EmbeddingProvider};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct GeometryProvider {
    dim: usize,
    topic_index: HashMap<String, usize>,
}

impl GeometryProvider {
    /// `topics` map to basis dims `0..topics.len()`. `dim` must be
    /// >= topics.len() so each topic gets its own axis.
    pub fn new(topics: &[&str], dim: usize) -> Self {
        assert!(dim >= topics.len(), "dim must be >= number of topics");
        let topic_index = topics
            .iter()
            .enumerate()
            .map(|(i, t)| (t.to_lowercase(), i))
            .collect();
        Self { dim, topic_index }
    }

    fn jitter(text: &str, i: usize) -> f32 {
        let mut h: u64 = 1469598103934665603;
        for b in text.bytes() {
            h = (h ^ b as u64).wrapping_mul(1099511628211);
        }
        h = (h ^ i as u64).wrapping_mul(1099511628211);
        ((h % 1000) as f32 / 1000.0) * 0.01
    }

    /// Pure, synchronous core — embed `text` to a unit vector. Exposed so
    /// tests can exercise it without an async runtime.
    pub fn raw(&self, text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let mut v = vec![0.0_f32; self.dim];
        let mut hit = false;
        for (term, &idx) in &self.topic_index {
            if lower.contains(term.as_str()) {
                v[idx] += 1.0;
                hit = true;
            }
        }
        if !hit {
            for (i, vi) in v.iter_mut().enumerate() {
                *vi += Self::jitter(&lower, i + self.dim) * 50.0;
            }
        }
        for (i, vi) in v.iter_mut().enumerate() {
            *vi += Self::jitter(&lower, i);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

#[async_trait]
impl EmbeddingProvider for GeometryProvider {
    fn name(&self) -> &'static str {
        "geometry"
    }
    fn model(&self) -> &str {
        "geometry-bench"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.raw(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn same_topic_closer_than_cross_topic() {
        let p = GeometryProvider::new(&["tokio", "lance", "duckdb"], 16);
        let query = p.raw("how to use tokio runtime");
        let same = p.raw("tokio async tasks");
        let cross = p.raw("duckdb single mutex");
        let sim_same = dot(&query, &same);
        let sim_cross = dot(&query, &cross);
        assert!(
            sim_same - sim_cross > 0.3,
            "expected sim_same ({sim_same:.4}) - sim_cross ({sim_cross:.4}) > 0.3"
        );
    }

    #[test]
    fn deterministic_and_unit_norm() {
        let p = GeometryProvider::new(&["tokio", "lance", "duckdb"], 16);
        let a = p.raw("tokio");
        let b = p.raw("tokio");
        assert_eq!(a, b, "raw() must be deterministic");
        assert_eq!(a.len(), 16, "vector length must equal dim");
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "L2 norm must be ~1.0, got {norm}"
        );
    }
}
