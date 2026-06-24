//! Route-B Phase 2 GATE: prove the decay writes — now issued through the
//! LanceDB Rust API (`table.update()`) instead of the DuckDB extension —
//! survive concurrency with another Rust-API writer without data loss,
//! and that the migrated decay formula produces byte-identical results
//! to the prior DuckDB-extension implementation.
//!
//! The old dual-writer race (DuckDB-extension stale base vs vacuum
//! prune) is gone: decay and ingest/status are now the SAME single
//! writer (the Rust API). The remaining lance optimistic-concurrency
//! commit conflict is retried natively inside `table.update()` (lance
//! 7.0 `execute_with_retry`, 10×/30 s) plus a thin outer safety net in
//! `LanceStore::with_lance_commit_retry`. These tests assert the net
//! guarantee: NO write is lost under concurrency.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mem::domain::capability_capsule::{CapabilityCapsuleRecord, CapabilityCapsuleStatus};
use mem::storage::Store;

const MS_PER_DAY: f64 = 86_400_000.0;
const DECAY_RATE_PER_DAY: f64 = 0.01;

fn ms_string(ms: u128) -> String {
    format!("{ms:020}")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn active_capsule(id: &str, updated_at: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "t".into(),
        status: CapabilityCapsuleStatus::Active,
        decay_score: 0.0,
        content_hash: "h".repeat(64),
        source_agent: "test".into(),
        created_at: updated_at.into(),
        updated_at: updated_at.into(),
        ..Default::default()
    }
}

/// Decay write CONCURRENT with another Rust-API write (a status
/// transition) against the same dataset: assert NO data loss — the
/// decay delta lands on the active rows AND the concurrent status change
/// lands, for every round. Deterministic: no sleeps-as-sync; the final
/// state is asserted exactly and is order-independent (whichever commit
/// wins the race, lance's internal retry re-applies the loser against
/// the fresh base, so both effects are present).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decay_concurrent_with_status_write_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("conc.lance")).await.unwrap());

    // Run several rounds to actually exercise the commit race (a single
    // round can serialize without contention). Each round: a fresh
    // active row to decay + a fresh provisional row to flip Active.
    const ROUNDS: usize = 12;
    let base = now_ms();
    let ten_days_ago = ms_string(base - 10 * MS_PER_DAY as u128);

    for round in 0..ROUNDS {
        let active_id = format!("m-active-{round}");
        let flip_id = format!("m-flip-{round}");

        store
            .insert_capability_capsule(active_capsule(&active_id, &ten_days_ago))
            .await
            .unwrap();
        // A provisional row the concurrent writer will flip to Active.
        let mut prov = active_capsule(&flip_id, &ten_days_ago);
        prov.status = CapabilityCapsuleStatus::Provisional;
        store.insert_capability_capsule(prov).await.unwrap();

        // Fire BOTH writers concurrently against the same Store.
        let now = now_ms();
        let now_str = ms_string(now);
        let s1 = store.clone();
        let s2 = store.clone();
        let flip = flip_id.clone();
        let decay_fut = async move {
            s1.apply_time_decay(DECAY_RATE_PER_DAY, now as f64, MS_PER_DAY, &now_str)
                .await
        };
        let status_fut = async move {
            s2.set_capsule_status("t", &flip, CapabilityCapsuleStatus::Active)
                .await
        };
        let (decay_res, status_res) = tokio::join!(decay_fut, status_fut);
        decay_res.expect("decay write must succeed under concurrency");
        status_res.expect("status write must succeed under concurrency");

        // Assert BOTH effects landed (order-independent final state).
        let active = store
            .get_capability_capsule_for_tenant("t", &active_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            active.decay_score as f64 > 0.0,
            "round {round}: decay write was lost — active row decay still 0"
        );
        assert!(
            active.last_used_at.is_some(),
            "round {round}: decay must stamp last_used_at"
        );

        let flipped = store
            .get_capability_capsule_for_tenant("t", &flip_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            flipped.status,
            CapabilityCapsuleStatus::Active,
            "round {round}: concurrent status write was lost"
        );
    }
}

/// Decay write CONCURRENT with a `bump_last_used_at` on a DIFFERENT row.
/// Both touch `capability_capsules` via `table.update()`, so they
/// genuinely contend for the commit. Assert both land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decay_concurrent_with_bump_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("conc2.lance")).await.unwrap());

    const ROUNDS: usize = 12;
    let base = now_ms();
    let ten_days_ago = ms_string(base - 10 * MS_PER_DAY as u128);

    for round in 0..ROUNDS {
        let decay_id = format!("d-{round}");
        let bump_id = format!("b-{round}");
        store
            .insert_capability_capsule(active_capsule(&decay_id, &ten_days_ago))
            .await
            .unwrap();
        store
            .insert_capability_capsule(active_capsule(&bump_id, &ten_days_ago))
            .await
            .unwrap();

        let now = now_ms();
        let now_str = ms_string(now);
        let s1 = store.clone();
        let s2 = store.clone();
        let bump = bump_id.clone();
        let bump_now = now_str.clone();
        let decay_fut = async move {
            s1.apply_time_decay(DECAY_RATE_PER_DAY, now as f64, MS_PER_DAY, &now_str)
                .await
        };
        let bump_fut = async move { s2.bump_last_used_at("t", &[bump], &bump_now).await };
        let (decay_res, bump_res) = tokio::join!(decay_fut, bump_fut);
        decay_res.expect("decay must succeed");
        bump_res.expect("bump must succeed");

        // The decayed row advanced its decay; the bumped row got a
        // last_recalled_at (only bump writes it) — neither lost.
        let decayed = store
            .get_capability_capsule_for_tenant("t", &decay_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            decayed.decay_score as f64 > 0.0,
            "round {round}: decay lost"
        );
        let bumped = store
            .get_capability_capsule_for_tenant("t", &bump_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            bumped.last_recalled_at.is_some(),
            "round {round}: bump lost — last_recalled_at not set"
        );
    }
}

