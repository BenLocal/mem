//! Backend-agnostic trait for capsule CRUD + lifecycle. The Phase 2
//! validation step of `docs/backend-coupling.md` §6.3: prove that a
//! narrow trait surface + a real alternate backend (in-memory
//! HashMap) can coexist with the Lance-backed `Store`, before
//! committing to the full multi-trait sweep in Phase 3+.
//!
//! Scope notes:
//!
//! - **Capsule CRUD only**. Search (BM25/ANN/hybrid), embedding-job
//!   queues, graph edges, transcripts, entities, sessions, vacuum —
//!   all belong to other sub-traits that will land in Phase 3+. This
//!   trait is intentionally narrow to keep the validation tractable.
//! - **No defaults**. Every method is required. Optional defaults
//!   (e.g. deriving `list_pending_review` from `list_for_tenant` +
//!   a status filter) were considered but deferred — the in-memory
//!   backend is small enough that hand-writing each method is
//!   clearer than wiring default impls.
//! - **Service migrated in Phase 5**: `capability_capsule_service`
//!   now holds `Arc<dyn Backend>` (umbrella supertrait); this trait
//!   is one of the nine that compose it.
//! - **Error type**: shared [`StorageError`]. The doc §3.3 mentions
//!   future `BackendError(Box<...>)` transparency; today's
//!   `StorageError` variants suffice.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapsuleStats, FeedbackSummary,
};
use crate::storage::types::{FeedbackEvent, StorageError};
use crate::storage::Store;

