//! Postgres backend integration tests (postgres-backend.md P1).
//!
//! Gated on the `postgres` cargo feature AND a reachable test database:
//! every test reads `MEM_TEST_POSTGRES_URL` and **skips** (prints +
//! returns) when it is unset, so the default `cargo test` (no feature,
//! no DB) stays green and CI's `rust` job is unaffected. To run:
//!
//! ```bash
//! docker run -d --name mem-pg -e POSTGRES_PASSWORD=mem -e POSTGRES_DB=mem \
//!   -p 5433:5432 pgvector/pgvector:pg16
//! MEM_TEST_POSTGRES_URL=postgres://postgres:mem@127.0.0.1:5433/mem \
//!   cargo test --features postgres --test postgres_backend
//! ```
//!
//! P1 validates the existing `PostgresCapsuleStore` scaffold (the
//! `CapsuleStore` trait) against a real Postgres — the Phase-4 spike
//! validation that the doc said "needs Docker + testcontainers" and
//! never ran. Cases mirror `capsule_store_parity.rs`.
#![cfg(feature = "postgres")]

use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
};
use mem::domain::entity::EntityKind;
use mem::domain::episode::EpisodeRecord;
use mem::domain::AddAliasOutcome;
use mem::storage::{
    current_timestamp, CapsuleSearchStore, CapsuleStore, EmbeddingVectorStore, EntityRegistry,
    EvolutionCandidate, EvolutionCandidateStore, FeedbackEvent, MaintenanceStore, MineCursorStore,
    PostgresCapsuleStore, SessionStore,
};

/// `Some(store)` on a fresh schema when `MEM_TEST_POSTGRES_URL` is set,
/// else `None` (caller skips). Each call drops + re-applies the schema
/// so tests are order-independent.
async fn backend() -> Option<Arc<dyn CapsuleStore>> {
    let url = std::env::var("MEM_TEST_POSTGRES_URL").ok()?;
    let store = PostgresCapsuleStore::connect_fresh(&url)
        .await
        .expect("connect + migrate test postgres");
    Some(Arc::new(store))
}

/// Like [`backend`] but returns the concrete store so we can both call
/// `EmbeddingVectorStore` methods AND reach `pool()` for raw pgvector
/// ANN queries. `None` (skip) when `MEM_TEST_POSTGRES_URL` is unset.
async fn embedding_backend() -> Option<PostgresCapsuleStore> {
    let url = std::env::var("MEM_TEST_POSTGRES_URL").ok()?;
    Some(
        PostgresCapsuleStore::connect_fresh(&url)
            .await
            .expect("connect + migrate test postgres"),
    )
}

/// f32 vector of length `dim` with `v[i] = vals[i]` and zeros after.
fn vec_of(dim: usize, vals: &[f32]) -> Vec<f32> {
    let mut v = vec![0.0_f32; dim];
    for (i, x) in vals.iter().enumerate() {
        v[i] = *x;
    }
    v
}

/// Native-endian f32 blob (matches `crate::embedding::wire::encode_f32_blob`).
fn blob_of(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_ne_bytes());
    }
    out
}

macro_rules! pg_test {
    ($name:ident, $backend:ident, $body:block) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some($backend) = backend().await else {
                eprintln!("skip {}: MEM_TEST_POSTGRES_URL unset", stringify!($name));
                return;
            };
            $body
        }
    };
}

fn fixture(id: &str, status: CapabilityCapsuleStatus) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "t".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status,
        scope: Scope::Repo,
        visibility: Visibility::Private,
        version: 1,
        summary: format!("summary-{id}"),
        content: format!("content-{id}"),
        content_hash: format!("{id:0>64}"),
        confidence: 0.5,
        decay_score: 0.0,
        source_agent: "test".into(),
        created_at: "00000000000000000000".into(),
        updated_at: "00000000000000000000".into(),
        ..Default::default()
    }
}

pg_test!(insert_and_get_round_trip, backend, {
    let row = fixture("a", CapabilityCapsuleStatus::Active);
    backend
        .insert_capability_capsule(row.clone())
        .await
        .unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("t", "a")
        .await
        .unwrap()
        .expect("tenant-scoped get should find the row");
    assert_eq!(got.capability_capsule_id, "a");
    assert_eq!(got.tenant, "t");
    assert_eq!(got.status, CapabilityCapsuleStatus::Active);
    assert_eq!(got.content, "content-a");
});

pg_test!(get_for_other_tenant_returns_none, backend, {
    backend
        .insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("other", "a")
        .await
        .unwrap();
    assert!(got.is_none(), "cross-tenant get must not leak");
});

pg_test!(accept_pending_transitions_status, backend, {
    backend
        .insert_capability_capsule(fixture("p", CapabilityCapsuleStatus::PendingConfirmation))
        .await
        .unwrap();
    backend.accept_pending("t", "p").await.unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("t", "p")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.status, CapabilityCapsuleStatus::Active);
});

