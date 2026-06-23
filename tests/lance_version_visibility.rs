//! Phase-0 probe for the "remove DuckDB, keep Lance" plan
//! (`docs/remove-duckdb-keep-lance.md` §5 Phase 0 / §8 risk #1).
//!
//! QUESTION this settles deterministically: when the future lance-native
//! read engine holds a `lancedb::Table` handle, does a write committed by
//! ANOTHER handle become visible to it automatically, or must it call
//! `checkout_latest()`? The answer decides whether the heavy DuckDB-style
//! `refresh()` / `mark_dirty()` / `ensure_fresh()` machinery (a ~100ms
//! full `Connection` + extension rebuild) can be deleted, and what replaces
//! it.
//!
//! This is the lancedb-Rust-read-side analogue of
//! `tests/lance_snapshot_visibility.rs` (which probes the DuckDB-ATTACH
//! read side). Like that one, the asserts below ENCODE the expected
//! semantics from the lancedb docs (the `ReadConsistency` enum:
//! `Manual` | `Eventual(Duration)` | `Strong`); if a future lancedb /
//! arrow upgrade changes the behaviour, the relevant assert flips and
//! forces us to revisit the read-engine design.
//!
//! ── CONCLUSION (encoded as the asserts; re-confirmed empirically here) ──
//!  1. A Table instance ALWAYS sees its OWN writes (internal consistency).
//!  2. DEFAULT connection (`Manual`): a separate reader handle does NOT see
//!     another handle's commit until `checkout_latest()`. So a *warm* read
//!     handle on a default connection needs an explicit refresh — but that
//!     refresh is `checkout_latest()` (cheap: reads the latest manifest,
//!     reuses the connection/object-store), NOT DuckDB's 100ms rebuild.
//!  3. `read_consistency_interval(ZERO)` (`Strong`): a warm reader handle
//!     sees writes IMMEDIATELY with no manual call — every read re-checks
//!     the latest version.
//!  4. A FRESH `open_table` after the write always sees it.
//!
//!  ⇒ The DuckDB `refresh/mark_dirty` machinery CAN be deleted. The
//!    lance-native read path can pick either: (a) open the read connection
//!    with `read_consistency_interval(ZERO)` for transparent freshness, or
//!    (b) `open_table` per read, or (c) keep warm handles + a dirty flag +
//!    `checkout_latest()`. All three are far cheaper than the current
//!    full-connection rebuild.

use std::sync::Arc;
use std::time::Duration;

use arrow_array::builder::{Int32Builder, StringBuilder};
use arrow_array::{Array, RecordBatch};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};

fn items_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("status", DataType::Utf8, false),
    ]))
}

fn rows_batch(schema: &Arc<Schema>, ids: &[(i32, &str)]) -> RecordBatch {
    let mut idb = Int32Builder::new();
    let mut sb = StringBuilder::new();
    for (id, status) in ids {
        idb.append_value(*id);
        sb.append_value(status);
    }
    RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(idb.finish()) as Arc<dyn Array>,
            Arc::new(sb.finish()),
        ],
    )
    .unwrap()
}

