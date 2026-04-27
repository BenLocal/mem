use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use sha2::{Digest, Sha256};
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
    #[allow(clippy::type_complexity)]
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

/// Hash a memory_id string to a u64 key using the first 8 bytes of SHA-256.
fn memory_id_to_u64(memory_id: &str) -> u64 {
    let digest = Sha256::digest(memory_id.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
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

    // ── Lock helpers ────────────────────────────────────────────────────────

    fn lock_index_read(&self) -> Result<RwLockReadGuard<'_, Index>, VectorIndexError> {
        self.index
            .read()
            .map_err(|e| VectorIndexError::UsearchOp(format!("index read lock poisoned: {e}")))
    }

    fn lock_index_write(&self) -> Result<RwLockWriteGuard<'_, Index>, VectorIndexError> {
        self.index
            .write()
            .map_err(|e| VectorIndexError::UsearchOp(format!("index write lock poisoned: {e}")))
    }

    fn lock_id_map_read(
        &self,
    ) -> Result<RwLockReadGuard<'_, HashMap<u64, String>>, VectorIndexError> {
        self.id_map
            .read()
            .map_err(|e| VectorIndexError::UsearchOp(format!("id_map read lock poisoned: {e}")))
    }

    fn lock_id_map_write(
        &self,
    ) -> Result<RwLockWriteGuard<'_, HashMap<u64, String>>, VectorIndexError> {
        self.id_map
            .write()
            .map_err(|e| VectorIndexError::UsearchOp(format!("id_map write lock poisoned: {e}")))
    }

    // ── Public accessors ────────────────────────────────────────────────────

    pub fn size(&self) -> usize {
        self.index.read().expect("index lock poisoned").size()
    }

    pub fn fingerprint(&self) -> &VectorIndexFingerprint {
        &self.fingerprint
    }

    // ── Core operations ─────────────────────────────────────────────────────

    /// Insert or update the embedding for `memory_id`.
    ///
    /// Lock acquisition order: `id_map` write first, then `index` write.
    ///
    /// Deviation from original spec: a detected SHA-256 u64 hash collision
    /// between two *different* memory IDs returns `VectorIndexError::HashCollision`
    /// rather than panicking. This is safer and lets callers decide policy.
    pub async fn upsert(
        &self,
        memory_id: &str,
        embedding: &[f32],
    ) -> Result<(), VectorIndexError> {
        if embedding.len() != self.fingerprint.dim {
            return Err(VectorIndexError::UsearchOp(format!(
                "embedding length {} does not match fingerprint dim {}",
                embedding.len(),
                self.fingerprint.dim
            )));
        }

        let key = memory_id_to_u64(memory_id);

        // Acquire id_map first (consistent lock ordering).
        let mut id_map = self.lock_id_map_write()?;
        if let Some(existing) = id_map.get(&key) {
            if existing != memory_id {
                return Err(VectorIndexError::HashCollision {
                    existing: existing.clone(),
                    incoming: memory_id.to_string(),
                });
            }
        }

        // Acquire index second.
        let index = self.lock_index_write()?;
        // remove is idempotent; ignore "not found" semantics
        let _ = index.remove(key);
        index
            .add(key, embedding)
            .map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?;

        id_map.insert(key, memory_id.to_string());
        Ok(())
    }

    /// Search for the `k` nearest neighbours of `query`.
    ///
    /// Returns an empty vec if `query` dimensions don't match, `k == 0`, or the
    /// index is empty.  Cosine metric: usearch returns distance = 1 − similarity,
    /// so similarity is computed as `1.0 - distance`.
    ///
    /// Lock acquisition order: `id_map` read first, then `index` read.
    pub async fn search(
        &self,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(String, f32)>, VectorIndexError> {
        if query.len() != self.fingerprint.dim || k == 0 {
            return Ok(vec![]);
        }

        // Acquire id_map first (consistent lock ordering).
        let id_map = self.lock_id_map_read()?;

        // Acquire index second, but drop it as soon as search() returns so
        // concurrent writers aren't blocked during the id_map iteration.
        let matches = {
            let index = self.lock_index_read()?;
            if index.size() == 0 {
                return Ok(vec![]);
            }
            index
                .search(query, k)
                .map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?
        };
        // index lock dropped here

        let mut out = Vec::with_capacity(matches.keys.len());
        for (i, key) in matches.keys.iter().enumerate() {
            if let Some(id) = id_map.get(key) {
                let dist = matches.distances[i];
                // usearch Cos metric returns distance = 1 - cosine_similarity
                let sim = 1.0 - dist;
                out.push((id.clone(), sim));
            }
        }
        Ok(out)
    }
}
