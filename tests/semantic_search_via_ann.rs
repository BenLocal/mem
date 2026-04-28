use mem::storage::{DuckDbRepository, VectorIndex, VectorIndexFingerprint};
use std::sync::Arc;
use tempfile::tempdir;

fn unit_vector(dim: usize, seed: u8) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    v[seed as usize % dim] = 1.0;
    v
}

/// Verifies that `semantic_search_memories` uses the ANN path (VectorIndex) when one is
/// attached, and correctly re-attaches scores and returns the expected memory_id at the top.
#[tokio::test]
async fn semantic_search_uses_vector_index_when_attached() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ann.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let fp = VectorIndexFingerprint {
        provider: "fake".to_string(),
        model: "fake".to_string(),
        dim: 256,
    };
    let idx = Arc::new(VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap());
    repo.attach_vector_index(idx.clone());

    // Seed the DB row + index entry directly (bypassing the embedding worker).
    let alpha_emb = unit_vector(256, 1);
    repo.seed_memory_embedding_for_test("dummy", "t", &alpha_emb)
        .await
        .unwrap();
    idx.upsert("dummy", &alpha_emb).await.unwrap();

    // Query with the same vector — cosine similarity should be 1.0.
    let hits = repo
        .semantic_search_memories("t", &alpha_emb, 5)
        .await
        .unwrap();
    assert!(!hits.is_empty(), "ANN path should return at least one hit");
    assert_eq!(
        hits[0].0.memory_id, "dummy",
        "top hit should be the seeded memory"
    );
    assert!(
        hits[0].1 > 0.99,
        "exact-match cosine score should be ~1.0, got {}",
        hits[0].1
    );
}

/// Verifies that when no VectorIndex is attached, the function transparently falls back to
/// the legacy linear scan and still returns correct results.
#[tokio::test]
async fn semantic_search_falls_back_to_legacy_when_no_index_attached() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("legacy.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    // Intentionally do NOT attach a VectorIndex.

    let emb = unit_vector(256, 2);
    repo.seed_memory_embedding_for_test("legacy_mem", "t", &emb)
        .await
        .unwrap();

    let hits = repo.semantic_search_memories("t", &emb, 5).await.unwrap();
    assert!(
        !hits.is_empty(),
        "legacy path should return at least one hit"
    );
    assert_eq!(hits[0].0.memory_id, "legacy_mem");
}

/// Verifies that `MEM_VECTOR_INDEX_USE_LEGACY=1` forces the linear-scan path even when
/// a VectorIndex is attached.
#[tokio::test]
async fn semantic_search_respects_use_legacy_env_var() {
    // Safety: env-var mutation in tests is racy across threads.  Tokio's default runtime is
    // multi-threaded, but each #[tokio::test] gets its own runtime, so this is safe within
    // a single test function provided no other thread mutates the same var concurrently.
    // We restore the value on exit.
    let prior = std::env::var("MEM_VECTOR_INDEX_USE_LEGACY").ok();
    unsafe {
        std::env::set_var("MEM_VECTOR_INDEX_USE_LEGACY", "1");
    }

    let result = async {
        let dir = tempdir().unwrap();
        let db = dir.path().join("use_legacy.duckdb");
        let repo = DuckDbRepository::open(&db).await.unwrap();

        let fp = VectorIndexFingerprint {
            provider: "fake".to_string(),
            model: "fake".to_string(),
            dim: 256,
        };
        let idx = Arc::new(VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap());
        repo.attach_vector_index(idx.clone());

        let emb = unit_vector(256, 3);
        repo.seed_memory_embedding_for_test("legacy_forced", "t", &emb)
            .await
            .unwrap();
        idx.upsert("legacy_forced", &emb).await.unwrap();

        let hits = repo.semantic_search_memories("t", &emb, 5).await.unwrap();
        assert!(!hits.is_empty(), "USE_LEGACY=1 should still find results");
        assert_eq!(hits[0].0.memory_id, "legacy_forced");
    }
    .await;

    // Restore env var.
    match prior {
        Some(v) => unsafe {
            std::env::set_var("MEM_VECTOR_INDEX_USE_LEGACY", v);
        },
        None => unsafe {
            std::env::remove_var("MEM_VECTOR_INDEX_USE_LEGACY");
        },
    }

    result
}

/// Edge case: empty query embedding returns empty results without panicking.
#[tokio::test]
async fn semantic_search_empty_query_returns_empty() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("empty_q.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let hits = repo.semantic_search_memories("t", &[], 5).await.unwrap();
    assert!(hits.is_empty());
}

/// Edge case: limit=0 returns empty results without panicking.
#[tokio::test]
async fn semantic_search_limit_zero_returns_empty() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("zero_limit.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let emb = unit_vector(256, 0);
    let hits = repo.semantic_search_memories("t", &emb, 0).await.unwrap();
    assert!(hits.is_empty());
}
