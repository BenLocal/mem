# Vector Index Sidecar (usearch) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the linear cosine scan in `DuckDbRepository::semantic_search_memories` with a `usearch` HNSW sidecar index, eliminating both the O(N) scaling and the silent `LIMIT 2000` truncation. DuckDB stays authoritative; the sidecar is rebuildable from it.

**Architecture:** A new module `src/storage/vector_index.rs` owns an `Arc<RwLock<usearch::Index>>` plus a `HashMap<u64, String>` for `memory_id` reverse lookup. Two files (`<MEM_DB_PATH>.usearch` + `<MEM_DB_PATH>.usearch.meta.json`) persist alongside the DuckDB database. Startup runs `open_or_rebuild`: load both files; if missing, fingerprint-mismatched, or row-count-mismatched against `memory_embeddings`, rebuild from DuckDB. The embedding worker mirrors every `upsert_memory_embedding` and every delete site to the index. Search re-fetches rows from DuckDB by `memory_id` after ANN recall, applying tenant/status filters and re-attaching cosine scores.

**Tech Stack:** Rust, axum, tokio, DuckDB (bundled), `usearch` crate (C++ binding), `sha2` (already a dep, for `memory_id → u64`).

**Spec:** `docs/superpowers/specs/2026-04-27-vector-index-sidecar-design.md`

---

## File Structure

**Create:**
- `src/storage/vector_index.rs` — `VectorIndex` struct, `EmbeddingRowSource` trait, fingerprint, save/load, open_or_rebuild
- `tests/vector_index.rs` — unit + integration tests for the new module
- `tests/common/storage.rs` (only if not already present) — small helper for opening repo + index in tempdir

**Modify:**
- `Cargo.toml` — add `usearch = "2"`
- `src/storage/mod.rs` — re-export `VectorIndex` and `VectorIndexError`
- `src/storage/duckdb.rs` — add `count_total_memory_embeddings`, `iter_memory_embeddings`, `fetch_memories_by_ids`; rewrite body of `semantic_search_memories`; preserve `legacy_semantic_search_memories`
- `src/service/embedding_worker.rs` — call `vector_index.upsert` after the existing `upsert_memory_embedding`
- `src/service/memory_service.rs` — accept `Arc<VectorIndex>` in constructor; mirror `vector_index.remove` at every delete site
- `src/app.rs` — `VectorIndex::open_or_rebuild` in startup; inject into service + worker
- `src/config.rs` — three new env vars
- `tests/embedding_worker.rs`, `tests/search_api.rs`, `tests/hybrid_search.rs` — assertions that index size moves with embedding row count
- `docs/mempalace-diff.md` — once green, mark §8 row #3 ✅

---

## Task 1: Add `usearch` dependency

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add the dep**

In `Cargo.toml` under `[dependencies]`, add:

```toml
usearch = "2"
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build`
Expected: clean build, no warnings about `usearch`. Cargo.lock updated.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add usearch crate for ANN sidecar (mempalace-diff §8 #3)"
```

---

## Task 2: Skeleton `vector_index` module + `EmbeddingRowSource` trait

**Files:**
- Create: `src/storage/vector_index.rs`
- Modify: `src/storage/mod.rs`
- Test: `tests/vector_index.rs` (new)

- [ ] **Step 1: Write the failing test**

Create `tests/vector_index.rs` with this content:

```rust
use mem::storage::vector_index::{EmbeddingRowSource, VectorIndex};

struct EmptySource;

impl EmbeddingRowSource for EmptySource {
    fn count_total_memory_embeddings(&self) -> Result<i64, mem::storage::StorageError> {
        Ok(0)
    }
    fn for_each_embedding(
        &self,
        _batch: usize,
        _f: &mut dyn FnMut(&str, &[u8]) -> Result<(), mem::storage::StorageError>,
    ) -> Result<(), mem::storage::StorageError> {
        Ok(())
    }
}

#[tokio::test]
async fn vector_index_starts_empty() {
    let idx = VectorIndex::new_in_memory(256, "fake", "fake", 256);
    assert_eq!(idx.size(), 0);
}
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test --test vector_index -q`
Expected: compile error (`VectorIndex` undefined, `vector_index` module unknown).

- [ ] **Step 3: Implement skeleton**

Create `src/storage/vector_index.rs`:

```rust
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
```

Modify `src/storage/mod.rs`. Find the existing module declarations and add:

```rust
pub mod vector_index;

pub use vector_index::{EmbeddingRowSource, VectorIndex, VectorIndexError, VectorIndexFingerprint};
```

**Add a new variant to `StorageError`** in `src/storage/duckdb.rs` (find `pub enum StorageError`):

```rust
#[error("vector index error: {0}")]
VectorIndex(String),
```

This avoids the `&'static str` constraint of `InvalidData` for runtime errors flowing out of `vector_index.rs`. Subsequent tasks (9, 14) construct `StorageError::VectorIndex(e.to_string())` directly. Also add `use tracing;` in `vector_index.rs` if not already pulled in by the trait.

- [ ] **Step 4: Run test to verify pass**

Run: `cargo test --test vector_index -q`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index.rs src/storage/mod.rs tests/vector_index.rs
git commit -m "feat(storage): scaffold VectorIndex + EmbeddingRowSource trait"
```

---

## Task 3: `upsert` + `search` round-trip

**Files:**
- Modify: `src/storage/vector_index.rs`
- Modify: `tests/vector_index.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/vector_index.rs`:

```rust
fn unit_vector(dim: usize, seed: u8) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    v[seed as usize % dim] = 1.0;
    v
}

#[tokio::test]
async fn upsert_then_search_returns_inserted_memory_id() {
    let idx = VectorIndex::new_in_memory(256, "fake", "fake", 16);
    idx.upsert("mem_a", &unit_vector(256, 1)).await.unwrap();
    idx.upsert("mem_b", &unit_vector(256, 2)).await.unwrap();
    idx.upsert("mem_c", &unit_vector(256, 3)).await.unwrap();

    let hits = idx.search(&unit_vector(256, 2), 1).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "mem_b");
    assert!(hits[0].1 > 0.99);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test vector_index upsert_then_search -q`
Expected: compile error (`upsert` and `search` not defined).

- [ ] **Step 3: Implement `upsert` and `search`**

In `src/storage/vector_index.rs`, add a key-hashing helper and the two methods:

```rust
use sha2::{Digest, Sha256};

