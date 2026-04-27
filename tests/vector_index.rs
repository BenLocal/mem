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
