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

/// Error variants returned by [`EntityService::create_with_aliases`].
///
/// Distinguishes a structured cross-entity alias conflict (so the HTTP
/// layer can return 409 + the conflicting owner per spec line 436) from
/// generic storage errors that flow through `AppError` as-is.
#[derive(Debug, thiserror::Error)]
pub enum CreateWithAliasesError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("alias {conflicting_alias:?} is already owned by entity {existing_entity_id}")]
    AliasConflict {
        existing_entity_id: String,
        conflicting_alias: String,
    },
}

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
    /// entity.
    ///
    /// Returns 409-mappable [`CreateWithAliasesError::AliasConflict`] if any
    /// alias is already bound to a different entity.
    ///
    /// Read-then-write contract: the helper first runs a `lookup_alias`
    /// pre-check on every requested alias to ensure none belong to a
    /// different entity, *before* any writes. DuckDB's single
    /// `Arc<Mutex<Connection>>` serializes all writes in this process, so
    /// the brief read window followed by writes under the same caller is
    /// race-free for in-process callers; the pre-check exists primarily to
    /// avoid partial-state on conflict (no entity / alias rows leak when
    /// one of N aliases is contested).
    pub async fn create_with_aliases(
        &self,
        tenant: &str,
        canonical_name: &str,
        kind: EntityKind,
        aliases: &[String],
        now: &str,
    ) -> Result<EntityWithAliases, CreateWithAliasesError> {
        // Pre-check: does any requested alias already belong to a *different*
        // entity? If so, fail before any writes. We don't yet know the
        // target entity_id (resolve_or_create may auto-promote), so we only
        // flag aliases owned by an entity whose canonical_name's normalized
        // form differs from the request — proxied via the canonical_name's
        // own normalized lookup below.
        let canonical_owner = self.repo.lookup_alias(tenant, canonical_name).await?;
        for alias in aliases {
            if let Some(owner) = self.repo.lookup_alias(tenant, alias).await? {
                // If the canonical_name already maps to an entity, that's
                // the would-be target; conflict only if the alias owner
                // differs from it.
                let conflicts = match &canonical_owner {
                    Some(target) => &owner != target,
                    // No prior canonical owner ⇒ resolve_or_create will
                    // either reuse a sibling-alias owner or auto-promote a
                    // new entity. In either case, an alias already owned by
                    // some *other* entity is a conflict.
                    None => true,
                };
                if conflicts {
                    return Err(CreateWithAliasesError::AliasConflict {
                        existing_entity_id: owner,
                        conflicting_alias: alias.clone(),
                    });
                }
            }
        }

        let entity_id = self
            .repo
            .resolve_or_create(tenant, canonical_name, kind, now)
            .await?;
        for alias in aliases {
            match self.repo.add_alias(tenant, &entity_id, alias, now).await? {
                AddAliasOutcome::Inserted | AddAliasOutcome::AlreadyOnSameEntity => {}
                AddAliasOutcome::ConflictWithDifferentEntity(other) => {
                    // Defense-in-depth: pre-check should have caught this,
                    // but propagate as the same typed conflict if a racer
                    // (out-of-process) slipped in.
                    return Err(CreateWithAliasesError::AliasConflict {
                        existing_entity_id: other,
                        conflicting_alias: alias.clone(),
                    });
                }
            }
        }
        let with_aliases = self
            .repo
            .get_entity(tenant, &entity_id)
            .await?
            .ok_or_else(|| {
                StorageError::InvalidInput("entity disappeared after creation".into())
            })?;
        Ok(with_aliases)
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
