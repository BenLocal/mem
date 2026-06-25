//! ClickHouse storage backend (opt-in via the `clickhouse` cargo feature +
//! `MEM_BACKEND=clickhouse`). Peer to the default Lance (lance-native)
//! stack and the Postgres spike — see `docs/clickhouse-backend.md`.
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P2).** P1 implemented [`CapsuleStore`] (versioned
//! insert + `FINAL` reads over a `ReplacingMergeTree`); **P2** adds the
//! other 10 sub-traits as `unimplemented!()` stubs ([`stubs`]) so the
//! blanket `impl<T> Backend for T` applies and [`ClickHouseBackend`] can
//! erase to `Arc<dyn Backend>` — wired into `app::from_config`. **P3** fills
//! [`EmbeddingVectorStore`] for real ([`embedding`]); **P4** fills
//! [`CapsuleSearchStore`] ([`search`] — hybrid recall: lexical candidate +
//! `cosineDistance` ANN + Rust-side RRF); P5 (the rest) remain
//! `unimplemented!()` stubs. The whole module sits behind
//! `#[cfg(feature = "clickhouse")]`, so the default build pulls neither the
//! `clickhouse` crate nor this code.
//!
//! [`CapsuleStore`]: crate::storage::capsule_store::CapsuleStore
//! [`EmbeddingVectorStore`]: crate::storage::embedding_vector_store::EmbeddingVectorStore

mod backend;
mod capsule_store;
mod embedding;
mod search;
mod stubs;

pub use backend::ClickHouseBackend;
