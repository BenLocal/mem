//! QW-1 bench: Rust-compose vs fused-SQL `hybrid_candidates`.
//!
//! Same shape as `examples/ingest_bench.rs` (project convention —
//! manual `Instant::now()` loops, no criterion dep). Seeds a Store
//! with N capsules + matching embeddings, then runs each
//! implementation M iterations across `k ∈ {10, 50, 100}` and prints
//! per-call wall-clock latency.
//!
//! Run with:
//!   cargo run --example hybrid_compose_vs_fused_bench --release
//!
//! Optional env:
//!   MEM_BENCH_HYBRID_N    seeded capsule count (default 500)
//!   MEM_BENCH_HYBRID_DIM  embedding dim (default 64 — keep small,
//!                         this is about the SQL/fan-out path, not
//!                         vector arithmetic)
//!   MEM_BENCH_HYBRID_M    iterations per (impl, k) cell (default 30)

use std::sync::Arc;
use std::time::{Duration, Instant};

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
};
use mem::service::embedding_helpers::f32_slice_to_blob;
use mem::storage::{current_timestamp, Store};
use tempfile::TempDir;

const TENANT: &str = "bench";
const QUERY_TEXT: &str = "lance vector search hybrid ranking pipeline candidate";

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    let n = env_usize("MEM_BENCH_HYBRID_N", 500);
    let dim = env_usize("MEM_BENCH_HYBRID_DIM", 64);
    let m = env_usize("MEM_BENCH_HYBRID_M", 30);

    println!("Hybrid candidates benchmark — Rust-compose vs fused-SQL");
    println!("(N={n} seeded capsules, dim={dim}, M={m} iterations per cell)");
    println!();

    let dir = TempDir::new()?;
    let store = Arc::new(Store::open(dir.path().join("store")).await?);
    seed(&store, n, dim).await?;
    let query_vec = synth_vec(0, dim);

    println!(
        "{:>5}  {:>14}  {:>14}  {:>9}  {:>14}  {:>14}",
        "k", "compose mean", "fused mean", "Δ %", "compose p99", "fused p99"
    );
    println!("{}", "-".repeat(80));

    for k in [10usize, 50, 100] {
        let compose = bench_compose(&store, &query_vec, k, m).await?;
        let fused = bench_fused(&store, &query_vec, k, m).await?;
        print_row(k, &compose, &fused);
    }

    Ok(())
}

struct Stats {
    mean: Duration,
    p50: Duration,
    p99: Duration,
}

async fn bench_compose(
    store: &Store,
    query_vec: &[f32],
    k: usize,
    m: usize,
) -> anyhow::Result<Stats> {
    // Warm-up — let the DuckDB extension prime caches, FTS index
    // load, etc. Without this the first iteration is a ~10x outlier.
    let _ = store
        .hybrid_candidates_compose(TENANT, QUERY_TEXT, query_vec, k)
        .await?;
    let mut samples = Vec::with_capacity(m);
    for _ in 0..m {
        let t = Instant::now();
        let _ = store
            .hybrid_candidates_compose(TENANT, QUERY_TEXT, query_vec, k)
            .await?;
        samples.push(t.elapsed());
    }
    Ok(summarise(samples))
}

async fn bench_fused(
    store: &Store,
    query_vec: &[f32],
    k: usize,
    m: usize,
) -> anyhow::Result<Stats> {
    let _ = store
        .hybrid_candidates(TENANT, QUERY_TEXT, query_vec, k)
        .await?;
    let mut samples = Vec::with_capacity(m);
    for _ in 0..m {
        let t = Instant::now();
        let _ = store
            .hybrid_candidates(TENANT, QUERY_TEXT, query_vec, k)
            .await?;
        samples.push(t.elapsed());
    }
    Ok(summarise(samples))
}

fn summarise(mut samples: Vec<Duration>) -> Stats {
    samples.sort();
    let n = samples.len();
    let mean = samples.iter().sum::<Duration>() / (n as u32);
    let p50 = samples[n / 2];
    let p99 = samples[((n as f64) * 0.99) as usize % n];
    Stats { mean, p50, p99 }
}

fn print_row(k: usize, compose: &Stats, fused: &Stats) {
    let delta_pct = if fused.mean.as_nanos() > 0 {
        let c = compose.mean.as_secs_f64();
        let f = fused.mean.as_secs_f64();
        ((c - f) / f) * 100.0
    } else {
        0.0
    };
    println!(
        "{:>5}  {:>11.2} ms  {:>11.2} ms  {:>+8.1}%  {:>11.2} ms  {:>11.2} ms",
        k,
        ms(compose.mean),
        ms(fused.mean),
        delta_pct,
        ms(compose.p99),
        ms(fused.p99),
    );
    let _ = compose.p50; // p50 captured for future analysis; not printed in the summary row
    let _ = fused.p50;
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

async fn seed(store: &Store, n: usize, dim: usize) -> anyhow::Result<()> {
    println!("Seeding {n} capsules with embeddings (dim={dim})...");
    let started = Instant::now();
    let now = current_timestamp();
    let mut capsules = Vec::with_capacity(n);
    for i in 0..n {
        capsules.push(make_capsule(i));
    }
    store.insert_capability_capsules(&capsules).await?;
    for (i, cap) in capsules.iter().enumerate() {
        let vec = synth_vec(i, dim);
        let blob = f32_slice_to_blob(&vec);
        store
            .upsert_capability_capsule_embedding(
                &cap.capability_capsule_id,
                TENANT,
                "bench-fake",
                dim as i64,
                &blob,
                &cap.content_hash,
                &cap.updated_at,
                &now,
            )
            .await?;
    }
    println!(
        "  done in {:.1}s ({} insert + {} upsert)",
        started.elapsed().as_secs_f64(),
        n,
        n
    );
    Ok(())
}

fn make_capsule(i: usize) -> CapabilityCapsuleRecord {
    // Content varies enough that BM25 sees real differences;
    // shared tokens keep the search non-trivial.
    let content = format!(
        "bench capsule {i}: lance vector search hybrid ranking pipeline candidate \
         experiment ranking RRF row {i} fixture"
    );
    let now = current_timestamp();
    CapabilityCapsuleRecord {
        capability_capsule_id: format!("mem_bench_{i:06}"),
        tenant: TENANT.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Private,
        version: 1,
        summary: format!("bench summary {i}"),
        content,
        evidence: vec![],
        code_refs: vec![],
        project: Some("mem".into()),
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.5,
        decay_score: 0.0,
        content_hash: format!("{:0>64}", i),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "bench".into(),
        created_at: now.clone(),
        updated_at: now,
        last_validated_at: None,
    }
}

/// Pseudo-random unit-ish vector seeded by `i`. Not cryptographic;
/// just gives ANN a different point per capsule.
fn synth_vec(i: usize, dim: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(dim);
    let seed = (i as u64).wrapping_mul(2_654_435_761);
    let mut x = seed;
    for _ in 0..dim {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let f = ((x >> 33) as u32 as f32) / (u32::MAX as f32);
        v.push(f - 0.5);
    }
    // Normalize.
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}
