//! ClickHouse backend parity smoke — **UNVALIDATED scaffold**
//! (clickhouse-backend P1).
//!
//! The backend is a default dependency (always compiled), but the suite
//! requires `MEM_TEST_CLICKHOUSE_URL` (e.g. `http://localhost:8123`): when
//! unset every case prints a skip line and returns `Ok` — there is no local
//! ClickHouse in CI/dev, so a plain `cargo test` must never fail merely for
//! being un-runnable.
//!
//! When a real ClickHouse is reachable it applies
//! `migrations/clickhouse/0001_capsule_store.sql` and runs a representative
//! subset of the `tests/capsule_store_parity.rs` scenarios against
//! `ClickHouseBackend` (full reuse of that crate's scenarios needs a shared
//! test-helper module — a P-later cleanup).

mod ch {
    use std::sync::Arc;

    use mem::domain::capability_capsule::GraphEdge;
    use mem::domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    };
    use mem::domain::EntityKind;
    use mem::domain::{BlockType, ConversationMessage, MessageRole};
    use mem::storage::types::EmbeddingJobInsert;
    use mem::storage::{
        current_timestamp, CapsuleStore, ClickHouseBackend, EmbeddingJobStore,
        EmbeddingVectorStore, EntityRegistry, FeedbackEvent, GraphStore, TranscriptStore,
    };

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
            content_hash: format!("{:0>64}", id),
            confidence: 0.5,
            decay_score: 0.0,
            source_agent: "test".into(),
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
            ..Default::default()
        }
    }

    /// Connect + migrate, or `None` to skip (URL unset). A fresh
    /// per-run database name keeps parallel test runs isolated would be
    /// ideal; P1 just targets whatever DB the URL names.
    async fn store() -> Option<Arc<dyn CapsuleStore>> {
        let url = std::env::var("MEM_TEST_CLICKHOUSE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let Some(url) = url else {
            eprintln!("MEM_TEST_CLICKHOUSE_URL unset — skipping ClickHouse parity");
            return None;
        };
        let backend = ClickHouseBackend::connect(&url)
            .await
            .expect("clickhouse connect");
        backend
            .apply_migrations()
            .await
            .expect("clickhouse migrate");
        Some(Arc::new(backend))
    }

    #[tokio::test]
    async fn insert_and_get_round_trip() {
        let Some(store) = store().await else { return };
        store
            .insert_capability_capsule(fixture("ch_a", CapabilityCapsuleStatus::Active))
            .await
            .unwrap();
        let got = store
            .get_capability_capsule_for_tenant("t", "ch_a")
            .await
            .unwrap()
            .expect("round-trip row");
        assert_eq!(got.capability_capsule_id, "ch_a");
        assert_eq!(got.status, CapabilityCapsuleStatus::Active);
    }

    #[tokio::test]
    async fn accept_pending_transitions_status() {
        let Some(store) = store().await else { return };
        store
            .insert_capability_capsule(fixture(
                "ch_pending",
                CapabilityCapsuleStatus::PendingConfirmation,
            ))
            .await
            .unwrap();
        let updated = store.accept_pending("t", "ch_pending").await.unwrap();
        assert_eq!(updated.status, CapabilityCapsuleStatus::Active);
        // The latest version read should also reflect Active (ReplacingMergeTree).
        let got = store
            .get_capability_capsule_for_tenant("t", "ch_pending")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.status, CapabilityCapsuleStatus::Active);
    }

    #[tokio::test]
    async fn find_by_idempotency_dedups_on_hash() {
        let Some(store) = store().await else { return };
        let row = fixture("ch_hash", CapabilityCapsuleStatus::Active);
        let hash = row.content_hash.clone();
        store.insert_capability_capsule(row).await.unwrap();
        let found = store
            .find_by_idempotency_or_hash("t", &None, &hash)
            .await
            .unwrap();
        assert_eq!(
            found.map(|r| r.capability_capsule_id),
            Some("ch_hash".to_string())
        );
    }

    /// Concrete backend (impls every sub-trait) so a test can reach methods
    /// off `CapsuleStore` — e.g. `EmbeddingVectorStore` (P3). `None` to skip.
    async fn ch_backend() -> Option<ClickHouseBackend> {
        let url = std::env::var("MEM_TEST_CLICKHOUSE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let Some(url) = url else {
            eprintln!("MEM_TEST_CLICKHOUSE_URL unset — skipping ClickHouse embedding parity");
            return None;
        };
        let backend = ClickHouseBackend::connect(&url)
            .await
            .expect("clickhouse connect");
        backend
            .apply_migrations()
            .await
            .expect("clickhouse migrate");
        Some(backend)
    }

    /// P3: a capsule embedding upserts and round-trips its vector + metadata,
    /// and `delete` clears it.
    #[tokio::test]
    async fn embedding_vector_round_trip_and_delete() {
        let Some(be) = ch_backend().await else { return };
        let v = vec![0.1_f32, 0.2, 0.3, 0.4];
        be.upsert_capability_capsule_embedding_chunks(
            "ch_emb",
            "t",
            "test-model",
            4,
            std::slice::from_ref(&v),
            "hash_emb",
            &current_timestamp(),
            &current_timestamp(),
        )
        .await
        .unwrap();
        assert_eq!(
            be.get_capability_capsule_embedding_vector("ch_emb")
                .await
                .unwrap(),
            Some(v)
        );
        assert_eq!(
            be.get_capability_capsule_embedding_row("ch_emb")
                .await
                .unwrap()
                .map(|(m, h, _)| (m, h)),
            Some(("test-model".to_string(), "hash_emb".to_string()))
        );
        be.delete_capability_capsule_embedding("ch_emb")
            .await
            .unwrap();
        // Mutations are async in ClickHouse; the delete is best-effort here —
        // we only assert it doesn't error. (A post-delete absence check would
        // need to wait out the mutation; deferred to the validation pass.)
    }

    /// P3: a 2-chunk conversation embedding upserts without error (the
    /// chunk_index discriminator lets both rows coexist under
    /// ReplacingMergeTree; the full chunk-set read is P5's search).
    #[tokio::test]
    async fn conversation_chunks_upsert_ok() {
        let Some(be) = ch_backend().await else { return };
        let vs = vec![vec![1.0_f32, 0.0], vec![0.0_f32, 1.0]];
        be.upsert_conversation_message_embedding_chunks(
            "msg_x",
            "t",
            "test-model",
            2,
            &vs,
            "hash_msg",
            &current_timestamp(),
            &current_timestamp(),
        )
        .await
        .unwrap();
        be.delete_conversation_message_embedding("msg_x")
            .await
            .unwrap();
    }

    /// P4: seed 2 capsules + embeddings → `ann_candidate_ids` ranks by the
    /// query vector (chunk-collapse + tenant postfilter), `bm25_candidate_ids`
    /// substring-matches the coarse lexical channel, and
    /// `hybrid_candidates_compose` fuses both via Rust RRF.
    #[tokio::test]
    async fn hybrid_search_ann_bm25_compose() {
        use mem::storage::CapsuleSearchStore;
        let Some(be) = ch_backend().await else { return };

        let mut a = fixture("ch_s_a", CapabilityCapsuleStatus::Active);
        a.content = "alpha vector database".into();
        let mut b = fixture("ch_s_b", CapabilityCapsuleStatus::Active);
        b.content = "beta graph store".into();
        be.insert_capability_capsule(a).await.unwrap();
        be.insert_capability_capsule(b).await.unwrap();

        let va = vec![1.0_f32, 0.0, 0.0];
        let vb = vec![0.0_f32, 1.0, 0.0];
        be.upsert_capability_capsule_embedding_chunks(
            "ch_s_a",
            "t",
            "m",
            3,
            std::slice::from_ref(&va),
            "ha",
            &current_timestamp(),
            &current_timestamp(),
        )
        .await
        .unwrap();
        be.upsert_capability_capsule_embedding_chunks(
            "ch_s_b",
            "t",
            "m",
            3,
            std::slice::from_ref(&vb),
            "hb",
            &current_timestamp(),
            &current_timestamp(),
        )
        .await
        .unwrap();

        // ANN: query near `va` → ch_s_a ranks first.
        let ann = be.ann_candidate_ids("t", &va, 5).await.unwrap();
        assert_eq!(ann.first().map(|(id, _)| id.as_str()), Some("ch_s_a"));

        // BM25 (coarse substring): "graph" → ch_s_b is a candidate.
        let bm25 = be.bm25_candidate_ids("t", "graph", 5).await.unwrap();
        assert!(bm25.iter().any(|(id, _)| id == "ch_s_b"));

        // Hybrid compose: text + vec → non-empty fused result, ch_s_a on top.
        let hybrid = be
            .hybrid_candidates_compose("t", "vector", &va, 5)
            .await
            .unwrap();
        assert_eq!(
            hybrid
                .first()
                .map(|(r, _)| r.capability_capsule_id.as_str()),
            Some("ch_s_a")
        );
    }

    #[tokio::test]
    async fn feedback_summary_counts_kinds() {
        let Some(store) = store().await else { return };
        let row = fixture("ch_fb", CapabilityCapsuleStatus::Active);
        store.insert_capability_capsule(row.clone()).await.unwrap();
        store
            .apply_feedback(
                &row,
                FeedbackEvent {
                    feedback_id: "fb1".into(),
                    capability_capsule_id: "ch_fb".into(),
                    feedback_kind: "useful".into(),
                    created_at: current_timestamp(),
                    note: None,
                },
            )
            .await
            .unwrap();
        let summary = store.feedback_summary("ch_fb").await.unwrap();
        assert_eq!(summary.useful, 1);
        assert_eq!(summary.total, 1);
    }

    fn edge(from: &str, to: &str, rel: &str) -> GraphEdge {
        GraphEdge {
            from_node_id: from.into(),
            to_node_id: to.into(),
            relation: rel.into(),
            valid_from: current_timestamp(),
            valid_to: None,
            confidence: Some(1.0),
            extractor: Some("test".into()),
            strength: None,
            stability: None,
            last_activated: None,
            access_count: None,
        }
    }

    /// P5 GraphStore: sync edges, then `neighbors` finds the incident active edge.
    #[tokio::test]
    async fn graph_sync_and_neighbors() {
        let Some(be) = ch_backend().await else { return };
        let now = current_timestamp();
        be.sync_memory_edges(&[edge("a", "b", "rel:x")], &now)
            .await
            .unwrap();
        let n = be.neighbors("a").await.unwrap();
        assert!(n
            .iter()
            .any(|e| e.to_node_id == "b" && e.relation == "rel:x"));
    }

    fn dated_edge(from: &str, to: &str, rel: &str, vfrom: &str, vto: Option<&str>) -> GraphEdge {
        let mut e = edge(from, to, rel);
        e.valid_from = vfrom.into();
        e.valid_to = vto.map(str::to_string);
        e
    }

    /// (e) neighbors_within must honor the point-in-time `as_of` window, not
    /// BFS-walk only the currently-active edge set. `add_edge_direct` INSERTs
    /// (synchronous, unlike the async ALTER mutations), and the fixed
    /// `(from, relation, to, valid_from)` keys dedup under FINAL so re-runs are
    /// idempotent against the persistent container.
    #[tokio::test]
    async fn neighbors_within_respects_as_of() {
        let Some(be) = ch_backend().await else { return };

        // Closed edge active over [100, 200); new edge active from 300.
        be.add_edge_direct(&dated_edge(
            "na",
            "nb",
            "rel:old",
            "00000000000000000100",
            Some("00000000000000000200"),
        ))
        .await
        .unwrap();
        be.add_edge_direct(&dated_edge(
            "na",
            "nc",
            "rel:new",
            "00000000000000000300",
            None,
        ))
        .await
        .unwrap();

        let at_150 = be
            .neighbors_within("na", 2, Some("00000000000000000150"))
            .await
            .unwrap();
        assert!(
            at_150.iter().any(|e| e.to_node_id == "nb"),
            "closed edge active at 150 must be included"
        );
        assert!(
            !at_150.iter().any(|e| e.to_node_id == "nc"),
            "edge not yet valid at 150 must be excluded"
        );

        let at_350 = be
            .neighbors_within("na", 2, Some("00000000000000000350"))
            .await
            .unwrap();
        assert!(
            !at_350.iter().any(|e| e.to_node_id == "nb"),
            "expired edge must be excluded at 350"
        );
        assert!(
            at_350.iter().any(|e| e.to_node_id == "nc"),
            "active edge must be included at 350"
        );
    }

    /// (e) query_predicate: `as_of=None` is the FULL history (active + closed),
    /// and `as_of=Some(ts)` restricts to edges active at ts — the CH scaffold
    /// ignored both and returned only the currently-active set.
    #[tokio::test]
    async fn query_predicate_as_of_and_full_history() {
        let Some(be) = ch_backend().await else { return };

        be.add_edge_direct(&dated_edge(
            "qa",
            "qb",
            "pred:qp",
            "00000000000000000100",
            Some("00000000000000000200"),
        ))
        .await
        .unwrap();
        be.add_edge_direct(&dated_edge(
            "qc",
            "qd",
            "pred:qp",
            "00000000000000000300",
            None,
        ))
        .await
        .unwrap();
        // Tie on valid_from with qa→qb (both "...100"), but a from_node_id that
        // sorts first — exercises the (valid_from, from_node_id, to_node_id)
        // tie-break so the order is deterministic (matches lance/postgres).
        // Closed at "...120" (before the as_of=150 probe) so it only affects the
        // full-history ordering, not the point-in-time count below.
        be.add_edge_direct(&dated_edge(
            "q0",
            "qz",
            "pred:qp",
            "00000000000000000100",
            Some("00000000000000000120"),
        ))
        .await
        .unwrap();

        let all = be.query_predicate("pred:qp", None).await.unwrap();
        let from_order: Vec<&str> = all.iter().map(|e| e.from_node_id.as_str()).collect();
        assert_eq!(
            from_order,
            vec!["q0", "qa", "qc"],
            "full history must be ordered by (valid_from, from_node_id, to_node_id); got {from_order:?}"
        );

        let at_150 = be
            .query_predicate("pred:qp", Some("00000000000000000150"))
            .await
            .unwrap();
        assert_eq!(
            at_150.len(),
            1,
            "as_of=150 must return only the edge active then"
        );
        assert_eq!(at_150[0].to_node_id, "qb");
    }

    #[allow(clippy::too_many_arguments)]
    fn cmsg(
        block_id: &str,
        line: u64,
        created_at: &str,
        role: MessageRole,
        bt: BlockType,
    ) -> ConversationMessage {
        ConversationMessage {
            message_block_id: block_id.into(),
            session_id: Some("pg_s".into()),
            tenant: "t".into(),
            caller_agent: "test".into(),
            transcript_path: "/tmp/pg.jsonl".into(),
            line_number: line,
            block_index: 0,
            message_uuid: None,
            role,
            block_type: bt,
            content: format!("block {line}"),
            tool_name: None,
            tool_use_id: None,
            embed_eligible: matches!(bt, BlockType::Text),
            created_at: created_at.into(),
            meta_json: None,
        }
    }

    /// (e) paged transcript read must apply the composite cursor AND the
    /// role/block_type/time filters — the CH scaffold ignored them all and
    /// always returned the first page from the start (broken/stuck pagination).
    #[tokio::test]
    async fn paged_transcript_read_applies_cursor_and_filters() {
        let Some(be) = ch_backend().await else { return };

        be.create_conversation_messages(&[
            cmsg(
                "pg1",
                1,
                "00000000000000000001",
                MessageRole::Assistant,
                BlockType::Text,
            ),
            cmsg(
                "pg2",
                2,
                "00000000000000000002",
                MessageRole::User,
                BlockType::Text,
            ),
            cmsg(
                "pg3",
                3,
                "00000000000000000003",
                MessageRole::Assistant,
                BlockType::ToolUse,
            ),
            cmsg(
                "pg4",
                4,
                "00000000000000000004",
                MessageRole::Assistant,
                BlockType::Text,
            ),
        ])
        .await
        .unwrap();

        // Cursor resume after (created_at="...002", line 2, block 0) → pg3, pg4.
        let (after, _) = be
            .get_conversation_messages_by_session_paged(
                "t",
                "pg_s",
                None,
                None,
                None,
                None,
                Some(("00000000000000000002", 2, 0)),
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            2,
            "cursor must resume after the given position"
        );
        assert_eq!(after[0].line_number, 3);
        assert_eq!(after[1].line_number, 4);

        // role filter → only the single user block.
        let (users, _) = be
            .get_conversation_messages_by_session_paged(
                "t",
                "pg_s",
                None,
                None,
                Some("user"),
                None,
                None,
                10,
            )
            .await
            .unwrap();
        assert_eq!(users.len(), 1, "role filter must narrow to user blocks");
        assert_eq!(users[0].message_block_id, "pg2");
    }

    /// (e) cross-session range read: the LIMIT was a bound string (ClickHouse
    /// rejects that outright) and role/block_type/cursor were ignored. Two
    /// sessions under one tenant, half-open time window, then a role narrow.
    #[tokio::test]
    async fn range_transcript_read_applies_filters() {
        let Some(be) = ch_backend().await else { return };

        fn rmsg(id: &str, sess: &str, at: &str, role: MessageRole) -> ConversationMessage {
            ConversationMessage {
                message_block_id: id.into(),
                session_id: Some(sess.into()),
                tenant: "tr".into(),
                caller_agent: "test".into(),
                transcript_path: "/tmp/r.jsonl".into(),
                line_number: 1,
                block_index: 0,
                message_uuid: None,
                role,
                block_type: BlockType::Text,
                content: id.into(),
                tool_name: None,
                tool_use_id: None,
                embed_eligible: true,
                created_at: at.into(),
                meta_json: None,
            }
        }
        be.create_conversation_messages(&[
            rmsg("r1", "rs_a", "00000000000000000010", MessageRole::Assistant),
            rmsg("r2", "rs_b", "00000000000000000020", MessageRole::User),
            rmsg("r3", "rs_a", "00000000000000000030", MessageRole::Assistant),
        ])
        .await
        .unwrap();

        // Half-open window [10, 30): r1 + r2 (r3 at 30 is excluded).
        let (win, _) = be
            .list_conversation_messages_in_range(
                "tr",
                Some("00000000000000000010"),
                Some("00000000000000000030"),
                None,
                None,
                None,
                10,
            )
            .await
            .unwrap();
        let ids: Vec<&str> = win.iter().map(|m| m.message_block_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["r1", "r2"],
            "half-open window must exclude the upper bound"
        );

        // role filter across sessions.
        let (users, _) = be
            .list_conversation_messages_in_range("tr", None, None, Some("user"), None, None, 10)
            .await
            .unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].message_block_id, "r2");
    }

    /// P5 EntityRegistry: resolve creates an entity; a second resolve of the
    /// same alias returns the same id; get_entity surfaces it.
    #[tokio::test]
    async fn entity_resolve_is_idempotent() {
        let Some(be) = ch_backend().await else { return };
        let now = current_timestamp();
        let id1 = be
            .resolve_or_create("t", "Rust", EntityKind::Topic, &now)
            .await
            .unwrap();
        let id2 = be
            .resolve_or_create("t", "rust", EntityKind::Topic, &now)
            .await
            .unwrap();
        assert_eq!(id1, id2);
        let got = be.get_entity("t", &id1).await.unwrap().unwrap();
        assert_eq!(got.entity.canonical_name, "Rust");
    }

    /// P5 EmbeddingJobStore: enqueue → claim → complete moves the status.
    #[tokio::test]
    async fn embedding_job_enqueue_claim_complete() {
        let Some(be) = ch_backend().await else { return };
        let now = current_timestamp();
        let ok = be
            .try_enqueue_embedding_job(EmbeddingJobInsert {
                job_id: "j1".into(),
                tenant: "t".into(),
                capability_capsule_id: "cap1".into(),
                target_content_hash: "h1".into(),
                provider: "fake".into(),
                available_at: now.clone(),
                created_at: now.clone(),
                updated_at: now.clone(),
            })
            .await
            .unwrap();
        assert!(ok);
        let claimed = be.claim_next_n_embedding_jobs(&now, 3, 10).await.unwrap();
        assert!(claimed.iter().any(|c| c.job_id == "j1"));
        be.complete_embedding_job("j1", &now).await.unwrap();
        assert_eq!(
            be.get_embedding_job_status("j1").await.unwrap(),
            Some("completed".to_string())
        );
    }
}