/// Backend-agnostic capsule CRUD + lifecycle. Phase 2 validation
/// surface — see module docs.
#[async_trait]
pub trait CapsuleStore: Send + Sync {
    /// Insert a single capsule row. Returns the stored record
    /// (identical to input today; future backends may stamp
    /// server-side fields and return the updated row).
    async fn insert_capability_capsule(
        &self,
        memory: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError>;

    /// Bulk insert. No-op when `memories` is empty. Caller is
    /// responsible for upstream idempotency dedup.
    async fn insert_capability_capsules(
        &self,
        memories: &[CapabilityCapsuleRecord],
    ) -> Result<(), StorageError>;

    /// Cross-tenant lookup by id (admin / version-chain walk).
    async fn get_capability_capsule(
        &self,
        capability_capsule_id: String,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError>;

    /// Tenant-scoped lookup — the common read path.
    async fn get_capability_capsule_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError>;

    /// Lookup but only if status == `PendingConfirmation`. Returns
    /// `Ok(None)` if the row exists but is in another status.
    async fn get_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError>;

    /// Idempotency / dedup probe. Returns the existing row when
    /// either `idempotency_key` matches or `content_hash` matches
    /// any live (non-rejected, non-archived) row for the tenant.
    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError>;

    /// Full live-status set for the tenant (excludes
    /// `Rejected` / `Archived` and `Diary` type). Caller filters /
    /// ranks downstream.
    async fn list_capability_capsules_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError>;

    /// `status = PendingConfirmation` only.
    async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError>;

    /// Bulk get-by-ids. Result order is not guaranteed to match
    /// input order (callers reshape via HashMap if needed). Empty
    /// `ids` short-circuits to `Ok(vec![])`.
    async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError>;

    /// `PendingConfirmation → Active` transition. Returns the row
    /// post-update.
    async fn accept_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError>;

    /// `PendingConfirmation → Rejected` transition. Returns the row
    /// post-update.
    async fn reject_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError>;

    /// Supersede a pending row with an edited active successor.
    /// Two-op semantics (mark old + insert new) — see the
    /// LANCE-SPECIFIC note on [`Store::replace_pending_with_successor`].
    ///
    /// **Trait contract — terminal state of the original**: the
    /// original capsule's status MUST end up as
    /// [`CapabilityCapsuleStatus::Rejected`] after this call. The
    /// `Rejected` choice (vs. `Archived`) is load-bearing — callers
    /// (`capability_capsule_service::edit_and_accept_pending`,
    /// version-chain walks, ranking exclusion lists) treat the two
    /// terminal statuses differently. Backend implementations that
    /// would naturally prefer `Archived` (e.g. a future Postgres
    /// backend with stricter audit semantics) must still write
    /// `Rejected` on this path.
    ///
    /// **Atomicity contract — NOT atomic across backends**. The trait
    /// makes NO guarantee that the "mark old as Rejected" and "insert
    /// successor" writes commit together. On a backend with
    /// transactions (Postgres) the implementation MAY wrap both in
    /// `BEGIN/COMMIT` so the window does not exist for that backend's
    /// callers — but the trait surface is the lowest common
    /// denominator. Concretely on the Lance backend a process crash
    /// between the two writes can leave the database with the
    /// original marked Rejected and no successor row (or, less
    /// commonly, the successor inserted with the original still
    /// PendingConfirmation). Callers MUST treat the two writes as
    /// independently observable on crash recovery and MUST NOT
    /// assume that reading `original` post-crash implies the
    /// successor is also visible. Per doc §3.3 (no `transaction()`
    /// method on the trait), upgrading this to a true atomic
    /// guarantee is rejected — it would force Lance to emulate
    /// rollback at the application layer, which the codebase has
    /// deliberately chosen not to do.
    ///
    /// The successor is returned post-insert; its
    /// `supersedes_capability_capsule_id` is expected to point at
    /// `original_memory_id` (caller-supplied via the input record).
    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError>;

    /// Apply a feedback event — writes the audit row and mutates
    /// the parent capsule's `confidence` / `decay_score` / `status`
    /// per the feedback kind.
    ///
    /// **Atomicity contract — NOT atomic across backends**. Same
    /// shape as [`Self::replace_pending_with_successor`]: this is a
    /// two-op operation (`INSERT INTO feedback_events` then
    /// `UPDATE capability_capsules`) and the trait makes NO guarantee
    /// they commit together. A backend MAY use a real transaction
    /// (the Postgres backend does); the Lance backend cannot. On
    /// crash recovery the caller MUST be prepared for: audit row
    /// present without the parent confidence / decay / status delta
    /// applied — or, less commonly, the parent delta applied without
    /// a corresponding audit row. Repeated `apply_feedback` calls
    /// with the same `feedback_id` are not deduplicated at this
    /// layer; idempotency, if needed, lives upstream.
    async fn apply_feedback(
        &self,
        memory: &CapabilityCapsuleRecord,
        feedback: FeedbackEvent,
    ) -> Result<CapabilityCapsuleRecord, StorageError>;

    /// Hard delete (irreversible). Trait contract: implementations
    /// MUST cascade to remove dependent rows in `feedback_events`,
    /// `embedding_jobs`, and `capability_capsule_embeddings` so the
    /// caller does not have to choreograph satellite cleanup. Graph
    /// edges where this capsule is the FROM node SHOULD be **closed**
    /// (`valid_to = now`) rather than deleted, preserving the
    /// time-travel `graph_edges.valid_from / valid_to` semantics.
    ///
    /// **Atomicity contract — NOT atomic across backends.** Same
    /// shape as [`Self::replace_pending_with_successor`] and
    /// [`Self::apply_feedback`]: backends that have transactions
    /// (Postgres) MAY wrap the cascade in `BEGIN/COMMIT`; Lance
    /// cannot. Callers MUST be prepared for partial-state failures
    /// (capsule row gone, one or more satellite tables still
    /// carrying orphans) and SHOULD retry on cascade errors — every
    /// cascade helper is idempotent on empty-set inputs.
    ///
    /// Returns `Err(InvalidData("memory not found"))` when the
    /// capsule row doesn't exist (no cascade attempted in that case).
    async fn delete_capability_capsule_hard(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError>;

    /// Aggregate feedback signals for one capsule (counts per kind).
    async fn feedback_summary(
        &self,
        capability_capsule_id: &str,
    ) -> Result<FeedbackSummary, StorageError>;

    /// Distinct `project` names for the tenant, sorted ascending.
    /// Capsules with `NULL` project are dropped from the list (every
    /// entry is a real project name). Powers the navigation sidebar
    /// in MCP / HTTP clients (`capability_capsule_list_wings`).
    async fn list_wings(&self, tenant: &str) -> Result<Vec<String>, StorageError>;

    /// Capsule-pool snapshot: total + per-status counts for the
    /// tenant. Status names that don't map to a `CapabilityCapsuleStatus`
    /// variant (forward-compat with future enum additions) are
    /// silently dropped — `pending_confirmation + provisional +
    /// active + archived + rejected != total` is the caller's
    /// detection signal.
    async fn capsule_stats(&self, tenant: &str) -> Result<CapsuleStats, StorageError>;

    /// Two-level `(project, repos)` taxonomy for the tenant. Sorted
    /// by project then repo. A project with no recorded repos appears
    /// as `(project, vec![])`. Powers `capability_capsule_get_taxonomy`
    /// in MCP / HTTP — lets clients render a project → repo tree in
    /// one round trip.
    async fn get_taxonomy(&self, tenant: &str) -> Result<Vec<(String, Vec<String>)>, StorageError>;

    /// Tenant-scoped capsule list with optional filters and cursor
    /// pagination. Each `Option<&str>` filter is independently
    /// no-op'd (None = don't restrict, not "must be NULL"). Returns
    /// `(rows, has_more)` — `has_more` uses the standard `LIMIT N+1`
    /// trick so the caller can decide whether to paginate. `cursor`
    /// is `(updated_at, capability_capsule_id)` for after-cursor
    /// resumption; rows ordered `updated_at DESC, capability_capsule_id
    /// ASC` to match the cursor invariant.
    #[allow(clippy::too_many_arguments)]
    async fn list_capability_capsules_in_scope(
        &self,
        tenant: &str,
        project: Option<&str>,
        repo: Option<&str>,
        module: Option<&str>,
        capsule_type: Option<&str>,
        status: Option<&str>,
        source_agent: Option<&str>,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<(Vec<CapabilityCapsuleRecord>, bool), StorageError>;
}

// ── Lance-backed impl: pure delegation through Store ───────────────
//
// The existing `Store` wires LanceStore writes + DuckDbQuery reads
// + the refresh ceremony. Implementing the trait on `Store` is the
// cheapest way to give the existing production path a trait
// surface; downstream callers get to choose between `Arc<Store>`
// (concrete, all methods available) and `Arc<dyn CapsuleStore>`
// (narrow, swappable).

#[async_trait]
impl CapsuleStore for Store {
    async fn insert_capability_capsule(
        &self,
        memory: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        Store::insert_capability_capsule(self, memory).await
    }

    async fn insert_capability_capsules(
        &self,
        memories: &[CapabilityCapsuleRecord],
    ) -> Result<(), StorageError> {
        Store::insert_capability_capsules(self, memories).await
    }

    async fn get_capability_capsule(
        &self,
        capability_capsule_id: String,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        self.lance
            .get_capability_capsule(capability_capsule_id)
            .await
    }

    async fn get_capability_capsule_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        Store::get_capability_capsule_for_tenant(self, tenant, capability_capsule_id).await
    }

    async fn get_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        Store::get_pending(self, tenant, capability_capsule_id).await
    }

    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        Store::find_by_idempotency_or_hash(self, tenant, idempotency_key, content_hash).await
    }

    async fn list_capability_capsules_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        Store::list_capability_capsules_for_tenant(self, tenant).await
    }