pg_test!(list_pending_review_filters_status, backend, {
    backend
        .insert_capability_capsule(fixture("act", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    backend
        .insert_capability_capsule(fixture(
            "pend",
            CapabilityCapsuleStatus::PendingConfirmation,
        ))
        .await
        .unwrap();
    let pending = backend.list_pending_review("t").await.unwrap();
    let ids: Vec<&str> = pending
        .iter()
        .map(|c| c.capability_capsule_id.as_str())
        .collect();
    assert_eq!(ids, vec!["pend"], "only PendingConfirmation rows listed");
});

pg_test!(find_by_idempotency_dedups_on_hash, backend, {
    let mut row = fixture("h", CapabilityCapsuleStatus::Active);
    row.content_hash = "deadbeef".into();
    backend.insert_capability_capsule(row).await.unwrap();
    let hit = backend
        .find_by_idempotency_or_hash("t", &None, "deadbeef")
        .await
        .unwrap();
    assert!(hit.is_some(), "existing content_hash should dedup");
    let miss = backend
        .find_by_idempotency_or_hash("t", &None, "00000000")
        .await
        .unwrap();
    assert!(miss.is_none(), "unknown hash is not a dup");
});

pg_test!(apply_feedback_moves_confidence, backend, {
    let row = fixture("f", CapabilityCapsuleStatus::Active);
    backend
        .insert_capability_capsule(row.clone())
        .await
        .unwrap();
    let before = backend
        .get_capability_capsule_for_tenant("t", "f")
        .await
        .unwrap()
        .unwrap()
        .confidence;
    let event = FeedbackEvent {
        feedback_id: "fb_1".into(),
        capability_capsule_id: "f".into(),
        feedback_kind: "useful".into(),
        created_at: current_timestamp(),
        note: None,
    };
    backend.apply_feedback(&row, event).await.unwrap();
    let after = backend
        .get_capability_capsule_for_tenant("t", "f")
        .await
        .unwrap()
        .unwrap()
        .confidence;
    assert!(
        after > before,
        "useful feedback raises confidence ({before} -> {after})"
    );
});

/// P2 smoke test: `mem serve` boots on the Postgres backend. Builds a
/// `Config` with `backend = Postgres` + a Fake embedding provider (so no
/// model download) and asserts `AppState::from_config` assembles cleanly
/// — i.e. the Postgres arm of `app.rs` connects, migrates, wires up the
/// services/workers, and never hits an `unimplemented!()` stub on the
/// startup path. Skips when `MEM_TEST_POSTGRES_URL` is unset.
#[tokio::test(flavor = "multi_thread")]
async fn serve_boots_on_postgres() {
    let Some(url) = std::env::var("MEM_TEST_POSTGRES_URL").ok() else {
        eprintln!("skip serve_boots_on_postgres: MEM_TEST_POSTGRES_URL unset");
        return;
    };
    let mut config = mem::config::Config::local();
    config.backend = mem::config::BackendKind::Postgres;
    config.postgres_url = Some(url);
    config.embedding.provider = mem::config::EmbeddingProviderKind::Fake;
    config.embedding.model = "fake".to_string();
    config.embedding.dim = 64;

    let state = mem::app::AppState::from_config(config).await;
    assert!(
        state.is_ok(),
        "AppState::from_config should assemble on the Postgres backend: {:?}",
        state.err()
    );
}

// ───────────────────────── EmbeddingVectorStore (P3) ──────────────────────

const DIM: usize = 8;

macro_rules! emb_test {
    ($name:ident, $store:ident, $body:block) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some($store) = embedding_backend().await else {
                eprintln!("skip {}: MEM_TEST_POSTGRES_URL unset", stringify!($name));
                return;
            };
            $body
        }
    };
}

emb_test!(embedding_upsert_get_roundtrip, store, {
    let v = vec_of(DIM, &[0.1, -0.2, 0.3, 0.4, -0.5, 0.6, 0.7, -0.8]);
    store
        .upsert_capability_capsule_embedding(
            "cap1",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&v),
            "hash1",
            "src-ts",
            "now-ts",
        )
        .await
        .unwrap();
    let got = store
        .get_capability_capsule_embedding_vector("cap1")
        .await
        .unwrap()
        .expect("vector should exist after upsert");
    assert_eq!(got.len(), DIM);
    for (a, b) in got.iter().zip(v.iter()) {
        assert!((a - b).abs() < 1e-6, "elementwise: {a} vs {b}");
    }
    // metadata triple = (model, content_hash, created_at==now)
    let row = store
        .get_capability_capsule_embedding_row("cap1")
        .await
        .unwrap()
        .expect("row should exist");
    assert_eq!(row, ("fake".into(), "hash1".into(), "now-ts".into()));
});

emb_test!(embedding_chunks_replace, store, {
    let three = vec![
        vec_of(DIM, &[1.0]),
        vec_of(DIM, &[0.0, 1.0]),
        vec_of(DIM, &[0.0, 0.0, 1.0]),
    ];
    store
        .upsert_capability_capsule_embedding_chunks(
            "capc", "t", "fake", DIM as i64, &three, "h", "s", "n",
        )
        .await
        .unwrap();
    let cnt = store.count_capsule_embedding_rows("capc").await.unwrap();
    assert_eq!(cnt, 3, "first upsert writes 3 chunk rows");

    let two = vec![vec_of(DIM, &[1.0]), vec_of(DIM, &[0.0, 1.0])];
    store
        .upsert_capability_capsule_embedding_chunks(
            "capc", "t", "fake", DIM as i64, &two, "h2", "s", "n",
        )
        .await
        .unwrap();
    let cnt2 = store.count_capsule_embedding_rows("capc").await.unwrap();
    assert_eq!(cnt2, 2, "second upsert fully replaces — only 2 rows remain");
});

