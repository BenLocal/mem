//! Backend-agnostic entity registry — Phase 3 sub-trait.
//!
//! Canonicalizes alias strings to stable `entity_id` (UUIDv7).
//! Aliases are normalized (lowercase + whitespace-collapsed) at the
//! PK; `canonical_name` preserves caller verbatim. Tenant-scoped.
//!
//! See `docs/backend-coupling.md` §3.1 + §6.4.

use async_trait::async_trait;

use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::storage::types::StorageError;
use crate::storage::Store;

#[async_trait]
pub trait EntityRegistry: Send + Sync {
    /// Get the canonical `entity_id` for `(tenant, alias)`, creating
    /// a new entity row if the alias doesn't exist yet. Returns the
    /// entity id (UUIDv7).
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError>;

    /// Add a new alias to an existing entity. Returns
    /// [`AddAliasOutcome::Added`] on success,
    /// [`AddAliasOutcome::AlreadyOnEntity`] if the alias is already
    /// linked to this entity, or [`AddAliasOutcome::Conflict`] if
    /// it belongs to a different entity (HTTP layer maps this to
    /// 409).
    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError>;

    /// Hydrate an entity row + all its aliases.
    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError>;

    /// Reverse lookup: alias text → canonical `entity_id`, if any.
    async fn lookup_alias(&self, tenant: &str, alias: &str)
        -> Result<Option<String>, StorageError>;

    /// List entities for `tenant`, optionally filtered by kind and
    /// a free-text query (LIKE against canonical_name + aliases).
    /// Clamped by `limit`.
    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError>;
}

#[async_trait]
impl EntityRegistry for Store {
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        Store::resolve_or_create(self, tenant, alias, kind, now).await
    }

    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        Store::add_alias(self, tenant, entity_id, alias, now).await
    }

    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        Store::get_entity(self, tenant, entity_id).await
    }

    async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        Store::lookup_alias(self, tenant, alias).await
    }

    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        Store::list_entities(self, tenant, kind_filter, query, limit).await
    }
}
