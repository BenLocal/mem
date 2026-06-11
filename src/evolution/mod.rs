//! Capsule self-evolution — E1 MVP (doc `docs/evolution-worker.md`).
//!
//! Layers:
//! - [`map`] — pure map-layer logic: embedding-space clustering
//!   (union-find on pairwise cosine, the `dedup_worker` skeleton),
//!   cross-cycle candidate alignment (member-set Jaccard), and the
//!   EvoMap-inspired anti-jitter evidence gate (K consecutive cycles
//!   + hysteresis). No I/O, no LLM.
//! - [`synthesis`] — the `SynthesisBackend` trait (doc §6.2). E1 ships
//!   only the `review` backend: generative work is deferred to the
//!   pending-review queue, keeping the worker LLM-free.
//!
//! Orchestration (load capsules → cluster → detect → gate → execute)
//! lives in `crate::worker::evolution_worker`.

pub mod map;
pub mod synthesis;
