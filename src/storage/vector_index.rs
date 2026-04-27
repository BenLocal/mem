use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use super::StorageError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexMeta {
    pub schema_version: u32,
    pub provider: String,
    pub model: String,
    pub dim: usize,
    pub row_count: usize,
    /// Stored as `{ "<u64-as-decimal-string>": "<memory_id>" }` to satisfy JSON object key rules.
    #[serde(with = "u64_keyed_map")]
    pub id_map: HashMap<u64, String>,
}

mod u64_keyed_map {
    use std::collections::HashMap;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        m: &HashMap<u64, String>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let stringy: HashMap<String, &String> =
            m.iter().map(|(k, v)| (k.to_string(), v)).collect();
        serde::Serialize::serialize(&stringy, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<HashMap<u64, String>, D::Error> {
        let stringy: HashMap<String, String> = HashMap::deserialize(d)?;
        stringy
            .into_iter()
            .map(|(k, v)| {
                k.parse::<u64>()
                    .map(|key| (key, v))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

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

impl std::fmt::Debug for VectorIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorIndex")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
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

    /// Remove the embedding for `memory_id` from the index.
    ///
    /// If `memory_id` was never inserted this is a no-op (returns `Ok`).
    ///
    /// Lock acquisition order: `id_map` write first, then `index` write.
    pub async fn remove(&self, memory_id: &str) -> Result<(), VectorIndexError> {
        let key = memory_id_to_u64(memory_id);
        let mut id_map = self.lock_id_map_write()?;
        let index = self.lock_index_write()?;
        // usearch::Index::remove returns Result<usize, cxx::Exception> where
        // the usize is the count of entries removed (0 or 1).  We treat both
        // outcomes as success; a "not found" is simply a no-op.
        let _ = index
            .remove(key)
            .map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?;
        id_map.remove(&key);
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

    /// Atomically persist the binary index and its meta JSON to disk.
    ///
    /// Writes to `.tmp` siblings first, then renames both into place so that a
    /// crash mid-write cannot leave partial files at the final paths.
    ///
    /// Lock acquisition order: `id_map` read first, then `index` read
    /// (consistent with the rest of the codebase).
    pub async fn save_to(
        &self,
        index_path: &Path,
        meta_path: &Path,
    ) -> Result<(), VectorIndexError> {
        // Acquire locks in id_map-first order to capture a consistent snapshot.
        let id_map = self.lock_id_map_read()?;
        let index = self.lock_index_read()?;

        let row_count = index.size();
        let meta = VectorIndexMeta {
            schema_version: 1,
            provider: self.fingerprint.provider.clone(),
            model: self.fingerprint.model.clone(),
            dim: self.fingerprint.dim,
            row_count,
            id_map: id_map.clone(),
        };

        let parent = index_path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;

        // Build temp paths by appending ".tmp" to avoid surprising behaviour
        // from Path::with_extension which replaces (not appends) the extension.
        let tmp_index: PathBuf = {
            let mut s = index_path.as_os_str().to_owned();
            s.push(".tmp");
            PathBuf::from(s)
        };
        let tmp_meta: PathBuf = {
            let mut s = meta_path.as_os_str().to_owned();
            s.push(".tmp");
            PathBuf::from(s)
        };

        index
            .save(tmp_index.to_str().ok_or_else(|| {
                VectorIndexError::UsearchOp("non-utf8 sidecar path".to_string())
            })?)
            .map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?;

        fs::write(&tmp_meta, serde_json::to_vec_pretty(&meta)?)?;

        fs::rename(&tmp_index, index_path)?;
        fs::rename(&tmp_meta, meta_path)?;

        Ok(())
    }

    /// Load a previously saved index from disk, validating the fingerprint.
    ///
    /// Returns `VectorIndexError::FingerprintMismatch` if the stored metadata
    /// does not match `expected_fp` (provider, model, or dim) or if `meta.dim == 0`
    /// (corrupted / hand-edited meta that would silently accept any query).
    pub async fn load_from(
        index_path: &Path,
        meta_path: &Path,
        expected_fp: &VectorIndexFingerprint,
    ) -> Result<Self, VectorIndexError> {
        let meta_bytes = fs::read(meta_path)?;
        let meta: VectorIndexMeta = serde_json::from_slice(&meta_bytes)?;

        // Fingerprint validation: provider, model, and dim must all match.
        if meta.provider != expected_fp.provider
            || meta.model != expected_fp.model
            || meta.dim != expected_fp.dim
        {
            return Err(VectorIndexError::FingerprintMismatch {
                stored: VectorIndexFingerprint {
                    provider: meta.provider.clone(),
                    model: meta.model.clone(),
                    dim: meta.dim,
                },
                current: expected_fp.clone(),
            });
        }

        // Task 2 carryover: guard against dim == 0 (corrupted / hand-edited meta).
        // A zero-dim index would silently accept any query, so we reject it early.
        if meta.dim == 0 {
            return Err(VectorIndexError::FingerprintMismatch {
                stored: VectorIndexFingerprint {
                    provider: meta.provider.clone(),
                    model: meta.model.clone(),
                    dim: meta.dim,
                },
                current: expected_fp.clone(),
            });
        }

        let opts = IndexOptions {
            dimensions: meta.dim,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            connectivity: 0,
            expansion_add: 0,
            expansion_search: 0,
            multi: false,
        };
        let index = Index::new(&opts).map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?;
        index
            .reserve(meta.row_count.max(8))
            .map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?;

        // Non-UTF8 path: return typed error rather than panic.
        let index_path_str = index_path
            .to_str()
            .ok_or_else(|| VectorIndexError::UsearchOp("non-utf8 sidecar path".to_string()))?;
        index
            .load(index_path_str)
            .map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?;

        Ok(Self {
            index: Arc::new(RwLock::new(index)),
            id_map: Arc::new(RwLock::new(meta.id_map)),
            fingerprint: VectorIndexFingerprint {
                provider: meta.provider,
                model: meta.model,
                dim: meta.dim,
            },
        })
    }
}
