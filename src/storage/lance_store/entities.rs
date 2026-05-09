//! Entity registry: `entities` + `entity_aliases` tables. Methods
//! previously bound by the `EntityRegistry` trait, now inherent on
//! `LanceStore`.

use arrow_array::{RecordBatch, StringArray};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{
    entity_alias_to_record_batch, entity_to_record_batch, lancedb_err, record_batch_to_entities,
    sql_quote, LanceStore,
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

    pub async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        let entities = self
            .conn
            .open_table("entities")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = entities
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
        let mut entity_iter = batches
            .iter()
            .flat_map(|b| record_batch_to_entities(b).unwrap_or_default().into_iter());
        let Some(entity) = entity_iter.next() else {
            return Ok(None);
        };

        // Pull aliases for this entity, sorted by created_at ASC then
        // alias_text ASC (mirror DuckDB SQL).
        let aliases_table = self
            .conn
            .open_table("entity_aliases")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream2 = aliases_table
            .query()
            .only_if(format!(
                "tenant = {} AND entity_id = {}",
                sql_quote(tenant),
                sql_quote(entity_id),
            ))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches2: Vec<RecordBatch> = stream2
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut alias_rows: Vec<(String, String)> = Vec::new(); // (created_at, alias_text)
        for b in &batches2 {
            fn col<'a, T: 'static>(
                batch: &'a RecordBatch,
                name: &'static str,
            ) -> Result<&'a T, StorageError> {
                batch
                    .column_by_name(name)
                    .ok_or(StorageError::InvalidData("missing column"))?
                    .as_any()
                    .downcast_ref::<T>()
                    .ok_or(StorageError::InvalidData("column type mismatch"))
            }
            let alias_text = col::<StringArray>(b, "alias_text")?;
            let created_at = col::<StringArray>(b, "created_at")?;
            for i in 0..b.num_rows() {
                alias_rows.push((
                    created_at.value(i).to_string(),
                    alias_text.value(i).to_string(),
                ));
            }
        }
        alias_rows.sort();
        let aliases: Vec<String> = alias_rows.into_iter().map(|(_, a)| a).collect();
        Ok(Some(EntityWithAliases { entity, aliases }))
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

    pub async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        let mut filter = format!("tenant = {}", sql_quote(tenant));
        if let Some(k) = kind_filter {
            filter.push_str(&format!(" AND kind = {}", sql_quote(k.as_db_str())));
        }
        // canonical_name LIKE '%query%' — LanceDB's filter parser accepts
        // SQL LIKE patterns with `%` wildcards.
        if let Some(q) = query {
            filter.push_str(&format!(
                " AND canonical_name LIKE {}",
                sql_quote(&format!("%{q}%")),
            ));
        }
        let table = self
            .conn
            .open_table("entities")
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
        let mut entities = Vec::new();
        for b in &batches {
            entities.extend(record_batch_to_entities(b)?);
        }
        // ORDER BY created_at DESC — sort in-memory.
        entities.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        entities.truncate(limit);
        Ok(entities)
    }
}
