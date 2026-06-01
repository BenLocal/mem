//! Recall ablation bench (capsule retrieval). Rebuilds the bench deleted
//! in 4df527b, scoped to capsule recall (baselines K9 / K10 / ③).
//! Run with: cargo test --test recall_bench -- --ignored --nocapture
//! Spec: docs/superpowers/specs/2026-06-01-recall-bench-rebuild-design.md
mod bench;

use bench::runner::{pretty_table, run_bench, write_json, Rung};
use bench::synthetic::{generate, SyntheticConfig};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "ablation bench — run with --ignored"]
async fn recall_ablation() {
    let cfg = SyntheticConfig::default();
    let f = generate(&cfg);
    let rungs = [
        Rung::LexicalOnly,
        Rung::SemanticOnly,
        Rung::Hybrid,
        Rung::Graph,
        Rung::Dynamics,
        Rung::ChunkingOn,
        Rung::ChunkingOff,
        Rung::Oracle,
    ];
    let report = run_bench(&f, &rungs).await;
    println!("\n{}", pretty_table(&report));
    let dir = std::path::Path::new("target/recall_bench");
    std::fs::create_dir_all(dir).unwrap();
    write_json(
        &report,
        &dir.join(format!("{}-seed{}.json", f.tenant, cfg.seed)),
    )
    .unwrap();
}