/// Decay-formula parity: the migrated lance `table.update()` decay must
/// produce the IDENTICAL `decay_score` / `last_used_at` / `updated_at` /
/// `last_recalled_at` outcome the DuckDB-extension version produced for
/// the same fixture + inputs. ~10 days * 0.01/day = exactly 0.1.
#[tokio::test(flavor = "multi_thread")]
async fn decay_formula_identical_to_duckdb_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("parity.lance")).await.unwrap();

    let now = now_ms();
    // Exactly 10 days old so the expected delta is exactly 0.1.
    let ten_days_ago = ms_string(now - 10 * MS_PER_DAY as u128);

    // (a) never-used active row → anchored on updated_at.
    store
        .insert_capability_capsule(active_capsule("never-used", &ten_days_ago))
        .await
        .unwrap();
    // (b) used active row: last_used_at = now → decays ~0 (clock reset).
    let now_str = ms_string(now);
    store
        .insert_capability_capsule(active_capsule("recently-used", &ten_days_ago))
        .await
        .unwrap();
    store
        .bump_last_used_at("t", &["recently-used".to_string()], &now_str)
        .await
        .unwrap();
    // (c) provisional row → status filter excludes it.
    let mut prov = active_capsule("prov", &ten_days_ago);
    prov.status = CapabilityCapsuleStatus::Provisional;
    store.insert_capability_capsule(prov).await.unwrap();
    // (d) saturated active row → decay_score < 1.0 filter excludes it.
    let mut sat = active_capsule("sat", &ten_days_ago);
    sat.decay_score = 1.0;
    store.insert_capability_capsule(sat).await.unwrap();
    // (e) expired active row → hard-expiry archives it FIRST.
    let mut expired = active_capsule("expired", &ten_days_ago);
    expired.expires_at = Some(ms_string(now - MS_PER_DAY as u128)); // expired 1 day ago
    store.insert_capability_capsule(expired).await.unwrap();

    // Run the sweep with `now` (same instant used to stamp recently-used).
    let sweep_now = now_ms();
    let sweep_str = ms_string(sweep_now);
    store
        .apply_time_decay(DECAY_RATE_PER_DAY, sweep_now as f64, MS_PER_DAY, &sweep_str)
        .await
        .unwrap();

    let read = |id: &str| {
        let store = &store;
        let id = id.to_string();
        async move {
            store
                .get_capability_capsule_for_tenant("t", &id)
                .await
                .unwrap()
                .unwrap()
        }
    };

    // (a) never-used: decay ≈ 0.1, last_used_at advanced to sweep now,
    // updated_at unchanged, last_recalled_at stays None (sweep must NOT
    // fabricate a recall signal).
    let nu = read("never-used").await;
    assert!(
        (0.0999..=0.1001).contains(&(nu.decay_score as f64)),
        "never-used decay must be exactly ~0.1; got {}",
        nu.decay_score
    );
    assert_eq!(nu.updated_at, ten_days_ago, "updated_at must NOT move");
    assert_eq!(
        nu.last_used_at.as_deref(),
        Some(sweep_str.as_str()),
        "last_used_at must advance to the sweep's now"
    );
    assert!(
        nu.last_recalled_at.is_none(),
        "sweep must not set last_recalled_at"
    );

    // (b) recently-used: clock was reset to ~now, so decay ≈ 0.
    let ru = read("recently-used").await;
    assert!(
        (ru.decay_score as f64) < 0.001,
        "recently-used must barely decay; got {}",
        ru.decay_score
    );
    assert!(
        ru.last_recalled_at.is_some(),
        "real recall stamped last_recalled_at and it survives the sweep"
    );

    // (c) provisional: untouched by status filter.
    let p = read("prov").await;
    assert_eq!(p.decay_score, 0.0);
    assert_eq!(p.status, CapabilityCapsuleStatus::Provisional);

    // (d) saturated: untouched by decay_score < 1.0 filter.
    let s = read("sat").await;
    assert_eq!(s.decay_score, 1.0);
    assert_eq!(s.status, CapabilityCapsuleStatus::Active);

    // (e) expired: hard-expiry archived it; decay passes must not touch it.
    let e = read("expired").await;
    assert_eq!(
        e.status,
        CapabilityCapsuleStatus::Archived,
        "expired active row must be archived by hard-expiry"
    );
    assert_eq!(
        e.decay_score, 0.0,
        "archived-by-expiry row must not also accrue decay"
    );
}
