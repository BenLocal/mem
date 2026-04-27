use mem::storage::vector_index::{EmbeddingRowSource, VectorIndex};

#[allow(dead_code)]
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
