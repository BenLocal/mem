//! ClickHouse storage backend (opt-in via the `clickhouse` cargo feature +
//! `MEM_BACKEND=clickhouse`). Peer to the default Lance (lance-native)
//! stack and the Postgres spike — see `docs/clickhouse-backend.md`.
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P2).** P1 implemented [`CapsuleStore`] (versioned
//! insert + `FINAL` reads over a `ReplacingMergeTree`); **P2** adds the
//! other 10 sub-traits as `unimplemented!()` stubs ([`stubs`]) so the
//! blanket `impl<T> Backend for T` applies and [`ClickHouseBackend`] can
//! erase to `Arc<dyn Backend>` — wired into `app::from_config`. The stub
//! bodies are filled in P3 (vectors) / P4 (search) / P5 (the rest). The
//! whole module sits behind `#[cfg(feature = "clickhouse")]`, so the
//! default build pulls neither the `clickhouse` crate nor this code.
//!
//! [`CapsuleStore`]: crate::storage::capsule_store::CapsuleStore

mod backend;
mod capsule_store;
mod stubs;

pub use backend::ClickHouseBackend;
