//! Postgres storage backend (opt-in via the `postgres` feature +
//! `MEM_BACKEND=postgres`). Peer to the default Lance + DuckDB stack.
//!
//! - [`capsule_store`] — `PostgresCapsuleStore`: the `PgPool` wrapper +
//!   `CapsuleStore` impl + migration bootstrap (`connect` / `connect_fresh`).
//! - [`backend`] — `PostgresBackend`: the remaining 10 `Backend`
//!   sub-traits (search / embeddings / graph / transcripts / jobs /
//!   entity / session / maintenance / cursor / evolution) over the same pool.

pub mod backend;
pub mod capsule_store;

pub use capsule_store::PostgresCapsuleStore;