emb_test!(embedding_delete, store, {
    let v = vec_of(DIM, &[0.5, 0.5]);
    store
        .upsert_capability_capsule_embedding(
            "capd",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&v),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
    store
        .delete_capability_capsule_embedding("capd")
        .await
        .unwrap();
    let got = store
        .get_capability_capsule_embedding_vector("capd")
        .await
        .unwrap();
    assert!(got.is_none(), "vector gone after delete");
});

emb_test!(embedding_ann_cosine_orders_by_distance, store, {
    // [1,0,..], [0.9,0.1,..], [0,1,..]; query [1,0,..]. Nearest two by
    // cosine distance are cap_a (identical dir) then cap_b (0.9,0.1).
    store
        .upsert_capability_capsule_embedding(
            "cap_a",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, &[1.0, 0.0])),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
    store
        .upsert_capability_capsule_embedding(
            "cap_b",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, &[0.9, 0.1])),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
    store
        .upsert_capability_capsule_embedding(
            "cap_c",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, &[0.0, 1.0])),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();

    let rows = store
        .ann_nearest_capsule_ids(&vec_of(DIM, &[1.0, 0.0]), 2)
        .await
        .unwrap();
    assert_eq!(
        rows,
        vec!["cap_a".to_string(), "cap_b".to_string()],
        "pgvector cosine orders cap_a (identical) then cap_b (0.9,0.1)"
    );
});

emb_test!(conversation_embedding_roundtrip, store, {
    let v = vec_of(DIM, &[0.2, 0.4, 0.6, 0.8, -0.1, -0.3, -0.5, -0.7]);
    store
        .upsert_conversation_message_embedding(
            "msg1",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&v),
            "mh",
            "s",
            "n",
        )
        .await
        .unwrap();
    let got = store
        .get_message_embedding_vector("msg1")
        .await
        .unwrap()
        .expect("message vector should exist after upsert");
    assert_eq!(got.len(), DIM);
    for (a, b) in got.iter().zip(v.iter()) {
        assert!((a - b).abs() < 1e-6, "elementwise: {a} vs {b}");
    }
    store
        .delete_conversation_message_embedding("msg1")
        .await
        .unwrap();
    let cnt = store.count_message_embedding_rows("msg1").await.unwrap();
    assert_eq!(cnt, 0, "conversation embedding gone after delete");
});

// ───────────────────── CapsuleSearchStore — hybrid (P4) ───────────────────
//
// Verifies the Postgres lexical (tsvector) / semantic (pgvector) / RRF
// channels behave-align with the Lance backend. These assert PG's own
// behaviour (cross-backend parity is hard to scaffold; the contract is
// "same top hit set"). One case directly checks the RRF ordering against
// `1/(60+rank)` summed — the exact formula in `pipeline::retrieve::sql_rrf`.
//
// CHINESE-TOKENIZATION NOTE: the 'simple' tsvector config splits on
// whitespace/punctuation and has NO CJK segmenter, so a run of Han
// characters is one token — sub-phrase lexical recall over Chinese is weak.
// That is an accepted, documented limitation (see 0003_search.sql); pgvector
// semantic recall + RRF cover Chinese. These tests therefore exercise
// lexical recall with ASCII tokens only.

/// Capsule fixture with explicit content + type, tenant "t".
fn capsule_with(id: &str, content: &str, ty: CapabilityCapsuleType) -> CapabilityCapsuleRecord {
    let mut c = fixture(id, CapabilityCapsuleStatus::Active);
    c.capability_capsule_type = ty;
    c.content = content.into();
    c.content_hash = format!("{id:0>64}");
    c
}

/// Insert a capsule + its (single-vector) embedding so the hybrid path has
/// both channels to draw on.
async fn seed(
    store: &PostgresCapsuleStore,
    id: &str,
    content: &str,
    ty: CapabilityCapsuleType,
    vec: &[f32],
) {
    store
        .insert_capability_capsule(capsule_with(id, content, ty))
        .await
        .unwrap();
    store
        .upsert_capability_capsule_embedding(
            id,
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, vec)),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
}

