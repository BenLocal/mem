//! Background workers spawned at process startup by `app::AppState`.
//!
//! Each worker holds an `Arc<Store>` and ticks at its own cadence.
//! They sit alongside `service::*` (request-driven HTTP handlers):
//! services run synchronously inside a request future, workers run
//! forever in their own tokio tasks.
//!
//! - `decay_worker` — bulk SQL UPDATE of `memories.decay_score`
//!   (active rows only, capped at 1.0). Goes through
//!   `Store::apply_time_decay` (DuckDB SQL via the lance extension).
//! - `embedding_worker` — drains `embedding_jobs`, calls
//!   `embed_batch`, upserts to `memory_embeddings`. Lance handles
//!   vector indexing internally — no separate HNSW sidecar to
//!   update.
//! - `transcript_embedding_worker` — same shape for
//!   `transcript_embedding_jobs` → `conversation_message_embeddings`.
//!
//! There is no `fts_worker` — BM25 index is built once at
//! `LanceStore::open` time on `(memories, content)` and
//! `(conversation_messages, content)` via the lance extension's
//! native FTS. Writes update the inverted index automatically.

pub mod decay_worker;
pub mod embedding_worker;
pub mod transcript_embedding_worker;
