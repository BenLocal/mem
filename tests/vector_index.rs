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