emb_test!(bm25_finds_lexical_match, store, {
    store
        .insert_capability_capsule(capsule_with(
            "lex_hit",
            "configure the embedding batch size knob",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule_with(
            "lex_miss",
            "completely unrelated transcript archive note",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    let hits = store
        .bm25_candidate_ids("t", "embedding batch", 10)
        .await
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
    assert!(
        ids.contains(&"lex_hit"),
        "keyword capsule is recalled: {ids:?}"
    );
    assert!(
        !ids.contains(&"lex_miss"),
        "unrelated capsule not recalled: {ids:?}"
    );
    // ranks are 1-based and contiguous.
    assert_eq!(hits[0].1, 1, "top hit has rank 1");
});

emb_test!(bm25_empty_query_is_empty, store, {
    store
        .insert_capability_capsule(capsule_with(
            "x",
            "anything",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    assert!(store
        .bm25_candidate_ids("t", "   ", 10)
        .await
        .unwrap()
        .is_empty());
});

emb_test!(ann_finds_semantic_neighbor, store, {
    // [1,0,..] nearest to query [1,0,..]; [0.9,0.1,..] second; [0,1,..] last.
    seed(
        &store,
        "near",
        "alpha",
        CapabilityCapsuleType::Experience,
        &[1.0, 0.0],
    )
    .await;
    seed(
        &store,
        "mid",
        "beta",
        CapabilityCapsuleType::Experience,
        &[0.9, 0.1],
    )
    .await;
    seed(
        &store,
        "far",
        "gamma",
        CapabilityCapsuleType::Experience,
        &[0.0, 1.0],
    )
    .await;
    let hits = store
        .ann_candidate_ids("t", &vec_of(DIM, &[1.0, 0.0]), 3)
        .await
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["near", "mid", "far"],
        "ANN orders by cosine distance"
    );
    assert_eq!(hits[0].1, 1, "1-based rank");
    assert_eq!(hits[1].1, 2);
});

emb_test!(ann_missing_embeddings_table_is_empty, store, {
    // No embedding ever upserted → table not lazy-created → empty, no error.
    let hits = store
        .ann_candidate_ids("t", &vec_of(DIM, &[1.0, 0.0]), 5)
        .await
        .unwrap();
    assert!(
        hits.is_empty(),
        "missing embeddings table short-circuits to empty"
    );
});

emb_test!(hybrid_fuses_both, store, {
    // lex_only: matches the text query, far embedding.
    // sem_only: no keyword, embedding identical to query.
    // both:     matches text AND near embedding → must rank first by RRF.
    seed(
        &store,
        "lex_only",
        "rust async runtime tokio scheduler",
        CapabilityCapsuleType::Experience,
        &[0.0, 1.0],
    )
    .await;
    seed(
        &store,
        "sem_only",
        "an entirely different unrelated topic",
        CapabilityCapsuleType::Experience,
        &[1.0, 0.0],
    )
    .await;
    seed(
        &store,
        "both",
        "rust async runtime done right",
        CapabilityCapsuleType::Experience,
        &[1.0, 0.0],
    )
    .await;

    let fused = store
        .hybrid_candidates("t", "rust async runtime", &vec_of(DIM, &[1.0, 0.0]), 10)
        .await
        .unwrap();
    let ids: Vec<&str> = fused
        .iter()
        .map(|(c, _)| c.capability_capsule_id.as_str())
        .collect();
    assert_eq!(
        ids.first(),
        Some(&"both"),
        "dual-channel hit ranks first: {ids:?}"
    );
    assert!(
        ids.contains(&"lex_only") && ids.contains(&"sem_only"),
        "single-channel hits still surface: {ids:?}"
    );
});

emb_test!(hybrid_rrf_score_matches_formula, store, {
    // Two capsules, distinct embeddings, no shared lexical token, query that
    // matches NEITHER lexically → the fused score is purely the ANN channel:
    // each capsule's rrf == 1/(60+rank_sem). Assert that exact value.
    seed(
        &store,
        "v1",
        "qqqqq",
        CapabilityCapsuleType::Experience,
        &[1.0, 0.0],
    )
    .await;
    seed(
        &store,
        "v2",
        "wwwww",
        CapabilityCapsuleType::Experience,
        &[0.0, 1.0],
    )
    .await;
    let fused = store
        .hybrid_candidates("t", "zzzzz", &vec_of(DIM, &[1.0, 0.0]), 10)
        .await
        .unwrap();
    let by_id: std::collections::HashMap<&str, f32> = fused
        .iter()
        .map(|(c, s)| (c.capability_capsule_id.as_str(), *s))
        .collect();
    // v1 is the nearest neighbor (rank_sem 1), v2 is rank_sem 2.
    let expect_rank1 = 1.0_f32 / (60.0 + 1.0);
    let expect_rank2 = 1.0_f32 / (60.0 + 2.0);
    assert!(
        (by_id["v1"] - expect_rank1).abs() < 1e-6,
        "v1 rrf == 1/(60+1): {}",
        by_id["v1"]
    );
    assert!(
        (by_id["v2"] - expect_rank2).abs() < 1e-6,
        "v2 rrf == 1/(60+2): {}",
        by_id["v2"]
    );
    // and the order respects the score.
    assert_eq!(fused[0].0.capability_capsule_id, "v1");
});

emb_test!(search_candidates_respects_status_and_diary, store, {
    store
        .insert_capability_capsule(capsule_with(
            "active1",
            "live capsule",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    store
        .insert_capability_capsule(fixture("arch1", CapabilityCapsuleStatus::Archived))
        .await
        .unwrap();
    store
        .insert_capability_capsule(fixture("rej1", CapabilityCapsuleStatus::Rejected))
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule_with(
            "diary1",
            "diary entry",
            CapabilityCapsuleType::Diary,
        ))
        .await
        .unwrap();

    let pool = store.search_candidates("t").await.unwrap();
    let ids: Vec<&str> = pool
        .iter()
        .map(|c| c.capability_capsule_id.as_str())
        .collect();
    assert!(ids.contains(&"active1"), "active capsule in pool: {ids:?}");
    assert!(!ids.contains(&"arch1"), "archived excluded: {ids:?}");
    assert!(!ids.contains(&"rej1"), "rejected excluded: {ids:?}");
    assert!(!ids.contains(&"diary1"), "diary excluded: {ids:?}");
});

emb_test!(search_candidates_dedups_superseded, store, {
    // old <- new(supersedes old, active). search_candidates must drop `old`.
    store
        .insert_capability_capsule(capsule_with(
            "old",
            "version one",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    let mut new = capsule_with("new", "version two", CapabilityCapsuleType::Experience);
    new.supersedes_capability_capsule_id = Some("old".into());
    store.insert_capability_capsule(new).await.unwrap();

    let pool = store.search_candidates("t").await.unwrap();
    let ids: Vec<&str> = pool
        .iter()
        .map(|c| c.capability_capsule_id.as_str())
        .collect();
    assert!(ids.contains(&"new"), "superseder present: {ids:?}");
    assert!(
        !ids.contains(&"old"),
        "superseded-by-active row dropped: {ids:?}"
    );
});

emb_test!(hybrid_excludes_archived_vec_only_hit, store, {
    // Archived capsule with an embedding that matches the query: the ANN
    // channel finds it, but the outer hydration filter must drop it (same as
    // the Lance hybrid outer WHERE).
    let mut arch = capsule_with("arch_vec", "qqq", CapabilityCapsuleType::Experience);
    arch.status = CapabilityCapsuleStatus::Archived;
    store.insert_capability_capsule(arch).await.unwrap();
    store
        .upsert_capability_capsule_embedding(
            "arch_vec",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, &[1.0, 0.0])),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
    let fused = store
        .hybrid_candidates("t", "", &vec_of(DIM, &[1.0, 0.0]), 10)
        .await
        .unwrap();
    let ids: Vec<&str> = fused
        .iter()
        .map(|(c, _)| c.capability_capsule_id.as_str())
        .collect();
    assert!(
        !ids.contains(&"arch_vec"),
        "archived vec-only hit dropped post-fusion: {ids:?}"
    );
});

// ═══════════════════════ P5 batch-1 sub-trait tests ═════════════════════
//
// MineCursorStore / EvolutionCandidateStore / SessionStore /
// EntityRegistry / MaintenanceStore — round-trip + behaviour parity
// against a real Postgres on a fresh per-test schema.

// ───────────────────────────── MineCursorStore ──────────────────────────

emb_test!(mine_cursor_upsert_get_and_monotonic, store, {
    // No row yet → None.
    assert!(store.get_mine_cursor("/t/a.jsonl").await.unwrap().is_none());

    // First upsert creates the row.
    store
        .upsert_mine_cursor("/t/a.jsonl", 10, "00000000000000000001")
        .await
        .unwrap();
    let c = store
        .get_mine_cursor("/t/a.jsonl")
        .await
        .unwrap()
        .expect("cursor exists after upsert");
    assert_eq!(c.transcript_path, "/t/a.jsonl");
    assert_eq!(c.last_line_number, 10);
    assert_eq!(c.updated_at, "00000000000000000001");

    // Second upsert on the same path replaces in place (ON CONFLICT) —
    // advancing the high-water mark, not inserting a duplicate.
    store
        .upsert_mine_cursor("/t/a.jsonl", 42, "00000000000000000002")
        .await
        .unwrap();
    let c2 = store.get_mine_cursor("/t/a.jsonl").await.unwrap().unwrap();
    assert_eq!(c2.last_line_number, 42);
    assert_eq!(c2.updated_at, "00000000000000000002");

    // A different path is an independent row.
    store
        .upsert_mine_cursor("/t/b.jsonl", 5, "00000000000000000003")
        .await
        .unwrap();
    assert_eq!(
        store
            .get_mine_cursor("/t/b.jsonl")
            .await
            .unwrap()
            .unwrap()
            .last_line_number,
        5
    );
    // /t/a.jsonl unaffected.
    assert_eq!(
        store
            .get_mine_cursor("/t/a.jsonl")
            .await
            .unwrap()
            .unwrap()
            .last_line_number,
        42
    );
});

// ─────────────────────── EvolutionCandidateStore ────────────────────────

fn evo_candidate(id: &str, tenant: &str, status: &str) -> EvolutionCandidate {
    EvolutionCandidate {
        candidate_id: id.into(),
        tenant: tenant.into(),
        op_kind: "merge".into(),
        member_ids: vec!["m1".into(), "m2".into()],
        params: r#"{"threshold":0.9}"#.into(),
        evidence: 1.5,
        consecutive_cycles: 2,
        status: status.into(),
        first_proposed_at: "00000000000000000001".into(),
        last_signal_at: "00000000000000000002".into(),
        executed_at: None,
        result_capsule_ids: vec![],
    }
}

emb_test!(evolution_candidate_upsert_and_list, store, {
    store
        .upsert_evolution_candidate(evo_candidate("c1", "t", "pending"))
        .await
        .unwrap();
    store
        .upsert_evolution_candidate(evo_candidate("c2", "t", "executed"))
        .await
        .unwrap();
    // Different tenant — must not leak into t's listing.
    store
        .upsert_evolution_candidate(evo_candidate("c3", "other", "pending"))
        .await
        .unwrap();

    // List all for t (status=None) → c1 + c2, not c3.
    let all = store.list_evolution_candidates("t", None).await.unwrap();
    let mut ids: Vec<&str> = all.iter().map(|c| c.candidate_id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["c1", "c2"]);

    // Status filter.
    let pending = store
        .list_evolution_candidates("t", Some("pending"))
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    let got = &pending[0];
    assert_eq!(got.candidate_id, "c1");
    // JSON id-list + params round-trip verbatim.
    assert_eq!(got.member_ids, vec!["m1".to_string(), "m2".to_string()]);
    assert_eq!(got.params, r#"{"threshold":0.9}"#);
    assert_eq!(got.op_kind, "merge");
    assert!((got.evidence - 1.5).abs() < 1e-6);
    assert_eq!(got.consecutive_cycles, 2);
    assert!(got.executed_at.is_none());
    assert!(got.result_capsule_ids.is_empty());
});

emb_test!(evolution_candidate_upsert_replaces_in_place, store, {
    store
        .upsert_evolution_candidate(evo_candidate("c1", "t", "pending"))
        .await
        .unwrap();
    // Re-upsert same id with mutated fields (executed).
    let mut updated = evo_candidate("c1", "t", "executed");
    updated.consecutive_cycles = 9;
    updated.executed_at = Some("00000000000000000099".into());
    updated.result_capsule_ids = vec!["r1".into()];
    store.upsert_evolution_candidate(updated).await.unwrap();

    let all = store.list_evolution_candidates("t", None).await.unwrap();
    assert_eq!(all.len(), 1, "ON CONFLICT replaced, did not duplicate");
    let c = &all[0];
    assert_eq!(c.status, "executed");
    assert_eq!(c.consecutive_cycles, 9);
    assert_eq!(c.executed_at.as_deref(), Some("00000000000000000099"));
    assert_eq!(c.result_capsule_ids, vec!["r1".to_string()]);
});

// ──────────────────────────────── SessionStore ──────────────────────────

emb_test!(session_open_touch_close_latest, store, {
    // No active session yet.
    assert!(store
        .latest_active_session("t", "claude")
        .await
        .unwrap()
        .is_none());

    let s = store
        .open_session("s1", "t", "claude", "00000000000000000010")
        .await
        .unwrap();
    assert_eq!(s.session_id, "s1");
    assert_eq!(s.memory_count, 0);
    assert!(s.ended_at.is_none());

    // latest_active_session finds it.
    let latest = store
        .latest_active_session("t", "claude")
        .await
        .unwrap()
        .expect("active session");
    assert_eq!(latest.session_id, "s1");

    // touch bumps memory_count + last_seen_at.
    store
        .touch_session("s1", "00000000000000000020")
        .await
        .unwrap();
    let touched = store
        .latest_active_session("t", "claude")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(touched.memory_count, 1);
    assert_eq!(touched.last_seen_at, "00000000000000000020");

    // A second, newer active session wins latest (ORDER BY last_seen_at DESC).
    store
        .open_session("s2", "t", "claude", "00000000000000000030")
        .await
        .unwrap();
    assert_eq!(
        store
            .latest_active_session("t", "claude")
            .await
            .unwrap()
            .unwrap()
            .session_id,
        "s2"
    );

    // Close s2 → latest falls back to s1.
    store
        .close_session("s2", "00000000000000000040")
        .await
        .unwrap();
    assert_eq!(
        store
            .latest_active_session("t", "claude")
            .await
            .unwrap()
            .unwrap()
            .session_id,
        "s1"
    );

    // Different caller_agent is isolated.
    assert!(store
        .latest_active_session("t", "other-agent")
        .await
        .unwrap()
        .is_none());
});

fn episode(id: &str, tenant: &str, outcome: &str) -> EpisodeRecord {
    EpisodeRecord {
        episode_id: id.into(),
        tenant: tenant.into(),
        goal: format!("goal-{id}"),
        steps: vec!["step1".into(), "step2".into()],
        outcome: outcome.into(),
        evidence: vec!["ev1".into()],
        scope: Scope::Repo,
        visibility: Visibility::Private,
        project: Some("proj".into()),
        repo: None,
        module: None,
        tags: vec!["t1".into()],
        source_agent: "test".into(),
        idempotency_key: None,
        created_at: format!("0000000000000000{id:0>4}"),
        updated_at: format!("0000000000000000{id:0>4}"),
        workflow_candidate: None,
    }
}

emb_test!(insert_and_list_successful_episodes, store, {
    store
        .insert_episode(episode("1", "t", "success"))
        .await
        .unwrap();
    store
        .insert_episode(episode("2", "t", "failure"))
        .await
        .unwrap();
    store
        .insert_episode(episode("3", "t", "success"))
        .await
        .unwrap();
    // Other tenant — excluded.
    store
        .insert_episode(episode("4", "other", "success"))
        .await
        .unwrap();

    let ok = store
        .list_successful_episodes_for_tenant("t")
        .await
        .unwrap();
    let ids: Vec<&str> = ok.iter().map(|e| e.episode_id.as_str()).collect();
    // Only outcome='success' rows for t; ordered created_at DESC (id 3, 1).
    assert_eq!(ids, vec!["3", "1"]);
    // Round-trip a list-column + scope/visibility field.
    let e3 = &ok[0];
    assert_eq!(e3.steps, vec!["step1".to_string(), "step2".to_string()]);
    assert_eq!(e3.scope, Scope::Repo);
    assert_eq!(e3.visibility, Visibility::Private);
    assert_eq!(e3.project.as_deref(), Some("proj"));
    assert_eq!(e3.tags, vec!["t1".to_string()]);
});

// ─────────────────────────────── EntityRegistry ─────────────────────────

emb_test!(entity_resolve_or_create_and_alias, store, {
    // Same normalized alias → same id (casing/whitespace collapse).
    let id1 = store
        .resolve_or_create(
            "ta",
            "Rust Async",
            EntityKind::Topic,
            "00000000000000000001",
        )
        .await
        .unwrap();
    let id1b = store
        .resolve_or_create(
            "ta",
            "  rust   ASYNC  ",
            EntityKind::Topic,
            "00000000000000000002",
        )
        .await
        .unwrap();
    assert_eq!(id1, id1b, "normalized alias round-trips to same entity");

    // Distinct alias → distinct entity.
    let id2 = store
        .resolve_or_create("ta", "DuckDB", EntityKind::Project, "00000000000000000003")
        .await
        .unwrap();
    assert_ne!(id1, id2);

    // Cross-tenant same alias → distinct entity (composite PK).
    let id3 = store
        .resolve_or_create(
            "tb",
            "Rust Async",
            EntityKind::Topic,
            "00000000000000000004",
        )
        .await
        .unwrap();
    assert_ne!(id1, id3);

    // add_alias: new alias on same entity → Inserted.
    assert_eq!(
        store
            .add_alias("ta", &id1, "Tokio", "00000000000000000010")
            .await
            .unwrap(),
        AddAliasOutcome::Inserted
    );
    // Re-add (idempotent) → AlreadyOnSameEntity.
    assert_eq!(
        store
            .add_alias("ta", &id1, "tokio", "00000000000000000011")
            .await
            .unwrap(),
        AddAliasOutcome::AlreadyOnSameEntity
    );
    // Different entity claiming an owned alias → Conflict(owner).
    assert_eq!(
        store
            .add_alias("ta", &id2, "Tokio", "00000000000000000012")
            .await
            .unwrap(),
        AddAliasOutcome::ConflictWithDifferentEntity(id1.clone())
    );

    // lookup_alias normalizes the probe.
    assert_eq!(
        store
            .lookup_alias("ta", "  RUST  async ")
            .await
            .unwrap()
            .as_deref(),
        Some(id1.as_str())
    );
    assert!(store.lookup_alias("ta", "nope").await.unwrap().is_none());
});

emb_test!(entity_get_and_list, store, {
    let id_rust = store
        .resolve_or_create(
            "ta",
            "Rust Async",
            EntityKind::Topic,
            "00000000000000000001",
        )
        .await
        .unwrap();
    let id_duck = store
        .resolve_or_create("ta", "DuckDB", EntityKind::Project, "00000000000000000010")
        .await
        .unwrap();
    store
        .add_alias("ta", &id_rust, "Tokio", "00000000000000000020")
        .await
        .unwrap();

    // get_entity → canonical_name verbatim, kind, aliases ordered
    // created_at ASC ('rust async' then 'tokio').
    let with = store
        .get_entity("ta", &id_rust)
        .await
        .unwrap()
        .expect("entity exists");
    assert_eq!(with.entity.canonical_name, "Rust Async");
    assert_eq!(with.entity.kind, EntityKind::Topic);
    assert_eq!(
        with.aliases,
        vec!["rust async".to_string(), "tokio".to_string()]
    );
    assert!(store.get_entity("ta", "missing").await.unwrap().is_none());

    // list_entities ordered created_at DESC: duck (later) then rust.
    let all = store.list_entities("ta", None, None, 10).await.unwrap();
    let ids: Vec<&str> = all.iter().map(|e| e.entity_id.as_str()).collect();
    assert_eq!(ids, vec![id_duck.as_str(), id_rust.as_str()]);

    // kind filter.
    let topics = store
        .list_entities("ta", Some(EntityKind::Topic), None, 10)
        .await
        .unwrap();
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].entity_id, id_rust);

    // LIKE substring on canonical_name (case-sensitive).
    let like = store
        .list_entities("ta", None, Some("Rust"), 10)
        .await
        .unwrap();
    assert_eq!(like.len(), 1);
    assert_eq!(like[0].canonical_name, "Rust Async");
});

// ────────────────────────────── MaintenanceStore ────────────────────────

emb_test!(apply_time_decay_bumps_active_only, store, {
    // Active row with last_used_at one day before now → decays.
    let mut active = fixture("dk_active", CapabilityCapsuleStatus::Active);
    active.decay_score = 0.0;
    active.updated_at = "00000000000086400000".into(); // 1 day in ms
    active.last_used_at = Some("00000000000086400000".into());
    store.insert_capability_capsule(active).await.unwrap();

    // Archived row → must NOT be touched by the sweep.
    let mut archived = fixture("dk_archived", CapabilityCapsuleStatus::Archived);
    archived.decay_score = 0.0;
    store.insert_capability_capsule(archived).await.unwrap();

    // now = 2 days in ms; rate 0.1/day; ms_per_day = 86_400_000.
    let now_ms = 172_800_000.0_f64;
    store
        .apply_time_decay(0.1, now_ms, 86_400_000.0, "00000000000172800000")
        .await
        .unwrap();

    let a = store
        .get_capability_capsule_for_tenant("t", "dk_active")
        .await
        .unwrap()
        .unwrap();
    // 1 day elapsed * 0.1/day = +0.1.
    assert!(
        (a.decay_score - 0.1).abs() < 1e-5,
        "decay bumped: {}",
        a.decay_score
    );
    // last_used_at advanced to now (decay clock reset).
    assert_eq!(a.last_used_at.as_deref(), Some("00000000000172800000"));

    let arch = store
        .get_capability_capsule_for_tenant("t", "dk_archived")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(arch.decay_score, 0.0, "archived row untouched");
});

emb_test!(apply_time_decay_archives_expired, store, {
    let mut exp = fixture("dk_exp", CapabilityCapsuleStatus::Active);
    exp.expires_at = Some("00000000000000000100".into());
    store.insert_capability_capsule(exp).await.unwrap();

    // now past the deadline → archived.
    store
        .apply_time_decay(0.1, 200.0, 86_400_000.0, "00000000000000000200")
        .await
        .unwrap();
    let got = store
        .get_capability_capsule_for_tenant("t", "dk_exp")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.status, CapabilityCapsuleStatus::Archived);
});

emb_test!(auto_promote_candidates_filters, store, {
    // Eligible: pending, updated_at < cutoff, low decay, type in list.
    let mut ok = fixture("ap_ok", CapabilityCapsuleStatus::PendingConfirmation);
    ok.capability_capsule_type = CapabilityCapsuleType::Experience;
    ok.updated_at = "00000000000000000100".into();
    ok.created_at = "00000000000000000100".into();
    ok.decay_score = 0.1;
    store.insert_capability_capsule(ok).await.unwrap();

    // Too fresh (updated_at >= cutoff) → excluded.
    let mut fresh = fixture("ap_fresh", CapabilityCapsuleStatus::PendingConfirmation);
    fresh.capability_capsule_type = CapabilityCapsuleType::Experience;
    fresh.updated_at = "00000000000000000900".into();
    store.insert_capability_capsule(fresh).await.unwrap();

    // High decay → excluded.
    let mut decayed = fixture("ap_decayed", CapabilityCapsuleStatus::PendingConfirmation);
    decayed.capability_capsule_type = CapabilityCapsuleType::Experience;
    decayed.updated_at = "00000000000000000100".into();
    decayed.decay_score = 0.9;
    store.insert_capability_capsule(decayed).await.unwrap();

    // Wrong type → excluded.
    let mut wrong = fixture("ap_wrong", CapabilityCapsuleStatus::PendingConfirmation);
    wrong.capability_capsule_type = CapabilityCapsuleType::Preference;
    wrong.updated_at = "00000000000000000100".into();
    store.insert_capability_capsule(wrong).await.unwrap();

    // Active (not pending) → excluded.
    let mut act = fixture("ap_active", CapabilityCapsuleStatus::Active);
    act.capability_capsule_type = CapabilityCapsuleType::Experience;
    act.updated_at = "00000000000000000100".into();
    store.insert_capability_capsule(act).await.unwrap();

    let got = store
        .auto_promote_candidates(
            "t",
            "00000000000000000500",
            &[CapabilityCapsuleType::Experience],
            0.5,
        )
        .await
        .unwrap();
    let ids: Vec<&str> = got
        .iter()
        .map(|c| c.capability_capsule_id.as_str())
        .collect();
    assert_eq!(ids, vec!["ap_ok"], "only the fully-eligible row: {ids:?}");

    // Empty types short-circuits.
    let none = store
        .auto_promote_candidates("t", "00000000000000000500", &[], 0.5)
        .await
        .unwrap();
    assert!(none.is_empty());
});

emb_test!(vacuum_old_versions_is_noop, store, {
    // Postgres has no Lance manifests — zero-stats no-op, both flags.
    let s = store.vacuum_old_versions_with(7, false).await.unwrap();
    assert_eq!(s.old_versions_removed, 0);
    assert_eq!(s.tables_pruned, 0);
    let s2 = store.vacuum_old_versions_with(0, true).await.unwrap();
    assert_eq!(s2.bytes_removed, 0);
});
