use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::ffi::OsString;

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
    dirty: AtomicUsize,
    idx_path: Option<PathBuf>,
    meta_path: Option<PathBuf>,
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
            dirty: AtomicUsize::new(0),
            idx_path: None,
            meta_path: None,
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

    // ── Dirty-tracking and periodic save ────────────────────────────────────

    /// Increment the dirty counter and return the new value.
    pub fn dirty_count_increment(&self) -> usize {
        self.dirty.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Reset the dirty counter to zero (call after a successful save).
    pub fn dirty_count_reset(&self) {
        self.dirty.store(0, Ordering::Release);
    }

    /// Save to the paths that were set when the index was opened or rebuilt.
    ///
    /// Returns an error if the paths are not set (i.e. the index is in-memory only).
    pub async fn save_at_default_paths(&self) -> Result<(), VectorIndexError> {
        let idx_path = self
            .idx_path
            .clone()
            .ok_or_else(|| VectorIndexError::UsearchOp("save_at_default_paths needs known paths".into()))?;
        let meta_path = self
            .meta_path
            .clone()
            .ok_or_else(|| VectorIndexError::UsearchOp("save_at_default_paths needs known paths".into()))?;
        self.save_to(&idx_path, &meta_path).await
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

        // Sanity guard: dim == 0 means corrupted or hand-edited meta.
        // Must be checked first, before the fingerprint comparison, so it fires
        // even when expected_fp.dim == 0 (which the fingerprint check would pass).
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
            dirty: AtomicUsize::new(0),
            idx_path: Some(index_path.to_path_buf()),
            meta_path: Some(meta_path.to_path_buf()),
        })
    }

    /// Open the sidecar index from disk if it exists and is consistent with the
    /// database row count and fingerprint; otherwise rebuild from scratch by
    /// iterating every embedding row in `source`.
    ///
    /// Consistency checks (in order):
    /// 1. Both sidecar files (`<db>.usearch` and `<db>.usearch.meta.json`) must exist.
    /// 2. The stored fingerprint must match `expected_fp`.
    /// 3. The index row count must equal `source.count_total_memory_embeddings()`.
    ///
    /// Any failure triggers a full rebuild; the rebuilt index is persisted to the
    /// sidecar paths before returning so the next call can load directly.
    ///
    /// Lock strategy during rebuild: both write guards are acquired *per row*
    /// inside the `for_each_embedding` closure (id_map first, then index — same
    /// order as `upsert` and `remove`). No other thread touches `fresh` at this
    /// point, so per-row acquisition is safe and keeps the pattern consistent.
    pub async fn open_or_rebuild<S: EmbeddingRowSource>(
        source: &S,
        db_path: &Path,
        expected_fp: &VectorIndexFingerprint,
    ) -> Result<Self, VectorIndexError> {
        let (idx_path, meta_path) = sidecar_paths(db_path);

        let live_count = source
            .count_total_memory_embeddings()
            .map_err(|e| VectorIndexError::UsearchOp(format!("count failed: {e}")))?;

        if idx_path.exists() && meta_path.exists() {
            match Self::load_from(&idx_path, &meta_path, expected_fp).await {
                Ok(loaded) => {
                    let idx_count = loaded.size();
                    if idx_count as i64 == live_count {
                        tracing::info!(rows = idx_count, "loaded vector index from sidecar");
                        return Ok(loaded);
                    }
                    tracing::warn!(
                        index = idx_count,
                        db = live_count,
                        "vector index row count drift; rebuilding"
                    );
                }
                Err(VectorIndexError::FingerprintMismatch { stored, current }) => {
                    tracing::warn!(?stored, ?current, "vector index fingerprint mismatch; rebuilding");
                }
                Err(other) => {
                    tracing::warn!(error = %other, "vector index load failed; rebuilding");
                }
            }
        }

        let started = std::time::Instant::now();
        let mut fresh = VectorIndex::new_in_memory(
            expected_fp.dim,
            &expected_fp.provider,
            &expected_fp.model,
            (live_count as usize).max(8),
        );

        source
            .for_each_embedding(512, &mut |memory_id, blob| {
                let vec = decode_f32_blob(blob, expected_fp.dim)
                    .map_err(|e| super::StorageError::VectorIndex(format!("rebuild decode: {e}")))?;
                let key = memory_id_to_u64(memory_id);
                // Acquire id_map write first, then index write — consistent lock ordering.
                let mut id_map = fresh
                    .lock_id_map_write()
                    .map_err(|e| super::StorageError::VectorIndex(e.to_string()))?;
                let index = fresh
                    .lock_index_write()
                    .map_err(|e| super::StorageError::VectorIndex(e.to_string()))?;
                index
                    .add(key, &vec)
                    .map_err(|e| super::StorageError::VectorIndex(e.to_string()))?;
                id_map.insert(key, memory_id.to_string());
                Ok(())
            })
            .map_err(|e| VectorIndexError::UsearchOp(format!("rebuild iter: {e}")))?;

        if let Err(e) = fresh.save_to(&idx_path, &meta_path).await {
            try_cleanup_tmp_paths(&idx_path, &meta_path);
            return Err(e);
        }
        // save_to succeeded — record the paths so save_at_default_paths works.
        fresh.idx_path = Some(idx_path);
        fresh.meta_path = Some(meta_path);

        tracing::info!(
            rows = fresh.size(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "rebuilt vector index"
        );
        Ok(fresh)
    }
}

/// Compute the sidecar file paths for a given DuckDB path.
///
/// `<db>.usearch` holds the binary index; `<db>.usearch.meta.json` holds the
/// metadata (fingerprint, id_map, row_count). Both are written atomically via
/// `.tmp` siblings in `save_to`.
pub fn sidecar_paths(db_path: &Path) -> (PathBuf, PathBuf) {
    let mut idx: OsString = db_path.as_os_str().to_owned();
    idx.push(".usearch");
    let mut meta: OsString = db_path.as_os_str().to_owned();
    meta.push(".usearch.meta.json");
    (PathBuf::from(idx), PathBuf::from(meta))
}

/// Decode a raw little-endian (native-endian) f32 blob into a `Vec<f32>`.
///
/// Returns an error if `blob.len() != expected_len * 4`.
fn decode_f32_blob(blob: &[u8], expected_len: usize) -> Result<Vec<f32>, String> {
    let expected_bytes = expected_len.checked_mul(4).ok_or("dim overflow")?;
    if blob.len() != expected_bytes {
        return Err(format!("blob length {} expected {}", blob.len(), expected_bytes));
    }
    let mut out = Vec::with_capacity(expected_len);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

/// Best-effort removal of the `.tmp` sidecar files left behind by a failed
/// `save_to`. Logs warnings on unexpected errors but never propagates them.
fn try_cleanup_tmp_paths(idx_path: &Path, meta_path: &Path) {
    let mut idx_tmp: OsString = idx_path.as_os_str().to_owned();
    idx_tmp.push(".tmp");
    let mut meta_tmp: OsString = meta_path.as_os_str().to_owned();
    meta_tmp.push(".tmp");
    if let Err(e) = std::fs::remove_file(&idx_tmp) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = ?idx_tmp, error = %e, "failed to clean up tmp index");
        }
    }
    if let Err(e) = std::fs::remove_file(&meta_tmp) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = ?meta_tmp, error = %e, "failed to clean up tmp meta");
        }
    }
}
