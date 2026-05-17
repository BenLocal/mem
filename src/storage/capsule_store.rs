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
//! - **Service is not migrated** (per the Phase 2 scope-A decision):
//!   `capability_capsule_service` still holds `Arc<Store>`. The
//!   trait exists so Phase 3 can do the service-layer migration
//!   incrementally; `Store: CapsuleStore` is the bridge.
//! - **Error type**: shared [`StorageError`]. The doc §3.3 mentions
//!   future `BackendError(Box<...>)` transparency; today's
//!   `StorageError` variants suffice.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, FeedbackSummary,
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
    async fn apply_feedback(
        &self,
        memory: &CapabilityCapsuleRecord,
        feedback: FeedbackEvent,
    ) -> Result<CapabilityCapsuleRecord, StorageError>;

    /// Hard delete (irreversible). Cascade-deletes from satellite
    /// tables is a backend-implementation concern; see the
    /// LANCE-SPECIFIC cascade note in `capability_capsule_service`.
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
        Store::get_capability_capsule(self, capability_capsule_id).await
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
        Store::feedback_summary(self, capability_capsule_id).await
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
            Ok(())
        })
    }

    async fn feedback_summary(
        &self,
        capability_capsule_id: &str,
    ) -> Result<FeedbackSummary, StorageError> {
        Ok(self.with_state(|s| {
            let mut total = 0u64;
            let mut useful = 0u64;
            let mut outdated = 0u64;
            let mut incorrect = 0u64;
            let mut applies_here = 0u64;
            let mut does_not_apply_here = 0u64;
            for ev in &s.feedback {
                if ev.capability_capsule_id != capability_capsule_id {
                    continue;
                }
                total += 1;
                match ev.feedback_kind.as_str() {
                    "useful" => useful += 1,
                    "outdated" => outdated += 1,
                    "incorrect" => incorrect += 1,
                    "applies_here" => applies_here += 1,
                    "does_not_apply_here" => does_not_apply_here += 1,
                    _ => {}
                }
            }
            FeedbackSummary {
                total,
                useful,
                outdated,
                incorrect,
                applies_here,
                does_not_apply_here,
            }
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
