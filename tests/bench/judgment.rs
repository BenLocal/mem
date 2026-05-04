//! Judgment derivation. Synthetic path uses pre-computed
//! `synthetic_judgments`; real path resolves anchor_entities via
//! EntityRegistry and scans session content.

use super::fixture::*;
use mem::domain::EntityKind;
use mem::pipeline::entity_normalize::normalize_alias;
use mem::storage::EntityRegistry;
use std::collections::HashSet;

pub async fn derive_judgments(
    fixture: &Fixture,
    registry: &dyn EntityRegistry,
    now: &str,
) -> JudgmentMap {
    let mut judgments: JudgmentMap = std::collections::HashMap::new();

    for query in &fixture.queries {
        // Synthetic: use pre-computed judgments verbatim.
        if let Some(synth) = &query.synthetic_judgments {
            judgments.insert(query.query_id.clone(), synth.clone());
            continue;
        }

        // Real: resolve each anchor alias, then scan for any matching alias in content.
        let mut entity_ids: HashSet<String> = HashSet::new();
        for alias in &query.anchor_entities {
            let id = registry
                .resolve_or_create(&fixture.tenant, alias, EntityKind::Topic, now)
                .await
                .expect("registry resolve_or_create");
            entity_ids.insert(id);
        }

        let mut relevant = HashSet::new();
        for session in &fixture.sessions {
            if session_mentions_any_alias_of(session, &entity_ids, registry, &fixture.tenant).await
            {
                relevant.insert(session.session_id.clone());
            }
        }
        judgments.insert(query.query_id.clone(), relevant);
    }
    judgments
}

async fn session_mentions_any_alias_of(
    session: &SessionFixture,
    target_entity_ids: &HashSet<String>,
    registry: &dyn EntityRegistry,
    tenant: &str,
) -> bool {
    // Tokenize whole session content (text + thinking only — bench skips tool blocks).
    let mut tokens: Vec<String> = Vec::new();
    for block in &session.blocks {
        if matches!(block.block_type.as_str(), "text" | "thinking") {
            for tok in block.content.split_whitespace() {
                tokens.push(tok.to_string());
            }
        }
    }
    // Also try multi-word phrases (up to 3 grams) since aliases like "Rust async" are 2 words.
    for window_size in 1..=3 {
        for window in tokens.windows(window_size) {
            let phrase = window.join(" ");
            let normalized = normalize_alias(&phrase);
            if normalized.is_empty() {
                continue;
            }
            if let Ok(Some(entity_id)) = registry.lookup_alias(tenant, &normalized).await {
                if target_entity_ids.contains(&entity_id) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem::storage::DuckDbRepository;
    use std::collections::HashSet;

    fn synthetic_fixture() -> Fixture {
        // Tiny one-session fixture with a pre-baked judgment.
        let mut precomp = HashSet::new();
        precomp.insert("s1".to_string());
        Fixture {
            kind: FixtureKind::Synthetic { seed: 0 },
            tenant: "t".to_string(),
            sessions: vec![SessionFixture {
                session_id: "s1".to_string(),
                started_at: "00000000020260503000".to_string(),
                blocks: vec![BlockFixture {
                    block_id: "b1".to_string(),
                    role: "user".to_string(),
                    block_type: "text".to_string(),
                    content: "Tokio runtime async Rust".to_string(),
                    created_at: "00000000020260503000".to_string(),
                }],
            }],
            queries: vec![QueryFixture {
                query_id: "q1".to_string(),
                text: "Rust async".to_string(),
                anchor_session_id: None,
                anchor_entities: vec!["Rust async".to_string()],
                synthetic_judgments: Some(precomp),
            }],
        }
    }

    #[tokio::test]
    async fn synthetic_path_uses_precomputed_judgments() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(tmp.path().join("j.duckdb"))
            .await
            .unwrap();
        let f = synthetic_fixture();
        let j = derive_judgments(&f, &repo, "00000000020260503000").await;
        assert_eq!(j.get("q1").unwrap().len(), 1);
        assert!(j.get("q1").unwrap().contains("s1"));
    }

    #[tokio::test]
    async fn real_path_derives_via_registry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(tmp.path().join("j.duckdb"))
            .await
            .unwrap();

        // Pre-populate the entity registry with a "Rust async" entity, then
        // explicitly link "tokio" as an alias of that same entity.
        let rust_async_id = repo
            .resolve_or_create("t", "Rust async", EntityKind::Topic, "00000000020260503000")
            .await
            .unwrap();
        repo.add_alias("t", &rust_async_id, "tokio", "00000000020260503000")
            .await
            .unwrap();

        let mut fixture = synthetic_fixture();
        fixture.kind = FixtureKind::Real;
        fixture.queries[0].synthetic_judgments = None; // force real path
        fixture.queries[0].anchor_entities = vec!["tokio".to_string()];
        // Block content already contains "Tokio" — normalize_alias lowercases it.

        let j = derive_judgments(&fixture, &repo, "00000000020260503000").await;
        // The session's content contains "Tokio"; normalize_alias("Tokio") = "tokio";
        // registry.lookup_alias should resolve to the entity created above.
        assert!(
            j.get("q1").unwrap().contains("s1"),
            "real path should mark s1 relevant via tokio→Rust async link, got: {:?}",
            j.get("q1")
        );
    }
}
