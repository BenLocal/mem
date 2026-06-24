//! Entity registry: `entities` + `entity_aliases` tables. Both halves
//! are lance-native and live here — writes (`resolve_or_create`,
//! `add_alias`, the in-write `lookup_alias` precondition) and reads
//! (`get_entity`, `list_entities`).

use arrow_array::{RecordBatch, StringArray};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{
    entity_alias_to_record_batch, entity_to_record_batch, lancedb_err, parse_col, sql_quote,
    LanceStore,
};
use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::storage::types::StorageError;

impl LanceStore {
    pub async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        use crate::pipeline::entity_normalize::normalize_alias;
        let normalized = normalize_alias(alias);

        // Lookup first.
        if let Some(id) = self.lookup_alias(tenant, alias).await? {
            return Ok(id);
        }

        // Auto-promote: insert entity + first alias. No transaction (LanceDB
        // doesn't have them) — under concurrent enqueue the
        // single-writer assumption holds (see embedding_jobs comment).
        let entity_id = uuid::Uuid::now_v7().to_string();
        let entity = Entity {
            entity_id: entity_id.clone(),
            tenant: tenant.to_string(),
            canonical_name: alias.to_string(),
            kind,
            created_at: now.to_string(),
        };
        let entities = self
            .conn
            .open_table("entities")
            .execute()
            .await
            .map_err(lancedb_err)?;
        entities
            .add(entity_to_record_batch(&entity)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let aliases = self
            .conn
            .open_table("entity_aliases")
            .execute()
            .await
            .map_err(lancedb_err)?;
        aliases
            .add(entity_alias_to_record_batch(
                tenant,
                &normalized,
                &entity_id,
                now,
            )?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(entity_id)
    }

    pub async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        use crate::pipeline::entity_normalize::normalize_alias;
        let normalized = normalize_alias(alias);

        // Existing-owner check: who currently owns the normalized form?
        let existing_owner = self.lookup_alias(tenant, alias).await?;
        match existing_owner {
            None => {
                let aliases_table = self
                    .conn
                    .open_table("entity_aliases")
                    .execute()
                    .await
                    .map_err(lancedb_err)?;
                aliases_table
                    .add(entity_alias_to_record_batch(
                        tenant,
                        &normalized,
                        entity_id,
                        now,
                    )?)
                    .execute()
                    .await
                    .map_err(lancedb_err)?;
                Ok(AddAliasOutcome::Inserted)
            }
            Some(owner) if owner == entity_id => Ok(AddAliasOutcome::AlreadyOnSameEntity),
            Some(other) => Ok(AddAliasOutcome::ConflictWithDifferentEntity(other)),
        }
    }