    async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        Store::list_pending_review(self, tenant).await
    }

    async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        Store::fetch_capability_capsules_by_ids(self, tenant, ids).await
    }

    async fn accept_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        Store::accept_pending(self, tenant, capability_capsule_id).await
    }

    async fn reject_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        Store::reject_pending(self, tenant, capability_capsule_id).await
    }

    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        Store::replace_pending_with_successor(self, tenant, original_memory_id, successor).await
    }

    async fn apply_feedback(
        &self,
        memory: &CapabilityCapsuleRecord,
        feedback: FeedbackEvent,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        Store::apply_feedback(self, memory, feedback).await
    }

    async fn delete_capability_capsule_hard(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        Store::delete_capability_capsule_hard(self, tenant, capability_capsule_id).await
    }

    async fn feedback_summary(
        &self,
        capability_capsule_id: &str,
    ) -> Result<FeedbackSummary, StorageError> {
        self.lance.feedback_summary(capability_capsule_id).await
    }

    async fn list_wings(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        Store::list_wings(self, tenant).await
    }

    async fn capsule_stats(&self, tenant: &str) -> Result<CapsuleStats, StorageError> {
        Store::capsule_stats(self, tenant).await
    }

    async fn get_taxonomy(&self, tenant: &str) -> Result<Vec<(String, Vec<String>)>, StorageError> {
        Store::get_taxonomy(self, tenant).await
    }

    async fn list_capability_capsules_in_scope(
        &self,
        tenant: &str,
        project: Option<&str>,
        repo: Option<&str>,
        module: Option<&str>,
        capsule_type: Option<&str>,
        status: Option<&str>,
        source_agent: Option<&str>,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<(Vec<CapabilityCapsuleRecord>, bool), StorageError> {
        Store::list_capability_capsules_in_scope(
            self,
            tenant,
            project,
            repo,
            module,
            capsule_type,
            status,
            source_agent,
            cursor,
            limit,
        )
        .await
    }
}

