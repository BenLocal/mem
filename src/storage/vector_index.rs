use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use thiserror::Error;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use super::StorageError;

#[derive(Debug, Clone)]
pub struct VectorIndexFingerprint {
    pub provider: String,
    pub model: String,
    pub dim: usize,
}

#[derive(Debug, Error)]
pub enum VectorIndexError {
    #[error("usearch error: {0}")]
    UsearchOp(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("fingerprint mismatch: stored={stored:?}, current={current:?}")]
    FingerprintMismatch {
        stored: VectorIndexFingerprint,
        current: VectorIndexFingerprint,
    },
    #[error("row count mismatch: index={index}, db={db}")]
    RowCountMismatch { index: usize, db: i64 },
    #[error("u64 hash collision for keys {existing} and {incoming}")]
    HashCollision {
        existing: String,
        incoming: String,
    },
}

pub trait EmbeddingRowSource {
    fn count_total_memory_embeddings(&self) -> Result<i64, StorageError>;
    fn for_each_embedding(
        &self,
        batch: usize,
        f: &mut dyn FnMut(&str, &[u8]) -> Result<(), StorageError>,
    ) -> Result<(), StorageError>;
}

pub struct VectorIndex {
    index: Arc<RwLock<Index>>,
    id_map: Arc<RwLock<HashMap<u64, String>>>,
    fingerprint: VectorIndexFingerprint,
}

impl VectorIndex {
    /// Construct an empty in-memory index. Used by tests and by the rebuild path
    /// before populating from a repository.
    pub fn new_in_memory(
        dim: usize,
        provider: &str,
        model: &str,
        capacity_hint: usize,
    ) -> Self {
        let opts = IndexOptions {
            dimensions: dim,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            connectivity: 0,
            expansion_add: 0,
            expansion_search: 0,
            multi: false,
        };
        let index = Index::new(&opts).expect("usearch index creation should succeed");
        index
            .reserve(capacity_hint.max(8))
            .expect("usearch reserve should succeed");
        Self {
            index: Arc::new(RwLock::new(index)),
            id_map: Arc::new(RwLock::new(HashMap::new())),
            fingerprint: VectorIndexFingerprint {
                provider: provider.to_string(),
                model: model.to_string(),
                dim,
            },
        }
    }

    pub fn size(&self) -> usize {
        self.index.read().expect("index lock poisoned").size()
    }

    pub fn fingerprint(&self) -> &VectorIndexFingerprint {
        &self.fingerprint
    }
}
