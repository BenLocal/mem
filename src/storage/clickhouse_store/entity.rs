//! `EntityRegistry` for [`ClickHouseBackend`].
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P5).** `entities` + `entity_aliases`, both
//! `ReplacingMergeTree(row_version)`. Alias PK is (tenant, alias_text);
//! alias strings are normalized (lowercase + ws-collapsed) via the shared
//! `pipeline::entity_normalize::normalize_alias`, same as the lance path.

use async_trait::async_trait;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::backend::ClickHouseBackend;
use super::capsule_store::{ch_err, enum_to_str, now_version};
use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::pipeline::entity_normalize::normalize_alias;
use crate::storage::types::StorageError;
use crate::storage::EntityRegistry;

#[derive(Row, Serialize, Deserialize)]
struct ChEntityRow {
    entity_id: String,
    tenant: String,
    canonical_name: String,
    kind: String,
    created_at: String,
    row_version: u64,
}

#[derive(Row, Serialize, Deserialize)]
struct ChAliasRow {
    tenant: String,
    alias_text: String,
    entity_id: String,
    created_at: String,
    row_version: u64,
}

fn kind_from_str(s: &str) -> EntityKind {
    match s {
        "project" => EntityKind::Project,
        "repo" => EntityKind::Repo,
        "module" => EntityKind::Module,
        "workflow" => EntityKind::Workflow,
        "tag" => EntityKind::Tag,
        "file" => EntityKind::File,
        _ => EntityKind::Topic,
    }
}

impl ClickHouseBackend {
    async fn ch_lookup_alias(
        &self,
        tenant: &str,
        normalized: &str,
    ) -> Result<Option<String>, StorageError> {
        let ids = self
            .client
            .query(
                "SELECT entity_id FROM entity_aliases FINAL \
                 WHERE tenant = ? AND alias_text = ? ORDER BY row_version DESC LIMIT 1",
            )
            .bind(tenant)
            .bind(normalized)
            .fetch_all::<String>()
            .await
            .map_err(ch_err)?;
        Ok(ids.into_iter().next().filter(|s| !s.is_empty()))
    }

    async fn ch_insert_alias(
        &self,
        tenant: &str,
        normalized: &str,
        entity_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let mut insert = self
            .client
            .insert::<ChAliasRow>("entity_aliases")
            .await
            .map_err(ch_err)?;
        insert
            .write(&ChAliasRow {
                tenant: tenant.to_owned(),
                alias_text: normalized.to_owned(),
                entity_id: entity_id.to_owned(),
                created_at: now.to_owned(),
                row_version: now_version(),
            })
            .await
            .map_err(ch_err)?;
        insert.end().await.map_err(ch_err)?;
        Ok(())
    }
}

#[async_trait]
impl EntityRegistry for ClickHouseBackend {
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        let normalized = normalize_alias(alias);
        if let Some(existing) = self.ch_lookup_alias(tenant, &normalized).await? {
            return Ok(existing);
        }
        let entity_id = uuid::Uuid::now_v7().to_string();
        let mut insert = self
            .client
            .insert::<ChEntityRow>("entities")
            .await
            .map_err(ch_err)?;
        insert
            .write(&ChEntityRow {
                entity_id: entity_id.clone(),
                tenant: tenant.to_owned(),
                canonical_name: alias.to_owned(),
                kind: enum_to_str(&kind),
                created_at: now.to_owned(),
                row_version: now_version(),
            })
            .await
            .map_err(ch_err)?;
        insert.end().await.map_err(ch_err)?;
        self.ch_insert_alias(tenant, &normalized, &entity_id, now)
            .await?;
        Ok(entity_id)
    }

    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        let normalized = normalize_alias(alias);
        match self.ch_lookup_alias(tenant, &normalized).await? {
            Some(owner) if owner == entity_id => Ok(AddAliasOutcome::AlreadyOnSameEntity),
            Some(other) => Ok(AddAliasOutcome::ConflictWithDifferentEntity(other)),
            None => {
                self.ch_insert_alias(tenant, &normalized, entity_id, now)
                    .await?;
                Ok(AddAliasOutcome::Inserted)
            }
        }
    }

    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM entities FINAL \
                 WHERE tenant = ? AND entity_id = ? ORDER BY row_version DESC LIMIT 1",
            )
            .bind(tenant)
            .bind(entity_id)
            .fetch_all::<ChEntityRow>()
            .await
            .map_err(ch_err)?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let aliases = self
            .client
            .query(
                "SELECT alias_text FROM entity_aliases FINAL \
                 WHERE tenant = ? AND entity_id = ? ORDER BY created_at ASC, alias_text ASC",
            )
            .bind(tenant)
            .bind(entity_id)
            .fetch_all::<String>()
            .await
            .map_err(ch_err)?;
        Ok(Some(EntityWithAliases {
            entity: Entity {
                entity_id: row.entity_id,
                tenant: row.tenant,
                canonical_name: row.canonical_name,
                kind: kind_from_str(&row.kind),
                created_at: row.created_at,
            },
            aliases,
        }))
    }

    async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        let normalized = normalize_alias(alias);
        self.ch_lookup_alias(tenant, &normalized).await
    }

    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        let mut sql = String::from("SELECT ?fields FROM entities FINAL WHERE tenant = ?");
        if kind_filter.is_some() {
            sql.push_str(" AND kind = ?");
        }
        if query.is_some() {
            sql.push_str(" AND positionCaseInsensitiveUTF8(canonical_name, ?) > 0");
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        let mut q = self.client.query(&sql).bind(tenant);
        if let Some(k) = kind_filter {
            q = q.bind(enum_to_str(&k));
        }
        if let Some(qq) = query {
            q = q.bind(qq);
        }
        let rows = q
            .bind(limit as u64)
            .fetch_all::<ChEntityRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows
            .into_iter()
            .map(|r| Entity {
                entity_id: r.entity_id,
                tenant: r.tenant,
                canonical_name: r.canonical_name,
                kind: kind_from_str(&r.kind),
                created_at: r.created_at,
            })
            .collect())
    }
}