/// CASE 1 — DEFAULT connection = `ReadConsistency::Manual`.
/// A warm reader handle does NOT see another handle's commit until
/// `checkout_latest()`. Covers both APPEND and UPDATE (the hard case).
#[tokio::test]
async fn default_connection_is_manual_needs_checkout_latest() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().to_str().unwrap();
    let schema = items_schema();

    // Default connection: read_consistency_interval unset → Manual.
    let db = lancedb::connect(uri).execute().await.unwrap();
    let writer = db
        .create_table(
            "items",
            rows_batch(&schema, &[(1, "pending"), (2, "pending"), (3, "pending")]),
        )
        .execute()
        .await
        .unwrap();

    // A SEPARATE reader handle, opened before the writes, with one read to
    // pin its view.
    let reader = db.open_table("items").execute().await.unwrap();
    assert_eq!(reader.count_rows(None).await.unwrap(), 3, "baseline rows");
    assert_eq!(
        reader
            .count_rows(Some("status = 'active'".to_string()))
            .await
            .unwrap(),
        0,
        "baseline active"
    );

    // ── APPEND id=4 via the WRITER handle ────────────────────────────
    writer
        .add(rows_batch(&schema, &[(4, "pending")]))
        .execute()
        .await
        .unwrap();

    // (1) writer sees its OWN write immediately (internal consistency).
    assert_eq!(
        writer.count_rows(None).await.unwrap(),
        4,
        "a Table instance must see its own append immediately"
    );
    // (2) the warm reader does NOT, on a Manual (default) connection.
    assert_eq!(
        reader.count_rows(None).await.unwrap(),
        3,
        "Manual connection: warm reader must NOT see another handle's append without checkout_latest"
    );
    // (3) checkout_latest makes it visible (the cheap refresh primitive).
    reader.checkout_latest().await.unwrap();
    assert_eq!(
        reader.count_rows(None).await.unwrap(),
        4,
        "checkout_latest must surface the append on a Manual connection"
    );

    // ── UPDATE id=1 → 'active' via the WRITER handle (the hard case) ──
    writer
        .update()
        .only_if("id = 1")
        .column("status", "'active'")
        .execute()
        .await
        .unwrap();

    // warm reader (already checked-out once) is pinned again at that version.
    assert_eq!(
        reader
            .count_rows(Some("status = 'active'".to_string()))
            .await
            .unwrap(),
        0,
        "Manual connection: warm reader must NOT see another handle's update without a new checkout_latest"
    );
    reader.checkout_latest().await.unwrap();
    assert_eq!(
        reader
            .count_rows(Some("status = 'active'".to_string()))
            .await
            .unwrap(),
        1,
        "checkout_latest must surface the update on a Manual connection"
    );

    // (4) a FRESH open_table after the writes always sees the latest.
    let fresh = db.open_table("items").execute().await.unwrap();
    assert_eq!(
        fresh.count_rows(None).await.unwrap(),
        4,
        "a fresh open_table must see the append"
    );
    assert_eq!(
        fresh
            .count_rows(Some("status = 'active'".to_string()))
            .await
            .unwrap(),
        1,
        "a fresh open_table must see the update"
    );
}

/// CASE 2 — `read_consistency_interval(ZERO)` = `ReadConsistency::Strong`.
/// A warm reader handle sees another handle's commits IMMEDIATELY, with no
/// manual `checkout_latest`. This is the drop-in replacement for the DuckDB
/// `refresh/mark_dirty` machinery: transparent freshness, per-read manifest
/// check (cheap) instead of a full connection rebuild.
#[tokio::test]
async fn strong_consistency_connection_sees_writes_without_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().to_str().unwrap();
    let schema = items_schema();

    let db = lancedb::connect(uri)
        .read_consistency_interval(Duration::ZERO)
        .execute()
        .await
        .unwrap();
    let writer = db
        .create_table(
            "items",
            rows_batch(&schema, &[(1, "pending"), (2, "pending"), (3, "pending")]),
        )
        .execute()
        .await
        .unwrap();

    let reader = db.open_table("items").execute().await.unwrap();
    assert_eq!(reader.count_rows(None).await.unwrap(), 3, "baseline rows");

    // APPEND via writer → reader sees it WITHOUT checkout_latest.
    writer
        .add(rows_batch(&schema, &[(4, "pending")]))
        .execute()
        .await
        .unwrap();
    assert_eq!(
        reader.count_rows(None).await.unwrap(),
        4,
        "Strong consistency (interval=0): warm reader must see the append with NO manual checkout"
    );

    // UPDATE via writer → reader sees it WITHOUT checkout_latest.
    writer
        .update()
        .only_if("id = 1")
        .column("status", "'active'")
        .execute()
        .await
        .unwrap();
    assert_eq!(
        reader
            .count_rows(Some("status = 'active'".to_string()))
            .await
            .unwrap(),
        1,
        "Strong consistency (interval=0): warm reader must see the update with NO manual checkout"
    );
}
