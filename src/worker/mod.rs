//! Background workers spawned at process startup by `app::AppState`.
//!
//! Each worker holds an `Arc<DuckDbRepository>` (or equivalent) and ticks
//! at its own cadence. They sit alongside `service::*` (request-driven
//! HTTP handlers): services run synchronously inside a request future,
//! workers run forever in their own tokio tasks.
//!
//! - `decay_worker`             — applies time-based confidence/decay updates to memories
//! - `embedding_worker`         — drains `embedding_jobs`, calls `embed_batch`, upserts HNSW
//! - `transcript_embedding_worker` — same but for `transcript_embedding_jobs`
//!
//! There is no `fts_worker` — the BM25 index is now incremental
//! (tantivy, see `storage::fts`); writes upsert directly, no rebuild
//! cycle, no background task needed.

pub mod decay_worker;
pub mod embedding_worker;
pub mod transcript_embedding_worker;
