//! Test-only helpers for the recall ablation bench. See
//! `docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md`.
//!
//! This module is loaded via `mod bench;` from `tests/recall_bench.rs`.
#![allow(dead_code)] // submodules build incrementally; some helpers land before callers.

pub mod fixture;
pub mod real;
pub mod synthetic;