    pub async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        use crate::pipeline::entity_normalize::normalize_alias;
        let normalized = normalize_alias(alias);
        let table = self
            .conn
            .open_table("entity_aliases")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!(
                "tenant = {} AND alias_text = {}",
                sql_quote(tenant),
                sql_quote(&normalized),
            ))
            .limit(1)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        for b in &batches {
            if b.num_rows() == 0 {
                continue;
            }
            let entity_id = b
                .column_by_name("entity_id")
                .ok_or(StorageError::InvalidData("missing entity_id column"))?
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or(StorageError::InvalidData("entity_id column type mismatch"))?;
            return Ok(Some(entity_id.value(0).to_string()));
        }
        Ok(None)
    }

    /// Route-B native equivalent of `DuckDbQuery::get_entity`: fetch the
    /// `entities` row for `(tenant, entity_id)` plus its `entity_aliases`
    /// list ordered `created_at ASC, alias_text ASC`. Returns `Ok(None)`
    /// when no entity row matches.
    ///
    /// Two scans (like the DuckDB impl): one for the entity row, one for
    /// its aliases. LanceDB has no ORDER BY, so the alias ordering is
    /// applied in Rust to match the DuckDB tie-break exactly.
    pub async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        const ENTITIES: &str = "entities";
        const ALIASES: &str = "entity_aliases";

        // ── entity row ──────────────────────────────────────────
        let entities_table = self
            .conn
            .open_table(ENTITIES)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = entities_table
            .query()
            .only_if(format!(
                "tenant = {} AND entity_id = {}",
                sql_quote(tenant),
                sql_quote(entity_id),
            ))
            .limit(1)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut entity: Option<Entity> = None;
        for b in &batches {
            if b.num_rows() == 0 {
                continue;
            }
            let entity_id_col = parse_col::<StringArray>(b, ENTITIES, "entity_id")?;
            let tenant_col = parse_col::<StringArray>(b, ENTITIES, "tenant")?;
            let canonical_name = parse_col::<StringArray>(b, ENTITIES, "canonical_name")?;
            let kind = parse_col::<StringArray>(b, ENTITIES, "kind")?;
            let created_at = parse_col::<StringArray>(b, ENTITIES, "created_at")?;
            let kind_str = kind.value(0);
            let parsed_kind = EntityKind::from_db_str(kind_str).ok_or_else(|| {
                StorageError::InvalidInput(format!("invalid entity kind {kind_str:?}"))
            })?;
            entity = Some(Entity {
                entity_id: entity_id_col.value(0).to_string(),
                tenant: tenant_col.value(0).to_string(),
                canonical_name: canonical_name.value(0).to_string(),
                kind: parsed_kind,
                created_at: created_at.value(0).to_string(),
            });
            break;
        }
        let Some(entity) = entity else {
            return Ok(None);
        };

        // ── alias rows (ordered created_at ASC, alias_text ASC) ──
        let aliases_table = self
            .conn
            .open_table(ALIASES)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = aliases_table
            .query()
            .only_if(format!(
                "tenant = {} AND entity_id = {}",
                sql_quote(tenant),
                sql_quote(entity_id),
            ))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        // (created_at, alias_text) tuples so the Rust sort reproduces the
        // DuckDB `ORDER BY created_at ASC, alias_text ASC`.
        let mut rows: Vec<(String, String)> = Vec::new();
        for b in &batches {
            if b.num_rows() == 0 {
                continue;
            }
            let alias_text = parse_col::<StringArray>(b, ALIASES, "alias_text")?;
            let created_at = parse_col::<StringArray>(b, ALIASES, "created_at")?;
            for i in 0..b.num_rows() {
                rows.push((
                    created_at.value(i).to_string(),
                    alias_text.value(i).to_string(),
                ));
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let aliases = rows.into_iter().map(|(_, alias)| alias).collect();

        Ok(Some(EntityWithAliases { entity, aliases }))
    }

    /// Route-B native equivalent of `DuckDbQuery::list_entities`: scan the
    /// `entities` table for `tenant`, optionally narrowed by `kind_filter`
    /// (exact match on the snake_case kind string) and `query` (a
    /// **case-sensitive substring** match on `canonical_name`, mirroring the
    /// DuckDB `canonical_name LIKE '%q%'`), ordered `created_at DESC`, capped
    /// at `clamp(limit, 1, 1024)`.
    ///
    /// The `kind` equality is pushed into the lance `only_if` predicate; the
    /// substring filter + ordering + limit run in Rust (LanceDB has no
    /// `ORDER BY` and the LIKE-with-user-text is simpler/safer to apply
    /// post-scan than to escape into a predicate). Parity-gated by
    /// `tests/parity_golden.rs`.
    pub async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        const ENTITIES: &str = "entities";
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024) as usize;
        let mut clauses = vec![format!("tenant = {}", sql_quote(tenant))];
        if let Some(k) = kind_filter {
            clauses.push(format!("kind = {}", sql_quote(k.as_db_str())));
        }
        let filter = clauses.join(" AND ");

        let table = self
            .conn
            .open_table(ENTITIES)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(filter)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;

        let mut out: Vec<Entity> = Vec::new();
        for b in &batches {
            if b.num_rows() == 0 {
                continue;
            }
            let entity_id = parse_col::<StringArray>(b, ENTITIES, "entity_id")?;
            let tenant_col = parse_col::<StringArray>(b, ENTITIES, "tenant")?;
            let canonical_name = parse_col::<StringArray>(b, ENTITIES, "canonical_name")?;
            let kind = parse_col::<StringArray>(b, ENTITIES, "kind")?;
            let created_at = parse_col::<StringArray>(b, ENTITIES, "created_at")?;
            for i in 0..b.num_rows() {
                let name = canonical_name.value(i);
                // `canonical_name LIKE '%q%'` — case-sensitive substring.
                if let Some(q) = query {
                    if !name.contains(q) {
                        continue;
                    }
                }
                let kind_str = kind.value(i);
                let parsed_kind = EntityKind::from_db_str(kind_str).ok_or_else(|| {
                    StorageError::InvalidInput(format!("invalid entity kind {kind_str:?}"))
                })?;
                out.push(Entity {
                    entity_id: entity_id.value(i).to_string(),
                    tenant: tenant_col.value(i).to_string(),
                    canonical_name: name.to_string(),
                    kind: parsed_kind,
                    created_at: created_at.value(i).to_string(),
                });
            }
        }
        // ORDER BY created_at DESC, then LIMIT.
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out.truncate(lim);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AddAliasOutcome;
    use tempfile::tempdir;

    /// Writes-only round trip: `resolve_or_create` + `add_alias` +
    /// `lookup_alias`. The read-shape assertions (`get_entity`,
    /// `list_entities` with kind / LIKE filters) live in
    /// `tests/entity_registry.rs` — they seed via `resolve_or_create` /
    /// `add_alias` then read back through the lance-native
    /// `get_entity` / `list_entities`.
    #[tokio::test]
    pub async fn lancedb_entity_registry_writes_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let id1 = repo
            .resolve_or_create(
                "tenant-a",
                "Rust Async",
                EntityKind::Topic,
                "00000001778000000000",
            )
            .await
            .unwrap();
        // Same alias under different casing/whitespace → same entity.
        let id1b = repo
            .resolve_or_create(
                "tenant-a",
                "  rust   ASYNC  ",
                EntityKind::Topic,
                "00000001778000000001",
            )
            .await
            .unwrap();
        assert_eq!(id1, id1b, "normalized alias must round-trip to same entity");

        let id2 = repo
            .resolve_or_create(
                "tenant-a",
                "DuckDB",
                EntityKind::Project,
                "00000001778000000002",
            )
            .await
            .unwrap();
        assert_ne!(id1, id2);

        // Different tenant, same alias → distinct entity.
        let id3 = repo
            .resolve_or_create(
                "tenant-b",
                "Rust Async",
                EntityKind::Topic,
                "00000001778000000003",
            )
            .await
            .unwrap();
        assert_ne!(id1, id3);

        // add_alias: new alias on same entity → Inserted.
        let r1 = repo
            .add_alias("tenant-a", &id1, "Tokio", "00000001778000000010")
            .await
            .unwrap();
        assert_eq!(r1, AddAliasOutcome::Inserted);

        // Same alias re-added → AlreadyOnSameEntity (idempotent).
        let r2 = repo
            .add_alias("tenant-a", &id1, "tokio", "00000001778000000011")
            .await
            .unwrap();
        assert_eq!(r2, AddAliasOutcome::AlreadyOnSameEntity);

        // Different entity claiming the same alias → Conflict.
        let r3 = repo
            .add_alias("tenant-a", &id2, "Tokio", "00000001778000000012")
            .await
            .unwrap();
        assert_eq!(
            r3,
            AddAliasOutcome::ConflictWithDifferentEntity(id1.clone())
        );

        // lookup_alias short-circuit — this read stays inherent on
        // LanceStore because `resolve_or_create` and `add_alias` use
        // it as a precondition.
        let look = repo.lookup_alias("tenant-a", "Rust Async").await.unwrap();
        assert_eq!(look.as_deref(), Some(id1.as_str()));
        let look_none = repo.lookup_alias("tenant-a", "unknown").await.unwrap();
        assert!(look_none.is_none());
    }
}
