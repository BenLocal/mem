//! ClickHouse storage backend (opt-in via the `clickhouse` cargo feature +
//! `MEM_BACKEND=clickhouse`). Peer to the default Lance (lance-native)
//! stack and the Postgres spike — see `docs/clickhouse-backend.md`.
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P1).** P1 wires only [`CapsuleStore`] on
//! [`ClickHouseBackend`] (versioned-insert + `FINAL`/argMax reads over a
//! `ReplacingMergeTree`); the other 10 sub-traits and the full `Backend`
//! umbrella land in P2+. The whole module sits behind
//! `#[cfg(feature = "clickhouse")]`, so the default build pulls neither the
//! `clickhouse` crate nor this code.
//!
//! [`CapsuleStore`]: crate::storage::capsule_store::CapsuleStore

mod backend;
mod capsule_store;

pub use backend::ClickHouseBackend;
