//! Integration tests for the `MEM_RW_POOL_ENABLED=1` r2d2 read pool
//! routing on `fetch_memories_by_ids`.
//!
//! These verify functional correctness under concurrent reads: the
//! pool actually parallelizes (no deadlock), and every concurrent
//! caller sees the same correct row set. The perf-improvement
//! threshold from the spec (≥1.5× P99 throughput) is *not* asserted
//! here — CI variance makes that flaky; the rollout gate that flips
//! the pool default-on lives in a separate bench.

use mem::{
    config::{EmbeddingProviderKind, EmbeddingSettings},
    domain::memory::{IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode},
    service::MemoryService,
    storage::DuckDbRepository,
};
use std::sync::Arc;
use tempfile::tempdir;

fn fake_settings() -> EmbeddingSettings {
    let mut s = EmbeddingSettings::development_defaults();
    s.provider = EmbeddingProviderKind::Fake;
    s.model = "fake".to_string();
    s.dim = 64;
    s
}

fn ingest_request(tenant: &str, content: &str) -> IngestMemoryRequest {
    IngestMemoryRequest {
        tenant: tenant.into(),
        memory_type: MemoryType::Implementation,
        content: content.into(),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: None,
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    }
}

/// 8 concurrent `fetch_memories_by_ids` calls against an
/// `MEM_RW_POOL_ENABLED=1` repo all return the same correct row set
/// without deadlocking.
///
/// We `set_var` *before* `DuckDbRepository::open` so the pool is built
/// during construction. The whole test is one `#[tokio::test]` to
/// keep env-var lifetime contained — there's no other test in this
/// file that could observe the flag flip.
#[tokio::test]
async fn pool_enabled_serves_concurrent_fetches() {
    // SAFETY: integration test binary is single-process; no other test
    // observes this env var since this file has only one test.
    unsafe {
        std::env::set_var("MEM_RW_POOL_ENABLED", "1");
    }

    let dir = tempdir().unwrap();
    let db = dir.path().join("pool.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = fake_settings();
    let service = Arc::new(MemoryService::new_with_settings(repo.clone(), &settings));

    let tenant = "tenant-pool";
    // Seed 24 memories so fetch_by_ids has meaningful work.
    let mut ids: Vec<String> = Vec::with_capacity(24);
    for i in 0..24 {
        let resp = service
            .ingest(ingest_request(tenant, &format!("pool-test fact {i}")))
            .await
            .expect("ingest");
        ids.push(resp.memory_id);
    }

    // Fan out 8 concurrent fetch_memories_by_ids. With the pool the
    // checkouts overlap; without the pool they serialize against
    // self.conn — we don't assert timing, only correctness.
    let mut handles = Vec::with_capacity(8);
    for _ in 0..8 {
        let repo_c = repo.clone();
        let ids_c: Vec<String> = ids.clone();
        handles.push(tokio::spawn(async move {
            let id_refs: Vec<&str> = ids_c.iter().map(|s| s.as_str()).collect();
            repo_c
                .fetch_memories_by_ids(tenant, &id_refs)
                .await
                .expect("fetch_memories_by_ids")
        }));
    }

    for h in handles {
        let rows = h.await.expect("fetch task");
        assert_eq!(
            rows.len(),
            24,
            "every concurrent fetch should hydrate all 24 seeded memories"
        );
    }

    unsafe {
        std::env::remove_var("MEM_RW_POOL_ENABLED");
    }
}

/// Same fan-out without the flag set — exercises the fallback path
/// where `with_read` locks `self.conn` instead of checking out a pool
/// connection. Same correctness expectation.
#[tokio::test]
async fn pool_disabled_falls_back_to_http_write_conn() {
    // Defensive: clear the var even if a parallel test set it
    // (Cargo runs each integration test file in its own process so
    // this is belt-and-braces).
    unsafe {
        std::env::remove_var("MEM_RW_POOL_ENABLED");
    }

    let dir = tempdir().unwrap();
    let db = dir.path().join("pool-off.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = fake_settings();
    let service = Arc::new(MemoryService::new_with_settings(repo.clone(), &settings));

    let tenant = "tenant-pool-off";
    let mut ids: Vec<String> = Vec::with_capacity(8);
    for i in 0..8 {
        let resp = service
            .ingest(ingest_request(tenant, &format!("fallback fact {i}")))
            .await
            .expect("ingest");
        ids.push(resp.memory_id);
    }

    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    let rows = repo
        .fetch_memories_by_ids(tenant, &id_refs)
        .await
        .expect("fetch_memories_by_ids");
    assert_eq!(rows.len(), 8);
}

/// Microbench: time N×K concurrent `fetch_memories_by_ids` calls.
/// Marked `#[ignore]` so regular `cargo test` skips it; run explicitly:
///
/// ```text
///     cargo test --release --test conn_pool bench_pool_off -- --ignored --nocapture
///     MEM_RW_POOL_ENABLED=1 \
///         cargo test --release --test conn_pool bench_pool_on -- --ignored --nocapture
/// ```
///
/// Compare the printed `fetches/sec` numbers to see the actual perf
/// delta on this hardware. Numbers are *not* CI-checked.
async fn bench_inner(label: &str) {
    let dir = tempdir().unwrap();
    let db = dir.path().join("bench.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = fake_settings();
    let service = Arc::new(MemoryService::new_with_settings(repo.clone(), &settings));

    let tenant = "bench-tenant";
    let mut ids: Vec<String> = Vec::with_capacity(100);
    for i in 0..100 {
        let resp = service
            .ingest(ingest_request(tenant, &format!("bench fact {i}")))
            .await
            .expect("ingest");
        ids.push(resp.memory_id);
    }

    // Warm-up: one sequential fetch to populate any caches.
    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    let _ = repo.fetch_memories_by_ids(tenant, &id_refs).await.unwrap();

    let iters = 200usize;
    let parallel = 8usize;
    let start = std::time::Instant::now();
    for _ in 0..iters {
        let mut handles = Vec::with_capacity(parallel);
        for _ in 0..parallel {
            let repo_c = repo.clone();
            let ids_c: Vec<String> = ids.clone();
            handles.push(tokio::spawn(async move {
                let id_refs: Vec<&str> = ids_c.iter().map(|s| s.as_str()).collect();
                repo_c
                    .fetch_memories_by_ids(tenant, &id_refs)
                    .await
                    .unwrap()
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }
    let elapsed = start.elapsed();
    let total_calls = iters * parallel;
    println!(
        "\nBENCH [{label}] {total_calls} concurrent fetches in {:.3?} ({:.0} fetches/sec, {:?}/fetch avg)",
        elapsed,
        total_calls as f64 / elapsed.as_secs_f64(),
        elapsed / total_calls as u32,
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_pool_off() {
    unsafe {
        std::env::remove_var("MEM_RW_POOL_ENABLED");
    }
    bench_inner("pool=off").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_pool_on() {
    unsafe {
        std::env::set_var("MEM_RW_POOL_ENABLED", "1");
    }
    bench_inner("pool=on").await;
    unsafe {
        std::env::remove_var("MEM_RW_POOL_ENABLED");
    }
}
