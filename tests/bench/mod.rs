//! Test-only helpers for the recall ablation bench. See
//! `docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md`.
//!
//! This module is loaded via `mod bench;` from `tests/recall_bench.rs`.

pub mod fixture;
pub mod judgment;
pub mod longmemeval;
pub mod longmemeval_dataset;
pub mod oracle;
pub mod real;
pub mod runner;
pub mod synthetic;
