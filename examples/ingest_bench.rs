//! Service-layer benchmark: single-row ingest vs batch ingest.
//!
//! Measures wall-clock time for the user-perceived hot path on both
//! pipelines:
//!
//!   - `CapabilityCapsuleService::ingest`         × N    (the old per-row HTTP)
//!   - `CapabilityCapsuleService::ingest_batch`   × 1    (the new /batch HTTP)
//!   - `TranscriptService::ingest`                × N
//!   - `TranscriptService::ingest_batch`          × 1
//!
//! Each scenario runs against a freshly-opened `Store` rooted in a
//! tempdir, so neither side benefits from a warmed-up disk cache from
//! the other.
//!
//! Run with:
//!   cargo run --example ingest_bench --release
//!
//! Optional env:
//!   MEM_BENCH_SIZES   comma-separated chunk sizes (default "10,50,100,200")

use std::sync::Arc;
use std::time::{Duration, Instant};

use mem::domain::capability_capsule::{
    CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
};
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::service::{CapabilityCapsuleService, TranscriptService};
use mem::storage::Store;
use tempfile::TempDir;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    let sizes = parse_sizes();

    println!("Ingest benchmark — service layer");
    println!("(each cell = wall-clock for a fresh Store; lower is better)");
    println!();

    println!("== Capability capsules ==");
    print_table_header();
    for n in &sizes {
        let single = bench_capsule_single(*n).await?;
        let batched = bench_capsule_batch(*n).await?;
        print_row(*n, single, batched);
    }

    println!();
    println!("== Transcript blocks ==");
    print_table_header();
    for n in &sizes {
        let single = bench_transcript_single(*n).await?;
        let batched = bench_transcript_batch(*n).await?;
        print_row(*n, single, batched);
    }

    Ok(())
}

fn parse_sizes() -> Vec<usize> {
    std::env::var("MEM_BENCH_SIZES")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|t| t.trim().parse::<usize>().ok())
                .filter(|n| *n > 0)
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| vec![10, 50, 100, 200])
}

fn print_table_header() {
    println!(
        "{:>5}  {:>12}  {:>12}  {:>10}  {:>10}",
        "N", "single (ms)", "batch (ms)", "speedup", "per-row μs"
    );
    println!("{}", "-".repeat(58));
}

fn print_row(n: usize, single: Duration, batched: Duration) {
    let speedup = if batched.as_nanos() == 0 {
        f64::INFINITY
    } else {
        single.as_secs_f64() / batched.as_secs_f64()
    };
    let per_row_single_us = (single.as_micros() as f64) / (n as f64);
    let per_row_batch_us = (batched.as_micros() as f64) / (n as f64);
    println!(
        "{:>5}  {:>12.1}  {:>12.1}  {:>9.1}x  {:>4.0} → {:<4.0}",
        n,
        ms(single),
        ms(batched),
        speedup,
        per_row_single_us,
        per_row_batch_us,
    );
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

// ─── Capsule benches ────────────────────────────────────────────────

async fn bench_capsule_single(n: usize) -> anyhow::Result<Duration> {
    let dir = TempDir::new()?;
    let store = Arc::new(Store::open(dir.path().join("store")).await?);
    let svc = CapabilityCapsuleService::new(store);

    let requests = (0..n).map(make_capsule_request).collect::<Vec<_>>();
    let t = Instant::now();
    for req in requests {
        svc.ingest(req).await?;
    }
    Ok(t.elapsed())
}

async fn bench_capsule_batch(n: usize) -> anyhow::Result<Duration> {
    let dir = TempDir::new()?;
    let store = Arc::new(Store::open(dir.path().join("store")).await?);
    let svc = CapabilityCapsuleService::new(store);

    let requests = (0..n).map(make_capsule_request).collect::<Vec<_>>();
    let t = Instant::now();
    let _ = svc.ingest_batch(requests).await?;
    Ok(t.elapsed())
}

fn make_capsule_request(i: usize) -> IngestCapabilityCapsuleRequest {
    IngestCapabilityCapsuleRequest {
        tenant: "bench".to_string(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        // Each capsule's content must be unique enough that the
        // idempotency-by-content-hash dedup doesn't fire.
        content: format!(
            "bench capsule #{i}: parameters get hashed so this counts as a fresh row \
             — do not collapse via dedup"
        ),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Private,
        project: Some("mem".to_string()),
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "bench".to_string(),
        idempotency_key: Some(format!("bench-{i}")),
        write_mode: WriteMode::Auto,
    }
}

// ─── Transcript benches ─────────────────────────────────────────────

async fn bench_transcript_single(n: usize) -> anyhow::Result<Duration> {
    let dir = TempDir::new()?;
    let store = Arc::new(Store::open(dir.path().join("store")).await?);
    store.set_transcript_job_provider("bench");
    let svc = TranscriptService::new(store, None);

    let msgs = (0..n).map(make_message).collect::<Vec<_>>();
    let t = Instant::now();
    for msg in msgs {
        svc.ingest(msg).await?;
    }
    Ok(t.elapsed())
}

async fn bench_transcript_batch(n: usize) -> anyhow::Result<Duration> {
    let dir = TempDir::new()?;
    let store = Arc::new(Store::open(dir.path().join("store")).await?);
    store.set_transcript_job_provider("bench");
    let svc = TranscriptService::new(store, None);

    let msgs = (0..n).map(make_message).collect::<Vec<_>>();
    let t = Instant::now();
    let _ = svc.ingest_batch(msgs).await?;
    Ok(t.elapsed())
}

fn make_message(i: usize) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("blk_{i}"),
        session_id: Some("bench-session".to_string()),
        tenant: "bench".to_string(),
        caller_agent: "bench".to_string(),
        // Shared transcript_path so the bulk dedup probe is realistic
        // (same shape as `mem mine` writing one transcript file).
        transcript_path: "/tmp/bench.jsonl".to_string(),
        line_number: i as u64,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type: BlockType::Text,
        content: format!("bench transcript block #{i}"),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: true,
        created_at: format!("00000001778000{:06}0", i),
        meta_json: None,
    }
}