// ── In-memory backend: HashMap-based, for parity tests ─────────────
//
// Not gated by `#[cfg(test)]` because Phase 2 parity tests live in
// `tests/capsule_store_parity.rs` (an integration test crate) and
// can only see `pub` items from the lib. Document explicitly as a
// non-production backend — the storage model is purely in-process
// (no persistence) and feedback handling is simplified compared to
// the Lance path.

/// In-memory `CapsuleStore` implementation. **Test/dev only** — no
/// persistence, no concurrency hardening beyond a single `Mutex`,
/// no index optimization. Used by the Phase 2 parity test suite to
/// validate the trait surface without standing up a LanceDB on disk.
///
/// Storage model: `HashMap<capsule_id, Record>` plus a
/// `Vec<FeedbackEvent>` audit log. Matches the Lance backend's
/// observable behavior for the trait surface; doesn't try to
/// reproduce internal details like `_versions` manifests.
#[derive(Default)]
pub struct InMemoryCapsuleStore {
    inner: Mutex<InMemoryState>,
}

#[derive(Default)]
struct InMemoryState {
    /// Capsule rows keyed by `capability_capsule_id`.
    capsules: HashMap<String, CapabilityCapsuleRecord>,
    /// Audit trail. Insert-only.
    feedback: Vec<FeedbackEvent>,
}

impl InMemoryCapsuleStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut InMemoryState) -> R) -> R {
        let mut guard = self
            .inner
            .lock()
            .expect("in-memory capsule store mutex poisoned");
        f(&mut guard)
    }
}

#[async_trait]
impl CapsuleStore for InMemoryCapsuleStore {
    async fn insert_capability_capsule(
        &self,
        memory: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        self.with_state(|s| {
            s.capsules
                .insert(memory.capability_capsule_id.clone(), memory.clone());
            Ok(memory)
        })
    }

    async fn insert_capability_capsules(
        &self,
        memories: &[CapabilityCapsuleRecord],
    ) -> Result<(), StorageError> {
        self.with_state(|s| {
            for m in memories {
                s.capsules
                    .insert(m.capability_capsule_id.clone(), m.clone());
            }
            Ok(())
        })
    }