fn memory_id_to_u64(memory_id: &str) -> u64 {
    let digest = Sha256::digest(memory_id.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

impl VectorIndex {
    pub async fn upsert(&self, memory_id: &str, embedding: &[f32]) -> Result<(), VectorIndexError> {
        if embedding.len() != self.fingerprint.dim {
            return Err(VectorIndexError::UsearchOp(format!(
                "embedding length {} does not match fingerprint dim {}",
                embedding.len(),
                self.fingerprint.dim
            )));
        }
        let key = memory_id_to_u64(memory_id);
        let mut id_map = self.id_map.write().expect("id_map poisoned");
        if let Some(existing) = id_map.get(&key) {
            if existing != memory_id {
                return Err(VectorIndexError::HashCollision {
                    existing: existing.clone(),
                    incoming: memory_id.to_string(),
                });
            }
        }
        let index = self.index.write().expect("index poisoned");
        // remove is idempotent; ignore "not found" semantics
        let _ = index.remove(key);
        index
            .add(key, embedding)
            .map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?;
        id_map.insert(key, memory_id.to_string());
        Ok(())
    }

    pub async fn search(
        &self,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(String, f32)>, VectorIndexError> {
        if query.len() != self.fingerprint.dim || k == 0 {
            return Ok(vec![]);
        }
        let index = self.index.read().expect("index poisoned");
        if index.size() == 0 {
            return Ok(vec![]);
        }
        let matches = index
            .search(query, k)
            .map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?;
        drop(index);

        let id_map = self.id_map.read().expect("id_map poisoned");
        let mut out = Vec::with_capacity(matches.keys.len());
        for (i, key) in matches.keys.iter().enumerate() {
            if let Some(id) = id_map.get(key) {
                let dist = matches.distances[i];
                // usearch cosine returns "distance" = 1 - cosine_similarity for Cos metric
                let sim = 1.0 - dist;
                out.push((id.clone(), sim));
            }
        }
        Ok(out)
    }
}
```

> Note for the implementer: `usearch::Matches` exact field names may differ across crate versions. If `matches.keys` / `matches.distances` are wrong, consult `cargo doc --open --package usearch` and adjust. Cosine "distance" vs "similarity" is the most common gotcha — verify the unit-vector test gets `sim > 0.99` for an exact match.

- [ ] **Step 4: Run test to verify pass**

Run: `cargo test --test vector_index -q`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index.rs tests/vector_index.rs
git commit -m "feat(vector_index): upsert + search with sha2-based u64 keying"
```

---

## Task 4: Overwrite semantics + `remove`

**Files:**
- Modify: `src/storage/vector_index.rs`
- Modify: `tests/vector_index.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/vector_index.rs`:

```rust
#[tokio::test]
async fn upsert_overwrites_previous_vector_for_same_memory_id() {
    let idx = VectorIndex::new_in_memory(256, "fake", "fake", 8);
    idx.upsert("mem_x", &unit_vector(256, 1)).await.unwrap();
    idx.upsert("mem_x", &unit_vector(256, 5)).await.unwrap();

    let hit_old = idx.search(&unit_vector(256, 1), 1).await.unwrap();
    let hit_new = idx.search(&unit_vector(256, 5), 1).await.unwrap();
    // After overwrite, the "old" query should still find mem_x (it's the only row)
    // but with low similarity; the "new" query should match strongly.
    assert_eq!(hit_new[0].0, "mem_x");
    assert!(hit_new[0].1 > 0.99);
    assert!(hit_old[0].1 < 0.5);
    assert_eq!(idx.size(), 1);
}

#[tokio::test]
async fn remove_makes_search_skip_the_id() {
    let idx = VectorIndex::new_in_memory(256, "fake", "fake", 8);
    idx.upsert("mem_keep", &unit_vector(256, 1)).await.unwrap();
    idx.upsert("mem_drop", &unit_vector(256, 2)).await.unwrap();
    idx.remove("mem_drop").await.unwrap();

    let hits = idx.search(&unit_vector(256, 2), 5).await.unwrap();
    assert!(hits.iter().all(|(id, _)| id != "mem_drop"));
    assert_eq!(idx.size(), 1);
}

#[tokio::test]
async fn remove_unknown_id_is_noop() {
    let idx = VectorIndex::new_in_memory(256, "fake", "fake", 4);
    idx.remove("never_inserted").await.unwrap();
    assert_eq!(idx.size(), 0);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test vector_index remove -q`
Expected: compile error (`remove` not defined).

- [ ] **Step 3: Implement `remove`**

In `src/storage/vector_index.rs`, add to the `impl VectorIndex` block:

```rust
impl VectorIndex {
    pub async fn remove(&self, memory_id: &str) -> Result<(), VectorIndexError> {
        let key = memory_id_to_u64(memory_id);
        let mut id_map = self.id_map.write().expect("id_map poisoned");
        let index = self.index.write().expect("index poisoned");
        // usearch::Index::remove returns the count removed; treat all as ok
        let _ = index.remove(key);
        id_map.remove(&key);
        Ok(())
    }
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo test --test vector_index -q`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index.rs tests/vector_index.rs
git commit -m "feat(vector_index): remove + overwrite semantics"
```

---

## Task 5: Meta JSON struct + serde

**Files:**
- Modify: `src/storage/vector_index.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/vector_index.rs`:

```rust
use mem::storage::vector_index::VectorIndexMeta;

#[test]
fn meta_round_trips_through_json() {
    let meta = VectorIndexMeta {
        schema_version: 1,
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dim: 1536,
        row_count: 42,
        id_map: vec![(123u64, "mem_alpha".into()), (456u64, "mem_beta".into())]
            .into_iter()
            .collect(),
    };
    let s = serde_json::to_string(&meta).unwrap();
    let back: VectorIndexMeta = serde_json::from_str(&s).unwrap();
    assert_eq!(back.schema_version, 1);
    assert_eq!(back.provider, "openai");
    assert_eq!(back.row_count, 42);
    assert_eq!(back.id_map.len(), 2);
    assert_eq!(back.id_map.get(&123u64).unwrap(), "mem_alpha");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test vector_index meta_round -q`
Expected: compile error (`VectorIndexMeta` not defined).

- [ ] **Step 3: Implement `VectorIndexMeta`**

In `src/storage/vector_index.rs`, add at module top:

```rust
use serde::{Deserialize, Serialize};

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
    use serde::{Deserializer, Serializer, Deserialize};

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
```

Re-export from `src/storage/mod.rs`:

```rust
pub use vector_index::{
    EmbeddingRowSource, VectorIndex, VectorIndexError, VectorIndexFingerprint, VectorIndexMeta,
};
```

- [ ] **Step 4: Run test to verify pass**

Run: `cargo test --test vector_index meta -q`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index.rs src/storage/mod.rs tests/vector_index.rs
git commit -m "feat(vector_index): VectorIndexMeta with u64-keyed id_map serde"
```

---

## Task 6: `save` to disk with atomic rename

**Files:**
- Modify: `src/storage/vector_index.rs`
- Modify: `tests/vector_index.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/vector_index.rs`:

```rust
use tempfile::tempdir;

#[tokio::test]
async fn save_writes_both_files_atomically() {
    let dir = tempdir().unwrap();
    let idx_path = dir.path().join("test.usearch");
    let meta_path = dir.path().join("test.usearch.meta.json");

    let idx = VectorIndex::new_in_memory(256, "fake", "fake", 8);
    idx.upsert("mem_alpha", &unit_vector(256, 1)).await.unwrap();
    idx.save_to(&idx_path, &meta_path).await.unwrap();

    assert!(idx_path.exists());
    assert!(meta_path.exists());

    let meta_str = std::fs::read_to_string(&meta_path).unwrap();
    let meta: mem::storage::VectorIndexMeta = serde_json::from_str(&meta_str).unwrap();
    assert_eq!(meta.row_count, 1);
    assert_eq!(meta.dim, 256);
    assert_eq!(meta.provider, "fake");
    assert_eq!(meta.id_map.len(), 1);
    assert_eq!(meta.id_map.values().next().unwrap(), "mem_alpha");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test vector_index save_writes -q`
Expected: compile error (`save_to` not defined).

- [ ] **Step 3: Implement `save_to`**

Append to `impl VectorIndex` in `src/storage/vector_index.rs`:

```rust
use std::path::{Path, PathBuf};
use std::fs;

impl VectorIndex {
    pub async fn save_to(
        &self,
        index_path: &Path,
        meta_path: &Path,
    ) -> Result<(), VectorIndexError> {
        // Build meta first so we capture an internally-consistent snapshot
        let id_map = self.id_map.read().expect("id_map poisoned");
        let index = self.index.read().expect("index poisoned");
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

        let tmp_index: PathBuf = index_path.with_extension("usearch.tmp");
        let tmp_meta: PathBuf = meta_path.with_extension("meta.json.tmp");

        index
            .save(tmp_index.to_str().expect("path utf8"))
            .map_err(|e| VectorIndexError::UsearchOp(e.to_string()))?;

        fs::write(&tmp_meta, serde_json::to_vec_pretty(&meta)?)?;

        fs::rename(&tmp_index, index_path)?;
        fs::rename(&tmp_meta, meta_path)?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo test --test vector_index -q`
Expected: 7 passed (5 prior + 1 meta + 1 save).

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index.rs tests/vector_index.rs
git commit -m "feat(vector_index): save_to with tempfile + atomic rename"
```

---

## Task 7: `load_from` with fingerprint validation

**Files:**
- Modify: `src/storage/vector_index.rs`
- Modify: `tests/vector_index.rs`

- [ ] **Step 1: Write the failing tests**

Append:

```rust
#[tokio::test]
async fn load_round_trips_save() {
    let dir = tempdir().unwrap();
    let idx_path = dir.path().join("rt.usearch");
    let meta_path = dir.path().join("rt.usearch.meta.json");

    let original = VectorIndex::new_in_memory(256, "fake", "fake", 8);
    original.upsert("mem_a", &unit_vector(256, 1)).await.unwrap();
    original.upsert("mem_b", &unit_vector(256, 2)).await.unwrap();
    original.save_to(&idx_path, &meta_path).await.unwrap();

    let loaded = VectorIndex::load_from(
        &idx_path,
        &meta_path,
        &mem::storage::VectorIndexFingerprint {
            provider: "fake".into(),
            model: "fake".into(),
            dim: 256,
        },
    )
    .await
    .unwrap();

    assert_eq!(loaded.size(), 2);
    let hits = loaded.search(&unit_vector(256, 2), 1).await.unwrap();
    assert_eq!(hits[0].0, "mem_b");
}

#[tokio::test]
async fn load_rejects_fingerprint_mismatch() {
    let dir = tempdir().unwrap();
    let idx_path = dir.path().join("fp.usearch");
    let meta_path = dir.path().join("fp.usearch.meta.json");

    let original = VectorIndex::new_in_memory(256, "fake", "fake", 4);
    original.upsert("mem_a", &unit_vector(256, 1)).await.unwrap();
    original.save_to(&idx_path, &meta_path).await.unwrap();

    let err = VectorIndex::load_from(
        &idx_path,
        &meta_path,
        &mem::storage::VectorIndexFingerprint {
            provider: "fake".into(),
            model: "fake".into(),
            dim: 128,
        },
    )
    .await
    .unwrap_err();
    matches!(err, mem::storage::VectorIndexError::FingerprintMismatch { .. });
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test vector_index load_ -q`
Expected: compile error (`load_from` not defined).

- [ ] **Step 3: Implement `load_from`**

Append to `impl VectorIndex`:

```rust
impl VectorIndex {
    pub async fn load_from(
        index_path: &Path,
        meta_path: &Path,
        expected_fp: &VectorIndexFingerprint,
    ) -> Result<Self, VectorIndexError> {
        let meta_bytes = fs::read(meta_path)?;
        let meta: VectorIndexMeta = serde_json::from_slice(&meta_bytes)?;

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
        index
            .load(index_path.to_str().expect("path utf8"))
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
```

Add `Clone` derive on `VectorIndexFingerprint`:

```rust
#[derive(Debug, Clone)]
pub struct VectorIndexFingerprint {
```

- [ ] **Step 4: Run all tests**

Run: `cargo test --test vector_index -q`
Expected: 9 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index.rs tests/vector_index.rs
git commit -m "feat(vector_index): load_from with fingerprint validation"
```

---

## Task 8: `DuckDbRepository` — `count_total_memory_embeddings` + `iter_memory_embeddings`

**Files:**
- Modify: `src/storage/duckdb.rs`
- Modify: `tests/embedding_jobs.rs` (add a quick assertion test) **OR** new file

- [ ] **Step 1: Write the failing test**

Append to `tests/embedding_jobs.rs` a new top-level test (use existing `base()` helpers if present; otherwise create minimal scaffolding):

```rust
#[tokio::test]
async fn count_total_memory_embeddings_returns_zero_for_empty_db() {
    use tempfile::tempdir;
    let dir = tempdir().unwrap();
    let db = dir.path().join("count-empty.duckdb");
    let repo = mem::storage::DuckDbRepository::open(&db).await.unwrap();
    assert_eq!(repo.count_total_memory_embeddings().await.unwrap(), 0);
}
```

Append a separate test for the iterator:

```rust
#[tokio::test]
async fn iter_memory_embeddings_visits_each_row() {
    use mem::storage::EmbeddingRowSource;
    use tempfile::tempdir;
    let dir = tempdir().unwrap();
    let db = dir.path().join("iter.duckdb");
    let repo = mem::storage::DuckDbRepository::open(&db).await.unwrap();
    // Direct seed: insert two memories + two memory_embeddings rows via raw SQL helper.
    repo.seed_memory_embedding_for_test("mem_a", "tenant-x", &[1.0, 0.0])
        .await
        .unwrap();
    repo.seed_memory_embedding_for_test("mem_b", "tenant-x", &[0.0, 1.0])
        .await
        .unwrap();

    let mut seen = Vec::new();
    repo.for_each_embedding(
        100,
        &mut |id, blob| {
            seen.push((id.to_string(), blob.to_vec()));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(seen.len(), 2);
    let ids: std::collections::HashSet<_> = seen.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains("mem_a"));
    assert!(ids.contains("mem_b"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test embedding_jobs count_total_ -q`
Expected: compile error.

- [ ] **Step 3: Implement on `DuckDbRepository`**

In `src/storage/duckdb.rs` add the public methods plus the test seeding helper. Find a sensible neighbor to the existing `count_memory_embeddings_for_memory` definition and add:

```rust
impl DuckDbRepository {
    pub async fn count_total_memory_embeddings(&self) -> Result<i64, StorageError> {
        let conn = self.conn()?;
        let count: i64 = conn.query_row(
            "select count(*) from memory_embeddings",
            params![],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Test-only seed used by integration tests that bypass the worker.
    #[doc(hidden)]
    pub async fn seed_memory_embedding_for_test(
        &self,
        memory_id: &str,
        tenant: &str,
        vec: &[f32],
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        let now = current_timestamp();
        let mut blob = Vec::with_capacity(vec.len() * 4);
        for v in vec {
            blob.extend_from_slice(&v.to_ne_bytes());
        }
        conn.execute(
            "insert or replace into memory_embeddings(
                memory_id, tenant, embedding_model, embedding_dim, embedding,
                content_hash, source_updated_at, created_at, updated_at
            ) values (?1, ?2, 'fake', ?3, ?4, 'seed', ?5, ?5, ?5)",
            params![memory_id, tenant, vec.len() as i64, blob, now],
        )?;
        // also insert a placeholder memories row so foreign keys succeed if added later;
        // for now memory_embeddings has no FK enforcement (DuckDB bundled), so this is a no-op safeguard.
        Ok(())
    }
}

impl EmbeddingRowSource for DuckDbRepository {
    fn count_total_memory_embeddings(&self) -> Result<i64, StorageError> {
        // sync wrapper over the async accessor — re-implement here without await
        let conn = self.conn()?;
        let count: i64 = conn.query_row(
            "select count(*) from memory_embeddings",
            params![],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    fn for_each_embedding(
        &self,
        _batch: usize,
        f: &mut dyn FnMut(&str, &[u8]) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select memory_id, embedding from memory_embeddings order by memory_id",
        )?;
        let mut rows = stmt.query(params![])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            f(&id, &blob)?;
        }
        Ok(())
    }
}
```

> Note: schema 002 declares `memory_embeddings.memory_id REFERENCES memories(memory_id)`, so a strict run will require a corresponding `memories` row. The simpler path is for `seed_memory_embedding_for_test` to also insert a minimal `memories` row first; if integration tests fail at this step, extend `seed_memory_embedding_for_test` to insert into `memories` with a placeholder row before the embedding row.

- [ ] **Step 4: Run tests**

Run: `cargo test --test embedding_jobs -q`
Expected: all prior tests + 2 new pass.

- [ ] **Step 5: Commit**

```bash
git add src/storage/duckdb.rs tests/embedding_jobs.rs
git commit -m "feat(storage): EmbeddingRowSource impl on DuckDbRepository"
```

---

## Task 9: `VectorIndex::open_or_rebuild`

**Files:**
- Modify: `src/storage/vector_index.rs`
- Modify: `tests/vector_index.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/vector_index.rs`:

```rust
use mem::storage::DuckDbRepository;

#[tokio::test]
async fn open_or_rebuild_returns_empty_against_fresh_db() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("open-empty.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let fp = mem::storage::VectorIndexFingerprint {
        provider: "fake".into(),
        model: "fake".into(),
        dim: 256,
    };
    let idx = VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap();
    assert_eq!(idx.size(), 0);
}

#[tokio::test]
async fn open_or_rebuild_rebuilds_when_no_sidecar_files_exist() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("rebuild.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.seed_memory_embedding_for_test("m1", "t", &unit_vector_owned(256, 1))
        .await
        .unwrap();
    repo.seed_memory_embedding_for_test("m2", "t", &unit_vector_owned(256, 2))
        .await
        .unwrap();

    let fp = mem::storage::VectorIndexFingerprint {
        provider: "fake".into(),
        model: "fake".into(),
        dim: 256,
    };
    let idx = VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap();
    assert_eq!(idx.size(), 2);
}

#[tokio::test]
async fn open_or_rebuild_rebuilds_on_row_count_drift() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("drift.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.seed_memory_embedding_for_test("m1", "t", &unit_vector_owned(256, 1))
        .await
        .unwrap();

    let fp = mem::storage::VectorIndexFingerprint {
        provider: "fake".into(),
        model: "fake".into(),
        dim: 256,
    };
    // First call: builds + saves
    let idx_a = VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap();
    assert_eq!(idx_a.size(), 1);
    drop(idx_a);

    // Bypass the index: add a new row directly
    repo.seed_memory_embedding_for_test("m2", "t", &unit_vector_owned(256, 2))
        .await
        .unwrap();

    // Second call: should detect mismatch and rebuild
    let idx_b = VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap();
    assert_eq!(idx_b.size(), 2);
}

fn unit_vector_owned(dim: usize, seed: u8) -> Vec<f32> {
    unit_vector(dim, seed)
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test vector_index open_or_rebuild_ -q`
Expected: compile error (`open_or_rebuild` undefined).

- [ ] **Step 3: Implement `open_or_rebuild`**

Append to `impl VectorIndex` in `src/storage/vector_index.rs`:

```rust
impl VectorIndex {
    /// Open the sidecar at `<db_path>.usearch[.meta.json]`, validating it
    /// against `expected_fp` and the live row count. On any inconsistency,
    /// rebuild from `source` and atomically write fresh files.
    pub async fn open_or_rebuild<S: EmbeddingRowSource>(
        source: &S,
        db_path: &Path,
        expected_fp: &VectorIndexFingerprint,
    ) -> Result<Self, VectorIndexError> {
        let (idx_path, meta_path) = sidecar_paths(db_path);

        let live_count = source
            .count_total_memory_embeddings()
            .map_err(|e| VectorIndexError::UsearchOp(format!("count failed: {e}")))?;

        // Try the happy path: load existing files
        if idx_path.exists() && meta_path.exists() {
            match Self::load_from(&idx_path, &meta_path, expected_fp).await {
                Ok(loaded) => {
                    let idx_count = loaded.size();
                    if idx_count as i64 == live_count {
                        tracing::info!(
                            rows = idx_count,
                            "loaded vector index from sidecar"
                        );
                        return Ok(loaded);
                    }
                    tracing::warn!(
                        index = idx_count,
                        db = live_count,
                        "vector index row count drift; rebuilding"
                    );
                }
                Err(VectorIndexError::FingerprintMismatch { stored, current }) => {
                    tracing::warn!(
                        ?stored,
                        ?current,
                        "vector index fingerprint mismatch; rebuilding"
                    );
                }
                Err(other) => {
                    tracing::warn!(error = %other, "vector index load failed; rebuilding");
                }
            }
        }

        // Rebuild path
        let started = std::time::Instant::now();
        let fresh = VectorIndex::new_in_memory(
            expected_fp.dim,
            &expected_fp.provider,
            &expected_fp.model,
            (live_count as usize).max(8),
        );
        source
            .for_each_embedding(512, &mut |memory_id, blob| {
                let vec = decode_f32_blob(blob, expected_fp.dim)
                    .map_err(|e| StorageError::VectorIndex(format!("rebuild decode: {e}")))?;
                let key = memory_id_to_u64(memory_id);
                let index = fresh.index.write().expect("index poisoned");
                index
                    .add(key, &vec)
                    .map_err(|e| StorageError::VectorIndex(e.to_string()))?;
                drop(index);
                let mut id_map = fresh.id_map.write().expect("id_map poisoned");
                id_map.insert(key, memory_id.to_string());
                Ok(())
            })
            .map_err(|e| VectorIndexError::UsearchOp(format!("rebuild iter: {e}")))?;

        fresh.save_to(&idx_path, &meta_path).await?;
        tracing::info!(
            rows = fresh.size(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "rebuilt vector index"
        );
        Ok(fresh)
    }
}

pub fn sidecar_paths(db_path: &Path) -> (PathBuf, PathBuf) {
    let mut idx = db_path.as_os_str().to_owned();
    idx.push(".usearch");
    let mut meta = db_path.as_os_str().to_owned();
    meta.push(".usearch.meta.json");
    (PathBuf::from(idx), PathBuf::from(meta))
}

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
```

> Note: the `Box::leak` for `StorageError::InvalidData` is a hack to fit the existing error type without changing it. If the error variant accepts an owned String, switch to that. Keep the spirit: rebuild errors must surface as `StorageError`.

- [ ] **Step 4: Run tests**

Run: `cargo test --test vector_index open_or_rebuild_ -q`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index.rs tests/vector_index.rs
git commit -m "feat(vector_index): open_or_rebuild with row_count + fingerprint check"
```

---

## Task 10: Corruption + missing-file rebuild paths

**Files:**
- Modify: `tests/vector_index.rs`

- [ ] **Step 1: Write the tests**

Append:

```rust
#[tokio::test]
async fn open_or_rebuild_recovers_from_corrupt_index_file() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("corrupt.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.seed_memory_embedding_for_test("m1", "t", &unit_vector_owned(256, 1))
        .await
        .unwrap();
    let fp = mem::storage::VectorIndexFingerprint {
        provider: "fake".into(),
        model: "fake".into(),
        dim: 256,
    };
    // First open: writes sidecar
    let _ = VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap();

    // Corrupt the index file
    let (idx_path, _) = mem::storage::vector_index::sidecar_paths(&db);
    std::fs::write(&idx_path, b"GARBAGE").unwrap();

    // Second open: detects corruption, rebuilds from DuckDB
    let idx = VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap();
    assert_eq!(idx.size(), 1);
}

#[tokio::test]
async fn open_or_rebuild_recovers_from_missing_meta_file() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("missing-meta.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.seed_memory_embedding_for_test("m1", "t", &unit_vector_owned(256, 1))
        .await
        .unwrap();
    let fp = mem::storage::VectorIndexFingerprint {
        provider: "fake".into(),
        model: "fake".into(),
        dim: 256,
    };
    let _ = VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap();
    let (_, meta_path) = mem::storage::vector_index::sidecar_paths(&db);
    std::fs::remove_file(&meta_path).unwrap();

    let idx = VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap();
    assert_eq!(idx.size(), 1);
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test vector_index recovers_ -q`
Expected: 2 passed (load_from already returns errors that open_or_rebuild swallows-and-rebuilds).

- [ ] **Step 3: Commit**

```bash
git add tests/vector_index.rs
git commit -m "test(vector_index): cover corrupt + missing meta rebuild paths"
```

---

## Task 11: `DuckDbRepository::fetch_memories_by_ids`

**Files:**
- Modify: `src/storage/duckdb.rs`
- Modify: `tests/embedding_jobs.rs` (or new `tests/storage_fetch_by_ids.rs`)

- [ ] **Step 1: Write the failing test**

Add a new file `tests/storage_fetch_by_ids.rs`:

```rust
use mem::domain::memory::{
    IngestMemoryRequest, MemoryStatus, MemoryType, Scope, Visibility, WriteMode,
};
use mem::service::MemoryService;
use mem::storage::DuckDbRepository;
use tempfile::tempdir;

#[tokio::test]
async fn fetch_by_ids_filters_tenant_and_status() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("fetch.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let svc = MemoryService::new(repo.clone());

    let make = |tenant: &str, content: &str| IngestMemoryRequest {
        tenant: tenant.into(),
        memory_type: MemoryType::Implementation,
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: None,
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    };
    let a = svc.ingest(make("ten-a", "alpha")).await.unwrap();
    let b = svc.ingest(make("ten-b", "beta")).await.unwrap();
    let c = svc.ingest(make("ten-a", "gamma")).await.unwrap();

    let ids = vec![a.memory_id.as_str(), b.memory_id.as_str(), c.memory_id.as_str()];
    let rows = repo
        .fetch_memories_by_ids("ten-a", &ids)
        .await
        .unwrap();

    let returned: std::collections::HashSet<_> =
        rows.iter().map(|m| m.memory_id.as_str()).collect();
    assert!(returned.contains(a.memory_id.as_str()));
    assert!(returned.contains(c.memory_id.as_str()));
    assert!(!returned.contains(b.memory_id.as_str())); // wrong tenant
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test storage_fetch_by_ids -q`
Expected: compile error (`fetch_memories_by_ids` undefined).

- [ ] **Step 3: Implement on `DuckDbRepository`**

Add in `src/storage/duckdb.rs` near `semantic_search_memories`:

```rust
impl DuckDbRepository {
    /// Returns `MemoryRecord` rows for the given ids, filtered to a tenant and
    /// the standard "live" status set. Used by the rewritten `semantic_search_memories`.
    pub async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn()?;

        // Build "?, ?, ?" placeholder list dynamically
        let placeholders = std::iter::repeat("?")
            .take(ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "select
                memory_id, tenant, memory_type, status, scope, visibility, version,
                summary, content, evidence_json, code_refs_json, project, repo,
                module, task_type, tags_json, confidence, decay_score, content_hash,
                idempotency_key, supersedes_memory_id, source_agent, created_at,
                updated_at, last_validated_at
             from memories
             where tenant = ?1
               and status not in ('rejected', 'archived')
               and memory_id in ({placeholders})"
        );

        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn duckdb::ToSql>> =
            vec![Box::new(tenant.to_string())];
        for id in ids {
            params_vec.push(Box::new(id.to_string()));
        }
        let params_refs: Vec<&dyn duckdb::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();

        let rows = stmt.query_map(&params_refs[..], map_memory_row)?;
        let mut out = Vec::with_capacity(ids.len());
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
```

> Note: `map_memory_row` is the existing row-mapping closure used by other read methods. If the codebase uses a different name (e.g. `memory_from_row`), use that. Inline-grep before writing the call.

- [ ] **Step 4: Run**

Run: `cargo test --test storage_fetch_by_ids -q`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/duckdb.rs tests/storage_fetch_by_ids.rs
git commit -m "feat(storage): fetch_memories_by_ids for ANN post-filter step"
```

---

## Task 12: Three new env vars in `Config`

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**

Append to the `mod tests` block in `src/config.rs`:

```rust
#[test]
fn vector_index_settings_have_defaults() {
    let s = EmbeddingSettings::from_env_vars(|_| None).unwrap();
    assert_eq!(s.vector_index_flush_every, 100);
    assert_eq!(s.vector_index_oversample, 4);
    assert!(!s.vector_index_use_legacy);
}

#[test]
fn vector_index_settings_read_from_env() {
    let s = EmbeddingSettings::from_env_vars(env(&[
        ("MEM_VECTOR_INDEX_FLUSH_EVERY", "50"),
        ("MEM_VECTOR_INDEX_OVERSAMPLE", "8"),
        ("MEM_VECTOR_INDEX_USE_LEGACY", "1"),
    ]))
    .unwrap();
    assert_eq!(s.vector_index_flush_every, 50);
    assert_eq!(s.vector_index_oversample, 8);
    assert!(s.vector_index_use_legacy);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib config -q`
Expected: compile error (fields unknown).

- [ ] **Step 3: Implement env-var fields**

In `src/config.rs`, extend `EmbeddingSettings`:

```rust
pub struct EmbeddingSettings {
    // ... existing fields ...
    pub vector_index_flush_every: usize,
    pub vector_index_oversample: usize,
    pub vector_index_use_legacy: bool,
}
```

In `development_defaults`:

```rust
vector_index_flush_every: 100,
vector_index_oversample: 4,
vector_index_use_legacy: false,
```

In `from_env_vars` (after the existing parsing block, before `Ok(s)`):

```rust
if let Some(raw) = get("MEM_VECTOR_INDEX_FLUSH_EVERY") {
    let n: usize = raw.parse().map_err(|_| {
        ConfigError::InvalidEmbeddingDim(format!("flush_every: {raw}"))
    })?;
    if n == 0 {
        return Err(ConfigError::InvalidEmbeddingDim("flush_every=0".into()));
    }
    s.vector_index_flush_every = n;
}
if let Some(raw) = get("MEM_VECTOR_INDEX_OVERSAMPLE") {
    let n: usize = raw.parse().map_err(|_| {
        ConfigError::InvalidEmbeddingDim(format!("oversample: {raw}"))
    })?;
    if n == 0 {
        return Err(ConfigError::InvalidEmbeddingDim("oversample=0".into()));
    }
    s.vector_index_oversample = n;
}
if let Some(raw) = get("MEM_VECTOR_INDEX_USE_LEGACY") {
    s.vector_index_use_legacy = matches!(raw.as_str(), "1" | "true" | "yes");
}
```

> Note: `ConfigError` reuse for these settings is pragmatic; if you prefer to add a dedicated `InvalidVectorIndex` variant, do so — minor scope creep but cleaner.

- [ ] **Step 4: Run**

Run: `cargo test --lib config -q`
Expected: all config tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): MEM_VECTOR_INDEX_{FLUSH_EVERY,OVERSAMPLE,USE_LEGACY}"
```

---

## Task 13: `DuckDbRepository` holds `Arc<VectorIndex>` (setter)

**Files:**
- Modify: `src/storage/duckdb.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/vector_index.rs`:

```rust
#[tokio::test]
async fn duckdb_repository_accepts_vector_index_injection() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("inject.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let fp = mem::storage::VectorIndexFingerprint {
        provider: "fake".into(),
        model: "fake".into(),
        dim: 256,
    };
    let idx = std::sync::Arc::new(VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap());
    repo.attach_vector_index(idx.clone());
    assert!(repo.has_vector_index());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test vector_index injection -q`
Expected: compile error (`attach_vector_index` undefined).

- [ ] **Step 3: Implement the holder**

In `src/storage/duckdb.rs`, modify the `DuckDbRepository` struct:

```rust
pub struct DuckDbRepository {
    conn: Arc<Mutex<Connection>>,
    // existing fields...
    vector_index: Arc<RwLock<Option<Arc<VectorIndex>>>>,
}
```

In `DuckDbRepository::open` (or wherever the struct is constructed), initialize:

```rust
vector_index: Arc::new(RwLock::new(None)),
```

Add accessors:

```rust
impl DuckDbRepository {
    pub fn attach_vector_index(&self, idx: Arc<VectorIndex>) {
        *self.vector_index.write().expect("vector_index lock poisoned") = Some(idx);
    }

    pub fn has_vector_index(&self) -> bool {
        self.vector_index.read().expect("vector_index lock poisoned").is_some()
    }

    pub(crate) fn vector_index(&self) -> Option<Arc<VectorIndex>> {
        self.vector_index.read().expect("vector_index lock poisoned").clone()
    }
}
```

Add `use std::sync::RwLock;` if not already present, and `use crate::storage::VectorIndex;`.

- [ ] **Step 4: Run**

Run: `cargo test --test vector_index injection -q`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add src/storage/duckdb.rs tests/vector_index.rs
git commit -m "feat(storage): DuckDbRepository holds Arc<VectorIndex> via setter"
```

---

## Task 14: Rewrite `semantic_search_memories`

**Files:**
- Modify: `src/storage/duckdb.rs`

- [ ] **Step 1: Write the failing test**

Add `tests/semantic_search_via_ann.rs`:

```rust
use mem::domain::memory::{
    IngestMemoryRequest, MemoryStatus, MemoryType, Scope, Visibility, WriteMode,
};
use mem::service::MemoryService;
use mem::service::embedding_worker;
use mem::config::EmbeddingSettings;
use mem::embedding::arc_embedding_provider;
use mem::storage::{DuckDbRepository, VectorIndex, VectorIndexFingerprint};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn semantic_search_uses_vector_index_when_attached() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ann.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = EmbeddingSettings::development_defaults();
    let provider = arc_embedding_provider(&settings).unwrap();

    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = Arc::new(VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap());
    repo.attach_vector_index(idx.clone());

    let svc = MemoryService::new_with_index(repo.clone(), idx.clone());
    let make = |c: &str| IngestMemoryRequest {
        tenant: "t".into(),
        memory_type: MemoryType::Implementation,
        content: c.into(),
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: None,
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    };
    let _ = svc.ingest(make("alpha-content")).await.unwrap();
    let _ = svc.ingest(make("beta-content")).await.unwrap();

    // Drive the worker to completion
    for _ in 0..4 {
        let _ = embedding_worker::tick(&repo, provider.as_ref(), &settings).await;
    }
    assert!(idx.size() >= 1);

    // Issue a semantic search via the repository's public method
    let q = provider.embed_text("alpha-content").await.unwrap();
    let hits = repo.semantic_search_memories("t", &q, 5).await.unwrap();
    assert!(!hits.is_empty());
    assert!(
        hits.iter().any(|(m, _)| m.content == "alpha-content"),
        "ANN should surface the alpha row"
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test semantic_search_via_ann -q`
Expected: compile error or runtime failure (worker doesn't yet write to VectorIndex; `MemoryService::new_with_index` undefined).

> The "drive worker" assertion `idx.size() >= 1` will fail in this task because Task 15 wires the worker. That is intentional — this task replaces the *read* path; Task 15 wires the *write* path. To make this task self-contained without Task 15, the test seeds the index directly:

Replace the worker drive loop in the test with:

```rust
    // Bypass worker: seed directly via the existing helper
    let alpha_emb = provider.embed_text("alpha-content").await.unwrap();
    repo.seed_memory_embedding_for_test("dummy", "t", &alpha_emb).await.unwrap();
    idx.upsert("dummy", &alpha_emb).await.unwrap();
```

This keeps Task 14 focused on the read path replacement.

- [ ] **Step 3: Replace `semantic_search_memories` body**

In `src/storage/duckdb.rs`, replace the body of `pub async fn semantic_search_memories(...)` with:

```rust
pub async fn semantic_search_memories(
    &self,
    tenant: &str,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
    if query_embedding.is_empty() || limit == 0 {
        return Ok(vec![]);
    }

    let Some(idx) = self.vector_index() else {
        // Pre-attach: behave as the legacy linear scan would.
        return self.legacy_semantic_search_memories(tenant, query_embedding, limit).await;
    };

    let oversample = std::env::var("MEM_VECTOR_INDEX_OVERSAMPLE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(4);
    let use_legacy = std::env::var("MEM_VECTOR_INDEX_USE_LEGACY")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if use_legacy {
        return self.legacy_semantic_search_memories(tenant, query_embedding, limit).await;
    }

    let k = limit.saturating_mul(oversample).max(limit);
    let hits = idx
        .search(query_embedding, k)
        .await
        .map_err(|e| StorageError::VectorIndex(format!("vector_index search: {e}")))?;
    if hits.is_empty() {
        return Ok(vec![]);
    }

    let id_strs: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
    let rows = self.fetch_memories_by_ids(tenant, &id_strs).await?;

    let by_id: std::collections::HashMap<&str, f32> =
        hits.iter().map(|(i, s)| (i.as_str(), *s)).collect();
    let mut scored: Vec<(MemoryRecord, f32)> = rows
        .into_iter()
        .filter_map(|m| by_id.get(m.memory_id.as_str()).map(|s| (m, *s)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    Ok(scored)
}
```

Rename the existing implementation to `legacy_semantic_search_memories` (private):

```rust
async fn legacy_semantic_search_memories(
    &self,
    tenant: &str,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
    // ... existing body verbatim, including the limit 2000 SQL ...
}
```

> Note: `MemoryService::new_with_index` is added in Task 16 — the test in this step uses `MemoryService::new` (existing) plus direct `repo.attach_vector_index(...)`. Adjust the test accordingly if needed.

- [ ] **Step 4: Run**

Run: `cargo test --test semantic_search_via_ann -q && cargo test --test search_api -q`
Expected: both pass. The existing `search_api` integration must continue to work because (a) without an attached index it hits `legacy_semantic_search_memories`, (b) with an attached index it goes through ANN; both produce sane results on small data.

- [ ] **Step 5: Commit**

```bash
git add src/storage/duckdb.rs tests/semantic_search_via_ann.rs
git commit -m "feat(storage): semantic_search_memories now uses VectorIndex (legacy path preserved under USE_LEGACY)"
```

---

## Task 15: Wire `vector_index.upsert` into the embedding worker

**Files:**
- Modify: `src/service/embedding_worker.rs`
- Modify: `tests/embedding_worker.rs`

- [ ] **Step 1: Update an existing test**

In `tests/embedding_worker.rs`, find `worker_completes_job_and_writes_embedding_row` and add at the end (after the `count_memory_embeddings_for_memory` assertion):

```rust
    // Vector index must reflect the new row when one is attached.
    let fp = mem::storage::VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = std::sync::Arc::new(
        mem::storage::VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap(),
    );
    repo.attach_vector_index(idx.clone());
    // Re-tick now with the index attached: a second job won't run, but the row was
    // already inserted, so open_or_rebuild's row_count check should populate it.
    assert_eq!(idx.size(), 1);
```

- [ ] **Step 2: Run to confirm passing baseline**

Run: `cargo test --test embedding_worker worker_completes -q`
Expected: pass (uses open_or_rebuild's rebuild path; no worker change yet).

Now write the actually-failing assertion: a fresh repo+index, attach, *then* tick the worker, *then* assert size grew:

Add a NEW test in `tests/embedding_worker.rs`:

```rust
#[tokio::test]
async fn worker_writes_to_attached_vector_index() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("worker-vec.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = EmbeddingSettings::development_defaults();
    let provider = arc_embedding_provider(&settings).unwrap();

    let fp = mem::storage::VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = std::sync::Arc::new(
        mem::storage::VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap(),
    );
    repo.attach_vector_index(idx.clone());
    assert_eq!(idx.size(), 0);

    let service = MemoryService::new(repo.clone());
    let response = service
        .ingest(IngestMemoryRequest {
            tenant: "t".into(),
            memory_type: MemoryType::Implementation,
            content: "wire-up content".into(),
            evidence: vec![],
            code_refs: vec![],
            scope: Scope::Repo,
            visibility: Visibility::Shared,
            project: None,
            repo: Some("mem".into()),
            module: None,
            task_type: None,
            tags: vec![],
            source_agent: "test".into(),
            idempotency_key: None,
            write_mode: WriteMode::Auto,
        })
        .await
        .unwrap();

    embedding_worker::tick(&repo, provider.as_ref(), &settings).await.unwrap();

    assert_eq!(idx.size(), 1);
    let q = provider.embed_text("wire-up content").await.unwrap();
    let hits = idx.search(&q, 1).await.unwrap();
    assert_eq!(hits[0].0, response.memory_id);
}
```

Run: `cargo test --test embedding_worker worker_writes_to_attached -q`
Expected: FAIL — `idx.size()` still 0 after tick because worker doesn't push.

- [ ] **Step 3: Wire the worker**

In `src/service/embedding_worker.rs`, after `repo.upsert_memory_embedding(...)` and before `repo.complete_embedding_job(...)`, insert:

```rust
if let Some(idx) = repo.vector_index() {
    if let Err(err) = idx.upsert(&job.memory_id, &embedding).await {
        warn!(
            job_id = %job.job_id,
            memory_id = %job.memory_id,
            error = %err,
            "vector index upsert failed; embedding row already written"
        );
        // Best effort: do not fail the job. Row+index reconciliation happens on next startup.
    } else {
        let count = idx.dirty_count_increment();
        if count >= settings.vector_index_flush_every {
            if let Err(err) = idx.save_at_default_paths().await {
                warn!(error = %err, "vector index periodic save failed");
            } else {
                idx.dirty_count_reset();
            }
        }
    }
}
```

This requires three new helpers on `VectorIndex`:

```rust
impl VectorIndex {
    pub fn dirty_count_increment(&self) -> usize {
        self.dirty.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1
    }
    pub fn dirty_count_reset(&self) {
        self.dirty.store(0, std::sync::atomic::Ordering::Release);
    }
    pub async fn save_at_default_paths(&self) -> Result<(), VectorIndexError> {
        let (idx_path, meta_path) = (
            self.idx_path.clone().expect("save_at_default_paths needs known paths"),
            self.meta_path.clone().expect("save_at_default_paths needs known paths"),
        );
        self.save_to(&idx_path, &meta_path).await
    }
}
```

This requires VectorIndex to remember its paths after `open_or_rebuild`. Update the struct:

```rust
pub struct VectorIndex {
    index: Arc<RwLock<Index>>,
    id_map: Arc<RwLock<HashMap<u64, String>>>,
    fingerprint: VectorIndexFingerprint,
    dirty: std::sync::atomic::AtomicUsize,
    idx_path: Option<PathBuf>,
    meta_path: Option<PathBuf>,
}
```

`new_in_memory` sets paths to `None`; `open_or_rebuild` sets them to the resolved sidecar paths after `save_to` succeeds.

- [ ] **Step 4: Run**

Run: `cargo test --test embedding_worker -q`
Expected: all 4 pass (3 prior + 1 new).

- [ ] **Step 5: Commit**

```bash
git add src/service/embedding_worker.rs src/storage/vector_index.rs tests/embedding_worker.rs
git commit -m "feat(worker): mirror upsert_memory_embedding into VectorIndex"
```

---

## Task 16: Wire `vector_index.remove` into delete paths

**Files:**
- Modify: `src/service/memory_service.rs`
- Modify: `src/storage/duckdb.rs` (the two internal helper sites)
- Modify: `tests/review_api.rs` (where supersede flow lives) **OR** new test

- [ ] **Step 1: Write the failing test**

Add `tests/vector_index_delete_mirror.rs`:

```rust
use mem::domain::memory::{
    IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode,
};
use mem::service::MemoryService;
use mem::service::embedding_worker;
use mem::config::EmbeddingSettings;
use mem::embedding::arc_embedding_provider;
use mem::storage::{DuckDbRepository, VectorIndex, VectorIndexFingerprint};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn delete_paths_mirror_into_vector_index() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("del.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = EmbeddingSettings::development_defaults();
    let provider = arc_embedding_provider(&settings).unwrap();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = Arc::new(VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap());
    repo.attach_vector_index(idx.clone());

    let svc = MemoryService::new(repo.clone());
    let req = |c: &str| IngestMemoryRequest {
        tenant: "t".into(),
        memory_type: MemoryType::Implementation,
        content: c.into(),
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: None,
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    };
    let r = svc.ingest(req("first")).await.unwrap();
    embedding_worker::tick(&repo, provider.as_ref(), &settings).await.unwrap();
    assert_eq!(idx.size(), 1);

    repo.delete_memory_embedding(&r.memory_id).await.unwrap();
    assert_eq!(
        repo.count_memory_embeddings_for_memory(&r.memory_id).await.unwrap(),
        0
    );
    assert_eq!(
        idx.size(),
        0,
        "vector_index must mirror delete_memory_embedding"
    );
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test vector_index_delete_mirror -q`
Expected: FAIL — index still has 1 row after delete.

- [ ] **Step 3: Wire the three delete sites**

**Site 1** — `src/service/memory_service.rs:287` (the explicit `delete_memory_embedding` caller):

```rust
self.repository.delete_memory_embedding(&mid).await?;
if let Some(idx) = self.repository.vector_index() {
    if let Err(err) = idx.remove(&mid).await {
        tracing::warn!(memory_id = %mid, error = %err, "vector index remove failed");
    }
}
```

**Site 2** — `src/storage/duckdb.rs:834` (`delete_embedding_references` inside supersede). Wrap the call site:

```rust
delete_embedding_references(&conn, original_memory_id)?;
if let Some(idx) = self.vector_index() {
    let _ = idx.remove(original_memory_id).await; // best effort
}
```

**Site 3** — `src/storage/duckdb.rs:1198` (the second `delete_embedding_references` call). Apply the same wrapper.

> The cleanest place to centralize this is to make `DuckDbRepository::delete_memory_embedding` itself fan out to `vector_index().remove(...)` after the SQL succeeds. If you do that, the wrapper at site 1 in `memory_service.rs` becomes unnecessary. Pick one approach and apply consistently.

- [ ] **Step 4: Run**

Run: `cargo test --test vector_index_delete_mirror -q && cargo test --test review_api -q`
Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add src/service/memory_service.rs src/storage/duckdb.rs tests/vector_index_delete_mirror.rs
git commit -m "feat(storage): mirror every memory_embeddings delete into VectorIndex"
```

---

## Task 17: `MemoryService::new_with_index` constructor

**Files:**
- Modify: `src/service/memory_service.rs`

- [ ] **Step 1: Add the constructor**

In `src/service/memory_service.rs`, find `impl MemoryService` and add:

```rust
impl MemoryService {
    pub fn new_with_index(
        repository: DuckDbRepository,
        vector_index: Arc<crate::storage::VectorIndex>,
    ) -> Self {
        repository.attach_vector_index(vector_index);
        Self::new(repository)
    }
}
```

This is an ergonomic wrapper. The hard wiring already happened in Task 13 (DuckDbRepository owns the Arc); this constructor just guarantees a service+index pair is constructed atomically at app startup.

- [ ] **Step 2: Run library tests**

Run: `cargo build --tests -q`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add src/service/memory_service.rs
git commit -m "feat(service): MemoryService::new_with_index helper"
```

---

## Task 18: Wire startup in `app.rs`

**Files:**
- Modify: `src/app.rs`

- [ ] **Step 1: Read the current startup sequence**

Inspect `src/app.rs`. Locate the lines that:
- Construct `Config`
- Open `DuckDbRepository`
- Construct `MemoryService`
- Spawn `embedding_worker::run`

- [ ] **Step 2: Insert `VectorIndex::open_or_rebuild` after repo is open**

Pseudocode for the new sequence (preserve surrounding context):

```rust
let config = Config::from_env()?;
let repo = DuckDbRepository::open(&config.db_path).await?;

let fp = mem::storage::VectorIndexFingerprint {
    provider: config.embedding.job_provider_id().to_string(),
    model: config.embedding.model.clone(),
    dim: config.embedding.dim,
};
let vector_index = std::sync::Arc::new(
    mem::storage::VectorIndex::open_or_rebuild(&repo, &config.db_path, &fp).await?,
);
repo.attach_vector_index(vector_index.clone());

let provider = mem::embedding::arc_embedding_provider(&config.embedding)?;
let service = mem::service::MemoryService::new(repo.clone());
// existing wiring continues...
```

The `attach_vector_index` call ensures both the worker (which reads `repo.vector_index()`) and the search path see the same Arc.

- [ ] **Step 3: Run**

Run: `cargo run --quiet` for a smoke (Ctrl-C after a few seconds), then `cargo test -q`.
Expected: server starts, log shows `rebuilt vector index: 0 rows in N ms`. All tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "feat(app): open_or_rebuild VectorIndex at startup"
```

---

## Task 19: Concurrency smoke test

**Files:**
- Modify: `tests/vector_index.rs`

- [ ] **Step 1: Write the test**

Append:

```rust
#[tokio::test]
async fn concurrent_search_and_upsert_does_not_panic() {
    let idx = std::sync::Arc::new(VectorIndex::new_in_memory(256, "fake", "fake", 64));
    // Pre-populate
    for i in 0..32u8 {
        idx.upsert(&format!("seed_{i}"), &unit_vector(256, i)).await.unwrap();
    }

    let mut handles = Vec::new();
    for q in 0..10u8 {
        let idx_c = idx.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..50 {
                let _ = idx_c.search(&unit_vector(256, q), 5).await.unwrap();
            }
        }));
    }
    let idx_w = idx.clone();
    handles.push(tokio::spawn(async move {
        for i in 100..150u8 {
            idx_w
                .upsert(&format!("hot_{i}"), &unit_vector(256, i))
                .await
                .unwrap();
        }
    }));

    for h in handles {
        h.await.unwrap();
    }
    assert!(idx.size() >= 32);
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test vector_index concurrent -q`
Expected: pass.

- [ ] **Step 3: Commit**

```bash
git add tests/vector_index.rs
git commit -m "test(vector_index): concurrent search + upsert smoke"
```

---

## Task 20: `MEM_VECTOR_INDEX_USE_LEGACY` regression test

**Files:**
- Create: `tests/vector_index_use_legacy.rs`

- [ ] **Step 1: Write the test**

```rust
use mem::domain::memory::{
    IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode,
};
use mem::service::{embedding_worker, MemoryService};
use mem::config::EmbeddingSettings;
use mem::embedding::arc_embedding_provider;
use mem::storage::{DuckDbRepository, VectorIndex, VectorIndexFingerprint};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn use_legacy_env_skips_vector_index() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("legacy.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = EmbeddingSettings::development_defaults();
    let provider = arc_embedding_provider(&settings).unwrap();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = Arc::new(VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap());
    repo.attach_vector_index(idx.clone());

    let svc = MemoryService::new(repo.clone());
    let _ = svc
        .ingest(IngestMemoryRequest {
            tenant: "t".into(),
            memory_type: MemoryType::Implementation,
            content: "legacy-target".into(),
            evidence: vec![],
            code_refs: vec![],
            scope: Scope::Repo,
            visibility: Visibility::Shared,
            project: None,
            repo: Some("mem".into()),
            module: None,
            task_type: None,
            tags: vec![],
            source_agent: "test".into(),
            idempotency_key: None,
            write_mode: WriteMode::Auto,
        })
        .await
        .unwrap();
    embedding_worker::tick(&repo, provider.as_ref(), &settings).await.unwrap();

    let q = provider.embed_text("legacy-target").await.unwrap();

    // Default path (ANN)
    std::env::remove_var("MEM_VECTOR_INDEX_USE_LEGACY");
    let ann_hits = repo.semantic_search_memories("t", &q, 1).await.unwrap();

    // Legacy path
    std::env::set_var("MEM_VECTOR_INDEX_USE_LEGACY", "1");
    let legacy_hits = repo.semantic_search_memories("t", &q, 1).await.unwrap();
    std::env::remove_var("MEM_VECTOR_INDEX_USE_LEGACY");

    assert_eq!(ann_hits.len(), 1);
    assert_eq!(legacy_hits.len(), 1);
    assert_eq!(ann_hits[0].0.memory_id, legacy_hits[0].0.memory_id);
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test vector_index_use_legacy -q`
Expected: pass.

- [ ] **Step 3: Commit**

```bash
git add tests/vector_index_use_legacy.rs
git commit -m "test(vector_index): MEM_VECTOR_INDEX_USE_LEGACY equivalence"
```

---

## Task 21: Final verification + close §8 #3 in roadmap

**Files:**
- Modify: `docs/mempalace-diff.md`

- [ ] **Step 1: Run full verification**

Run:
```bash
cargo test -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: all green.

- [ ] **Step 2: Smoke `cargo run`**

Run: `MEM_DB_PATH=/tmp/mem-smoke.duckdb cargo run`
Expected: server starts, log line `rebuilt vector index: 0 rows in N ms` (or `loaded vector index from sidecar` on second start).

- [ ] **Step 3: Mark §8 row #3 complete**

In `docs/mempalace-diff.md`, change the row from:

```markdown
| 3 | 🔍 | 引入 `usearch` sidecar ANN（同时消除 `semantic_search_memories` 的 `limit 2000` 隐式截断）| 🟠 性能基础设施 + 🔴 修隐式正确性边界 | M（1–2 天） | 中（需要 repair 路径） | `Cargo.toml`、`storage/`、新增 `vector_index.rs` |
```

to:

```markdown
| 3 | 🔍 | ✅ 引入 `usearch` sidecar ANN（消除 `semantic_search_memories` 的 `limit 2000` 隐式截断）| 🟠 性能基础设施 + 🔴 修隐式正确性边界 | M（1–2 天） | 中（需要 repair 路径） | `Cargo.toml`、`storage/`、新增 `vector_index.rs` |
```

- [ ] **Step 4: Commit**

```bash
git add docs/mempalace-diff.md
git commit -m "docs(mempalace-diff): mark §8 #3 complete (closes mempalace-diff §8 #3)"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage check:** every section of `2026-04-27-vector-index-sidecar-design.md` maps to one or more tasks above. Tasks 1–10 build the core module; 11–12 add SQL helpers and config; 13–18 wire it into the application; 19–21 cover concurrency, fallback, and roadmap closure.
- **`USE_LEGACY` is a one-release escape hatch.** Add a `// TODO: remove after release X.Y` comment when adding the env var. Filing a bd issue at the same time is recommended.
- **`StorageError::VectorIndex(String)`** is the dedicated variant for runtime errors flowing out of vector_index.rs (added in Task 2). Don't smuggle them through `InvalidData`.
- **`semantic_search_memories` keeps its public signature.** No caller in `memory_service.rs` or `pipeline/retrieve.rs` should need editing. Verify with `grep -rn semantic_search_memories src/ tests/` before declaring victory.
- **Schema migrations:** none required. Sidecar lives outside DuckDB.
- **Manual verification:** between Tasks 18 and 19, run `cargo run` against an existing non-empty DB (if you have one) and confirm `loaded vector index from sidecar` appears on second start.
