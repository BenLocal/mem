//! Façade for the entity-registry HTTP layer. See spec
//! docs/superpowers/specs/2026-05-02-entity-registry-design.md.
//!
//! Thin shim over [`EntityRegistry`] (the storage trait): the HTTP layer
//! never touches `DuckDbRepository` directly. Method bodies stay simple —
//! the only non-trivial helper is `create_with_aliases`, which sequences
//! `resolve_or_create` + zero-or-more `add_alias` calls and re-fetches the
//! resulting [`EntityWithAliases`] for the response body.

use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::storage::{DuckDbRepository, EntityRegistry, StorageError};

#[derive(Clone)]
pub struct EntityService {
    repo: DuckDbRepository,
}

impl EntityService {
    pub fn new(repo: DuckDbRepository) -> Self {
        Self { repo }
    }

    /// Create or resolve `canonical_name` (caller's verbatim input) under
    /// `tenant`, then attach each item in `aliases` to the resulting
    /// entity. The optional aliases use `add_alias` semantics: a conflict
    /// (alias already owned by a *different* entity) propagates as
    /// `StorageError::InvalidInput` so the HTTP layer returns 400. (The
    /// 409 path is reserved for the explicit `POST .../aliases` endpoint
    /// where the caller deliberately targets one entity_id.)
    pub async fn create_with_aliases(
        &self,
        tenant: &str,
        canonical_name: &str,
        kind: EntityKind,
        aliases: &[String],
        now: &str,
    ) -> Result<EntityWithAliases, StorageError> {
        let entity_id = self
            .repo
            .resolve_or_create(tenant, canonical_name, kind, now)
            .await?;
        for alias in aliases {
            match self.repo.add_alias(tenant, &entity_id, alias, now).await? {
                AddAliasOutcome::Inserted | AddAliasOutcome::AlreadyOnSameEntity => {}
                AddAliasOutcome::ConflictWithDifferentEntity(other) => {
                    return Err(StorageError::InvalidInput(format!(
                        "alias {alias:?} is already owned by entity {other}"
                    )));
                }
            }
        }
        self.repo
            .get_entity(tenant, &entity_id)
            .await?
            .ok_or_else(|| StorageError::InvalidInput("entity disappeared after creation".into()))
    }

    pub async fn get(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        self.repo.get_entity(tenant, entity_id).await
    }

    pub async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        self.repo.add_alias(tenant, entity_id, alias, now).await
    }

    pub async fn list(
        &self,
        tenant: &str,
        kind: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        self.repo.list_entities(tenant, kind, query, limit).await
    }
}
