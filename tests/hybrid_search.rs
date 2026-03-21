use std::sync::Arc;

use mem::{
    config::EmbeddingSettings,
    domain::{
        memory::{MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility},
        query::SearchMemoryRequest,
    },
    embedding::{arc_embedding_provider, deterministic_embedding},
    service::MemoryService,
    storage::DuckDbRepository,
};
use tempfile::tempdir;

fn f32_to_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

#[tokio::test]
async fn hybrid_search_surfaces_semantic_match_without_lexical_overlap() {
    let dim = 64;
    let mut settings = EmbeddingSettings::development_defaults();
    settings.dim = dim;
    let provider = arc_embedding_provider(&settings).unwrap();

    let dir = tempdir().unwrap();
    let db = dir.path().join("hybrid.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let query_text = "query-unique-semantic-anchor-xyz";
    let query_vec = deterministic_embedding(query_text, dim);

    let mem_noise = MemoryRecord {
        memory_id: "mem_noise".into(),
        tenant: "t1".into(),
        memory_type: MemoryType::Implementation,
        status: MemoryStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: "noise".into(),
        content: "alpha beta gamma delta".into(),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: "hash-noise".into(),
        idempotency_key: None,
        supersedes_memory_id: None,
        source_agent: "test".into(),
        created_at: "1".into(),
        updated_at: "1".into(),
        last_validated_at: None,
    };

    let mem_hit = MemoryRecord {
        memory_id: "mem_hit".into(),
        tenant: "t1".into(),
        memory_type: MemoryType::Implementation,
        status: MemoryStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: "other".into(),
        content: "unrelated body text".into(),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: "hash-hit".into(),
        idempotency_key: None,
        supersedes_memory_id: None,
        source_agent: "test".into(),
        created_at: "2".into(),
        updated_at: "2".into(),
        last_validated_at: None,
    };

    repo.insert_memory(mem_noise.clone()).await.unwrap();
    repo.insert_memory(mem_hit.clone()).await.unwrap();

    let now = "00000000000000000001";
    repo.upsert_memory_embedding(
        &mem_noise.memory_id,
        &mem_noise.tenant,
        "fake",
        dim as i64,
        &f32_to_blob(&deterministic_embedding("noise-source", dim)),
        &mem_noise.content_hash,
        &mem_noise.updated_at,
        now,
    )
    .await
    .unwrap();
    repo.upsert_memory_embedding(
        &mem_hit.memory_id,
        &mem_hit.tenant,
        "fake",
        dim as i64,
        &f32_to_blob(&query_vec),
        &mem_hit.content_hash,
        &mem_hit.updated_at,
        now,
    )
    .await
    .unwrap();

    let service = MemoryService::with_graph_and_embedding_providers(
        repo,
        Arc::new(mem::storage::LocalGraphAdapter::default()),
        "fake".into(),
        Some(provider),
    );

    let response = service
        .search(SearchMemoryRequest {
            query: query_text.into(),
            intent: "debugging".into(),
            scope_filters: vec![],
            token_budget: 800,
            caller_agent: "test".into(),
            expand_graph: false,
            tenant: Some("t1".into()),
        })
        .await
        .unwrap();

    let mut ids = Vec::new();
    ids.extend(response.directives.iter().map(|d| d.memory_id.as_str()));
    ids.extend(response.relevant_facts.iter().map(|f| f.memory_id.as_str()));
    ids.extend(response.reusable_patterns.iter().map(|p| p.memory_id.as_str()));
    if let Some(w) = response.suggested_workflow.as_ref() {
        ids.push(w.memory_id.as_str());
    }

    assert!(
        ids.iter().any(|id| *id == "mem_hit"),
        "expected semantic hit mem_hit in compressed response, got {ids:?}"
    );
}
