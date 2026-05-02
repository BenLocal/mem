//! `EntityRegistry` impl for `DuckDbRepository`. See spec
//! docs/superpowers/specs/2026-05-02-entity-registry-design.md.
//!
//! Mirrors the file layout of `transcript_repo.rs`: a separate module that
//! extends `DuckDbRepository` with a focused trait implementation while
//! reusing the same `Arc<Mutex<Connection>>` (via `self.conn()`).
//!
//! The `resolve_or_create` flow holds the mutex across SELECT-then-INSERT-INSERT
//! so the auto-promote case is race-free. `add_alias` reads the existing
//! owner first, then either INSERTs (new alias), reports
//! `AlreadyOnSameEntity` (idempotent re-add), or returns
//! `ConflictWithDifferentEntity(other_id)` — all under the same lock hold.

use async_trait::async_trait;
use duckdb::OptionalExt;

use super::duckdb::{DuckDbRepository, EntityRegistry, StorageError};
use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::pipeline::entity_normalize::normalize_alias;

#[async_trait]
impl EntityRegistry for DuckDbRepository {
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        let normalized = normalize_alias(alias);
        let conn = self.conn()?;

        // Lookup first.
        let existing: Option<String> = conn
            .query_row(
                "select entity_id from entity_aliases \
                 where tenant = ?1 and alias_text = ?2",
                duckdb::params![tenant, normalized],
                |r| r.get(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)?;
        if let Some(id) = existing {
            return Ok(id);
        }

        // Auto-promote. Both INSERTs run while the mutex is held.
        let entity_id = uuid::Uuid::now_v7().to_string();
        conn.execute(
            "insert into entities (entity_id, tenant, canonical_name, kind, created_at) \
             values (?1, ?2, ?3, ?4, ?5)",
            duckdb::params![entity_id, tenant, alias, kind.as_db_str(), now],
        )?;
        conn.execute(
            "insert into entity_aliases (tenant, alias_text, entity_id, created_at) \
             values (?1, ?2, ?3, ?4)",
            duckdb::params![tenant, normalized, entity_id, now],
        )?;
        Ok(entity_id)
    }

    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        let conn = self.conn()?;
        let entity = conn
            .query_row(
                "select entity_id, tenant, canonical_name, kind, created_at \
                 from entities where tenant = ?1 and entity_id = ?2",
                duckdb::params![tenant, entity_id],
                |r| -> duckdb::Result<Entity> {
                    let kind_s: String = r.get(3)?;
                    Ok(Entity {
                        entity_id: r.get(0)?,
                        tenant: r.get(1)?,
                        canonical_name: r.get(2)?,
                        kind: EntityKind::from_db_str(&kind_s).ok_or_else(|| {
                            duckdb::Error::FromSqlConversionFailure(
                                3,
                                duckdb::types::Type::Text,
                                format!("invalid kind: {kind_s}").into(),
                            )
                        })?,
                        created_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::DuckDb)?;
        let Some(entity) = entity else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "select alias_text from entity_aliases \
             where tenant = ?1 and entity_id = ?2 \
             order by created_at asc, alias_text asc",
        )?;
        let aliases: Vec<String> = stmt
            .query_map(duckdb::params![tenant, entity_id], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<duckdb::Result<Vec<_>>>()?;
        Ok(Some(EntityWithAliases { entity, aliases }))
    }

    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        let normalized = normalize_alias(alias);
        let conn = self.conn()?;

        let existing_owner: Option<String> = conn
            .query_row(
                "select entity_id from entity_aliases \
                 where tenant = ?1 and alias_text = ?2",
                duckdb::params![tenant, normalized],
                |r| r.get(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)?;

        match existing_owner {
            None => {
                conn.execute(
                    "insert into entity_aliases (tenant, alias_text, entity_id, created_at) \
                     values (?1, ?2, ?3, ?4)",
                    duckdb::params![tenant, normalized, entity_id, now],
                )?;
                Ok(AddAliasOutcome::Inserted)
            }
            Some(owner) if owner == entity_id => Ok(AddAliasOutcome::AlreadyOnSameEntity),
            Some(other) => Ok(AddAliasOutcome::ConflictWithDifferentEntity(other)),
        }
    }

    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        let conn = self.conn()?;
        let mut sql = String::from(
            "select entity_id, tenant, canonical_name, kind, created_at \
             from entities where tenant = ?1",
        );
        let mut params: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant.to_string())];

        if let Some(k) = kind_filter {
            sql.push_str(" and kind = ?");
            sql.push_str(&format!("{}", params.len() + 1));
            params.push(Box::new(k.as_db_str().to_string()));
        }
        if let Some(q) = query {
            sql.push_str(" and canonical_name like ?");
            sql.push_str(&format!("{}", params.len() + 1));
            params.push(Box::new(format!("%{q}%")));
        }
        sql.push_str(" order by created_at desc limit ?");
        sql.push_str(&format!("{}", params.len() + 1));
        params.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |r| -> duckdb::Result<Entity> {
            let kind_s: String = r.get(3)?;
            Ok(Entity {
                entity_id: r.get(0)?,
                tenant: r.get(1)?,
                canonical_name: r.get(2)?,
                kind: EntityKind::from_db_str(&kind_s).ok_or_else(|| {
                    duckdb::Error::FromSqlConversionFailure(
                        3,
                        duckdb::types::Type::Text,
                        format!("invalid kind: {kind_s}").into(),
                    )
                })?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<duckdb::Result<Vec<_>>>()?)
    }
}