    async fn get_capability_capsule(
        &self,
        capability_capsule_id: String,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        Ok(self.with_state(|s| s.capsules.get(&capability_capsule_id).cloned()))
    }

    async fn get_capability_capsule_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        Ok(self.with_state(|s| {
            s.capsules
                .get(capability_capsule_id)
                .filter(|r| r.tenant == tenant)
                .cloned()
        }))
    }

    async fn get_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        Ok(self.with_state(|s| {
            s.capsules
                .get(capability_capsule_id)
                .filter(|r| {
                    r.tenant == tenant && r.status == CapabilityCapsuleStatus::PendingConfirmation
                })
                .cloned()
        }))
    }

    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        Ok(self.with_state(|s| {
            s.capsules
                .values()
                .find(|r| {
                    if r.tenant != tenant {
                        return false;
                    }
                    if matches!(
                        r.status,
                        CapabilityCapsuleStatus::Rejected | CapabilityCapsuleStatus::Archived
                    ) {
                        return false;
                    }
                    let key_match = match (idempotency_key, &r.idempotency_key) {
                        (Some(k), Some(rk)) if !k.is_empty() => k == rk,
                        _ => false,
                    };
                    key_match || r.content_hash == content_hash
                })
                .cloned()
        }))
    }

    async fn list_capability_capsules_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        Ok(self.with_state(|s| {
            s.capsules
                .values()
                .filter(|r| {
                    r.tenant == tenant
                        && !matches!(
                            r.status,
                            CapabilityCapsuleStatus::Rejected | CapabilityCapsuleStatus::Archived
                        )
                })
                .cloned()
                .collect()
        }))
    }

    async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        Ok(self.with_state(|s| {
            s.capsules
                .values()
                .filter(|r| {
                    r.tenant == tenant && r.status == CapabilityCapsuleStatus::PendingConfirmation
                })
                .cloned()
                .collect()
        }))
    }

    async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.with_state(|s| {
            ids.iter()
                .filter_map(|id| s.capsules.get(*id).filter(|r| r.tenant == tenant).cloned())
                .collect()
        }))
    }

    async fn accept_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        self.with_state(|s| {
            let r = s
                .capsules
                .get_mut(capability_capsule_id)
                .filter(|r| r.tenant == tenant)
                .ok_or(StorageError::InvalidData("memory not found"))?;
            r.status = CapabilityCapsuleStatus::Active;
            r.updated_at = crate::storage::current_timestamp();
            Ok(r.clone())
        })
    }

    async fn reject_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        self.with_state(|s| {
            let r = s
                .capsules
                .get_mut(capability_capsule_id)
                .filter(|r| r.tenant == tenant)
                .ok_or(StorageError::InvalidData("memory not found"))?;
            r.status = CapabilityCapsuleStatus::Rejected;
            r.updated_at = crate::storage::current_timestamp();
            Ok(r.clone())
        })
    }

    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        self.with_state(|s| {
            // Two-op semantics: reject the original, insert the
            // successor. Same non-atomic shape as the Lance backend.
            let original = s
                .capsules
                .get_mut(original_memory_id)
                .filter(|r| r.tenant == tenant)
                .ok_or(StorageError::InvalidData("memory not found"))?;
            original.status = CapabilityCapsuleStatus::Rejected;
            original.updated_at = crate::storage::current_timestamp();
            s.capsules
                .insert(successor.capability_capsule_id.clone(), successor.clone());
            Ok(successor)
        })
    }

    async fn apply_feedback(
        &self,
        memory: &CapabilityCapsuleRecord,
        feedback: FeedbackEvent,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        // String→kind parsing lives on the domain enum (since the
        // Phase 2 side-findings fix); both backends share it.
        let kind =
            crate::domain::capability_capsule::FeedbackKind::from_db_str(&feedback.feedback_kind)
                .ok_or(StorageError::InvalidData("invalid feedback kind"))?;

        self.with_state(|s| {
            // Always log the event (audit), even if the row is missing.
            s.feedback.push(feedback.clone());
            let r = s
                .capsules
                .get_mut(&memory.capability_capsule_id)
                .ok_or(StorageError::InvalidData("memory not found"))?;
            r.confidence = (r.confidence + kind.confidence_delta()).clamp(0.0, 1.0);
            r.decay_score = (r.decay_score + kind.decay_delta()).clamp(0.0, 1.0);
            if let Some(s_after) = kind.status_after() {
                r.status = s_after;
            }
            if kind.marks_validated() {
                r.last_validated_at = Some(feedback.created_at.clone());
            }
            r.updated_at = feedback.created_at.clone();
            Ok(r.clone())
        })
    }

    async fn delete_capability_capsule_hard(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        self.with_state(|s| {
            let exists = s
                .capsules
                .get(capability_capsule_id)
                .filter(|r| r.tenant == tenant)
                .is_some();
            if !exists {
                return Err(StorageError::InvalidData("memory not found"));
            }
            s.capsules.remove(capability_capsule_id);
            // Trait contract: cascade satellite tables. The InMemory
            // backend only stores capsules + feedback events; drop
            // matching feedback rows so `feedback_summary` for the
            // deleted id returns the default after the cascade.
            // (Other satellites — embedding_jobs / embeddings /
            // graph_edges — aren't modeled in this dev/test backend.)
            s.feedback
                .retain(|ev| ev.capability_capsule_id != capability_capsule_id);
            Ok(())
        })
    }

    async fn feedback_summary(
        &self,
        capability_capsule_id: &str,
    ) -> Result<FeedbackSummary, StorageError> {
        Ok(self.with_state(|s| {
            let mut summary = FeedbackSummary::default();
            for ev in &s.feedback {
                if ev.capability_capsule_id != capability_capsule_id {
                    continue;
                }
                summary.total += 1;
                match ev.feedback_kind.as_str() {
                    "useful" => summary.useful += 1,
                    "outdated" => summary.outdated += 1,
                    "incorrect" => summary.incorrect += 1,
                    "applies_here" => summary.applies_here += 1,
                    "does_not_apply_here" => summary.does_not_apply_here += 1,
                    "auto_promoted" => summary.auto_promoted += 1,
                    _ => {}
                }
            }
            summary
        }))
    }

    async fn list_wings(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        Ok(self.with_state(|s| {
            let mut wings: Vec<String> = s
                .capsules
                .values()
                .filter(|r| r.tenant == tenant)
                .filter_map(|r| r.project.clone())
                .collect();
            wings.sort();
            wings.dedup();
            wings
        }))
    }

    async fn capsule_stats(&self, tenant: &str) -> Result<CapsuleStats, StorageError> {
        Ok(self.with_state(|s| {
            let mut stats = CapsuleStats::default();
            for r in s.capsules.values().filter(|r| r.tenant == tenant) {
                stats.total += 1;
                match r.status {
                    CapabilityCapsuleStatus::PendingConfirmation => stats.pending_confirmation += 1,
                    CapabilityCapsuleStatus::Provisional => stats.provisional += 1,
                    CapabilityCapsuleStatus::Active => stats.active += 1,
                    CapabilityCapsuleStatus::Archived => stats.archived += 1,
                    CapabilityCapsuleStatus::Rejected => stats.rejected += 1,
                }
            }
            stats
        }))
    }

    async fn get_taxonomy(&self, tenant: &str) -> Result<Vec<(String, Vec<String>)>, StorageError> {
        Ok(self.with_state(|s| {
            use std::collections::BTreeMap;
            let mut map: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
            for r in s.capsules.values().filter(|r| r.tenant == tenant) {
                if let Some(p) = &r.project {
                    let entry = map.entry(p.clone()).or_default();
                    if let Some(repo) = &r.repo {
                        entry.insert(repo.clone());
                    }
                }
            }
            map.into_iter()
                .map(|(p, repos)| (p, repos.into_iter().collect()))
                .collect()
        }))
    }

    async fn list_capability_capsules_in_scope(
        &self,
        tenant: &str,
        project: Option<&str>,
        repo: Option<&str>,
        module: Option<&str>,
        capsule_type: Option<&str>,
        status: Option<&str>,
        source_agent: Option<&str>,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<(Vec<CapabilityCapsuleRecord>, bool), StorageError> {
        let lim = limit.clamp(1, 200);
        Ok(self.with_state(|s| {
            let mut rows: Vec<CapabilityCapsuleRecord> = s
                .capsules
                .values()
                .filter(|r| r.tenant == tenant)
                .filter(|r| project.is_none_or(|v| r.project.as_deref() == Some(v)))
                .filter(|r| repo.is_none_or(|v| r.repo.as_deref() == Some(v)))
                .filter(|r| module.is_none_or(|v| r.module.as_deref() == Some(v)))
                .filter(|r| {
                    capsule_type.is_none_or(|v| {
                        serde_json::to_value(&r.capability_capsule_type)
                            .ok()
                            .and_then(|s| s.as_str().map(str::to_owned))
                            .as_deref()
                            == Some(v)
                    })
                })
                .filter(|r| {
                    status.is_none_or(|v| {
                        serde_json::to_value(&r.status)
                            .ok()
                            .and_then(|s| s.as_str().map(str::to_owned))
                            .as_deref()
                            == Some(v)
                    })
                })
                .filter(|r| source_agent.is_none_or(|v| r.source_agent == v))
                .filter(|r| match cursor {
                    None => true,
                    Some((cur_updated, cur_id)) => {
                        r.updated_at.as_str() < cur_updated
                            || (r.updated_at.as_str() == cur_updated
                                && r.capability_capsule_id.as_str() > cur_id)
                    }
                })
                .cloned()
                .collect();
            rows.sort_by(|a, b| {
                b.updated_at
                    .cmp(&a.updated_at)
                    .then_with(|| a.capability_capsule_id.cmp(&b.capability_capsule_id))
            });
            let has_more = rows.len() > lim;
            rows.truncate(lim);
            (rows, has_more)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{
        CapabilityCapsuleType, FeedbackKind, Scope, Visibility,
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
            source_agent: "test".into(),
            created_at: "00000000000000000000".into(),
            updated_at: "00000000000000000000".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn in_memory_insert_and_get_round_trip() {
        let s = InMemoryCapsuleStore::new();
        s.insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::Active))
            .await
            .unwrap();
        let r = s.get_capability_capsule_for_tenant("t", "a").await.unwrap();
        assert!(r.is_some());
        assert_eq!(r.unwrap().capability_capsule_id, "a");
    }

    #[tokio::test]
    async fn in_memory_accept_pending_transitions_status() {
        let s = InMemoryCapsuleStore::new();
        s.insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::PendingConfirmation))
            .await
            .unwrap();
        let r = s.accept_pending("t", "a").await.unwrap();
        assert_eq!(r.status, CapabilityCapsuleStatus::Active);
    }

    #[tokio::test]
    async fn in_memory_apply_feedback_useful_bumps_confidence() {
        let s = InMemoryCapsuleStore::new();
        let original = fixture("a", CapabilityCapsuleStatus::Active);
        let baseline = original.confidence;
        s.insert_capability_capsule(original.clone()).await.unwrap();
        let ev = FeedbackEvent {
            feedback_id: "fb_1".into(),
            capability_capsule_id: "a".into(),
            feedback_kind: FeedbackKind::Useful.as_str().to_string(),
            created_at: "00000000000000000001".into(),
            note: None,
        };
        let updated = s.apply_feedback(&original, ev).await.unwrap();
        assert!(updated.confidence > baseline, "confidence should rise");
    }
}
