//! Phase 4 Postgres spike (backend-coupling.md §6.5):
//! `PostgresCapsuleStore` — a sqlx-backed implementation of the
//! [`super::CapsuleStore`] trait. Behind the `postgres` cargo
//! feature so the default build doesn't pull sqlx.
//!
//! **Status: scaffold, untested**. This module compiles but has
//! never been run against a real Postgres instance — the Phase 4
//! validation envisioned in doc §6.5 ("跑一个集成测试 suite 看痛点")
//! needs Docker + testcontainers infra that this code doesn't
//! ship with. The implementation pain points encountered while
//! writing the scaffold are recorded in §6.5 of the doc.
//!
//! Connect with:
//! ```ignore
//! use sqlx::postgres::PgPoolOptions;
//! let pool = PgPoolOptions::new()
//!     .max_connections(8)
//!     .connect("postgres://localhost/mem").await?;
//! let backend = PostgresCapsuleStore::new(pool);
//! ```
//!
//! Schema is in `migrations/postgres/0001_capsule_store.sql` —
//! apply it before instantiating the backend.

use async_trait::async_trait;
use sqlx::{PgPool, Row};

use super::CapsuleStore;
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, FeedbackKind,
    FeedbackSummary, Scope, Visibility,
};
use crate::storage::types::{FeedbackEvent, StorageError};

/// Postgres-backed [`CapsuleStore`] (Phase 4 spike). Holds a sqlx
/// connection pool; cheap to clone (pool is `Arc`'d internally).
#[derive(Clone)]
pub struct PostgresCapsuleStore {
    pool: PgPool,
}

impl PostgresCapsuleStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Map sqlx errors to `StorageError`. sqlx has rich error variants
/// (decode / database / pool / column-not-found) — Phase 4 spike
/// flattens to `InvalidInput` with a string. Future cleanup can
/// add a `StorageError::Backend(Box<dyn Error>)` variant per doc
/// §3.3 if richer surfacing is needed.
fn sqlx_err(e: sqlx::Error) -> StorageError {
    StorageError::InvalidInput(format!("postgres: {e}"))
}

/// Parse a `TEXT[]` column from a row. sqlx maps PG arrays to
/// `Vec<String>` natively when the `postgres` feature is on.
fn try_get_string_list(
    row: &sqlx::postgres::PgRow,
    name: &str,
) -> Result<Vec<String>, StorageError> {
    row.try_get::<Vec<String>, _>(name).map_err(sqlx_err)
}

/// Parse a `TEXT` enum column → domain enum. Mirrors the lance
/// backend's `enum_from_str` helper but uses the domain enums'
/// `from_db_str` methods (added in the Phase 2 side-findings
/// cleanup) where available, and serde for the rest.
fn parse_status(s: &str) -> Result<CapabilityCapsuleStatus, StorageError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| StorageError::InvalidData("unknown capsule status"))
}

fn parse_type(s: &str) -> Result<CapabilityCapsuleType, StorageError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| StorageError::InvalidData("unknown capsule type"))
}

fn parse_scope(s: &str) -> Result<Scope, StorageError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| StorageError::InvalidData("unknown capsule scope"))
}

fn parse_visibility(s: &str) -> Result<Visibility, StorageError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| StorageError::InvalidData("unknown capsule visibility"))
}

fn enum_to_str<T: serde::Serialize>(v: &T) -> Result<String, StorageError> {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or(StorageError::InvalidData(
            "enum did not serialize as string",
        ))
}

/// Project the 27-column row into a `CapabilityCapsuleRecord`.
/// Column order matches `select_columns()` below.
fn row_to_record(row: &sqlx::postgres::PgRow) -> Result<CapabilityCapsuleRecord, StorageError> {
    Ok(CapabilityCapsuleRecord {
        capability_capsule_id: row.try_get("capability_capsule_id").map_err(sqlx_err)?,
        tenant: row.try_get("tenant").map_err(sqlx_err)?,
        capability_capsule_type: parse_type(
            &row.try_get::<String, _>("capability_capsule_type")
                .map_err(sqlx_err)?,
        )?,
        status: parse_status(&row.try_get::<String, _>("status").map_err(sqlx_err)?)?,
        scope: parse_scope(&row.try_get::<String, _>("scope").map_err(sqlx_err)?)?,
        visibility: parse_visibility(&row.try_get::<String, _>("visibility").map_err(sqlx_err)?)?,
        version: row.try_get("version").map_err(sqlx_err)?,
        summary: row.try_get("summary").map_err(sqlx_err)?,
        content: row.try_get("content").map_err(sqlx_err)?,
        evidence: try_get_string_list(row, "evidence")?,
        code_refs: try_get_string_list(row, "code_refs")?,
        project: row.try_get("project").map_err(sqlx_err)?,
        repo: row.try_get("repo").map_err(sqlx_err)?,
        module: row.try_get("module").map_err(sqlx_err)?,
        task_type: row.try_get("task_type").map_err(sqlx_err)?,
        tags: try_get_string_list(row, "tags")?,
        topics: try_get_string_list(row, "topics")?,
        confidence: row.try_get("confidence").map_err(sqlx_err)?,
        decay_score: row.try_get("decay_score").map_err(sqlx_err)?,
        content_hash: row.try_get("content_hash").map_err(sqlx_err)?,
        idempotency_key: row.try_get("idempotency_key").map_err(sqlx_err)?,
        session_id: row.try_get("session_id").map_err(sqlx_err)?,
        supersedes_capability_capsule_id: row
            .try_get("supersedes_capability_capsule_id")
            .map_err(sqlx_err)?,
        source_agent: row.try_get("source_agent").map_err(sqlx_err)?,
        created_at: row.try_get("created_at").map_err(sqlx_err)?,
        updated_at: row.try_get("updated_at").map_err(sqlx_err)?,
        last_validated_at: row.try_get("last_validated_at").map_err(sqlx_err)?,
    })
}

const SELECT_COLUMNS: &str = "capability_capsule_id, tenant, capability_capsule_type, status, \
    scope, visibility, version, summary, content, evidence, code_refs, project, repo, module, \
    task_type, tags, topics, confidence, decay_score, content_hash, idempotency_key, session_id, \
    supersedes_capability_capsule_id, source_agent, created_at, updated_at, last_validated_at";

#[async_trait]
impl CapsuleStore for PostgresCapsuleStore {
    async fn insert_capability_capsule(
        &self,
        memory: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let sql = "INSERT INTO capability_capsules (\
            capability_capsule_id, tenant, capability_capsule_type, status, scope, visibility, \
            version, summary, content, evidence, code_refs, project, repo, module, task_type, \
            tags, topics, confidence, decay_score, content_hash, idempotency_key, session_id, \
            supersedes_capability_capsule_id, source_agent, created_at, updated_at, \
            last_validated_at) \
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
                    $18, $19, $20, $21, $22, $23, $24, $25, $26, $27)";
        sqlx::query(sql)
            .bind(&memory.capability_capsule_id)
            .bind(&memory.tenant)
            .bind(enum_to_str(&memory.capability_capsule_type)?)
            .bind(enum_to_str(&memory.status)?)
            .bind(enum_to_str(&memory.scope)?)
            .bind(enum_to_str(&memory.visibility)?)
            .bind(memory.version)
            .bind(&memory.summary)
            .bind(&memory.content)
            .bind(&memory.evidence)
            .bind(&memory.code_refs)
            .bind(&memory.project)
            .bind(&memory.repo)
            .bind(&memory.module)
            .bind(&memory.task_type)
            .bind(&memory.tags)
            .bind(&memory.topics)
            .bind(memory.confidence)
            .bind(memory.decay_score)
            .bind(&memory.content_hash)
            .bind(&memory.idempotency_key)
            .bind(&memory.session_id)
            .bind(&memory.supersedes_capability_capsule_id)
            .bind(&memory.source_agent)
            .bind(&memory.created_at)
            .bind(&memory.updated_at)
            .bind(&memory.last_validated_at)
            .execute(&self.pool)
            .await
            .map_err(sqlx_err)?;
        Ok(memory)
    }

    async fn insert_capability_capsules(
        &self,
        memories: &[CapabilityCapsuleRecord],
    ) -> Result<(), StorageError> {
        // sqlx 0.8 doesn't ship a generic multi-row INSERT helper —
        // either build the VALUES list dynamically (one binding per
        // row × 27 columns) or just call `insert_capability_capsule`
        // in a loop inside a transaction. Phase 4 spike uses the
        // loop form (simpler, fewer binding gotchas); production
        // impl should switch to `COPY FROM STDIN` for batch ingest.
        // See §6.5 doc pain point #4.
        if memories.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(sqlx_err)?;
        for m in memories {
            sqlx::query(
                "INSERT INTO capability_capsules (\
                    capability_capsule_id, tenant, capability_capsule_type, status, scope, \
                    visibility, version, summary, content, evidence, code_refs, project, repo, \
                    module, task_type, tags, topics, confidence, decay_score, content_hash, \
                    idempotency_key, session_id, supersedes_capability_capsule_id, source_agent, \
                    created_at, updated_at, last_validated_at) \
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                            $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27)",
            )
            .bind(&m.capability_capsule_id)
            .bind(&m.tenant)
            .bind(enum_to_str(&m.capability_capsule_type)?)
            .bind(enum_to_str(&m.status)?)
            .bind(enum_to_str(&m.scope)?)
            .bind(enum_to_str(&m.visibility)?)
            .bind(m.version)
            .bind(&m.summary)
            .bind(&m.content)
            .bind(&m.evidence)
            .bind(&m.code_refs)
            .bind(&m.project)
            .bind(&m.repo)
            .bind(&m.module)
            .bind(&m.task_type)
            .bind(&m.tags)
            .bind(&m.topics)
            .bind(m.confidence)
            .bind(m.decay_score)
            .bind(&m.content_hash)
            .bind(&m.idempotency_key)
            .bind(&m.session_id)
            .bind(&m.supersedes_capability_capsule_id)
            .bind(&m.source_agent)
            .bind(&m.created_at)
            .bind(&m.updated_at)
            .bind(&m.last_validated_at)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(())
    }

    async fn get_capability_capsule(
        &self,
        capability_capsule_id: String,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules \
             WHERE capability_capsule_id = $1 LIMIT 1"
        );
        let row = sqlx::query(&sql)
            .bind(&capability_capsule_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_err)?;
        row.as_ref().map(row_to_record).transpose()
    }

    async fn get_capability_capsule_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules \
             WHERE tenant = $1 AND capability_capsule_id = $2 LIMIT 1"
        );
        let row = sqlx::query(&sql)
            .bind(tenant)
            .bind(capability_capsule_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_err)?;
        row.as_ref().map(row_to_record).transpose()
    }

    async fn get_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules \
             WHERE tenant = $1 AND capability_capsule_id = $2 \
                 AND status = 'pending_confirmation' \
             LIMIT 1"
        );
        let row = sqlx::query(&sql)
            .bind(tenant)
            .bind(capability_capsule_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_err)?;
        row.as_ref().map(row_to_record).transpose()
    }

    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        // Compose: match either idempotency_key (when supplied) OR
        // content_hash, excluding rejected/archived rows.
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules \
             WHERE tenant = $1 \
               AND status NOT IN ('rejected', 'archived') \
               AND ((idempotency_key IS NOT NULL AND idempotency_key = $2) \
                    OR content_hash = $3) \
             LIMIT 1"
        );
        let row = sqlx::query(&sql)
            .bind(tenant)
            .bind(idempotency_key.as_deref())
            .bind(content_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_err)?;
        row.as_ref().map(row_to_record).transpose()
    }

    async fn list_capability_capsules_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        // Live status, exclude diary type — same filter the Lance
        // backend's `search_candidates` uses.
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules \
             WHERE tenant = $1 \
               AND status NOT IN ('rejected', 'archived') \
               AND capability_capsule_type != 'diary' \
             ORDER BY updated_at DESC, version DESC, capability_capsule_id ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(tenant)
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_err)?;
        rows.iter().map(row_to_record).collect()
    }

    async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules \
             WHERE tenant = $1 AND status = 'pending_confirmation' \
             ORDER BY created_at DESC"
        );
        let rows = sqlx::query(&sql)
            .bind(tenant)
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_err)?;
        rows.iter().map(row_to_record).collect()
    }

    async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Use ANY($2::text[]) — sqlx binds &[&str] as Postgres
        // TEXT[] when sent through `.bind`. Cleaner than the
        // `IN (?, ?, ...)` placeholder fan-out the Lance backend
        // needs because duckdb-rs doesn't support array binding.
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules \
             WHERE tenant = $1 AND capability_capsule_id = ANY($2)"
        );
        let owned_ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        let rows = sqlx::query(&sql)
            .bind(tenant)
            .bind(&owned_ids)
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_err)?;
        rows.iter().map(row_to_record).collect()
    }

    async fn accept_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let now = crate::storage::current_timestamp();
        let rows_affected = sqlx::query(
            "UPDATE capability_capsules SET status = 'active', updated_at = $1 \
             WHERE tenant = $2 AND capability_capsule_id = $3",
        )
        .bind(&now)
        .bind(tenant)
        .bind(capability_capsule_id)
        .execute(&self.pool)
        .await
        .map_err(sqlx_err)?
        .rows_affected();
        if rows_affected == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }
        self.get_capability_capsule_for_tenant(tenant, capability_capsule_id)
            .await?
            .ok_or(StorageError::InvalidData(
                "memory missing after status update",
            ))
    }

    async fn reject_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let now = crate::storage::current_timestamp();
        let rows_affected = sqlx::query(
            "UPDATE capability_capsules SET status = 'rejected', updated_at = $1 \
             WHERE tenant = $2 AND capability_capsule_id = $3",
        )
        .bind(&now)
        .bind(tenant)
        .bind(capability_capsule_id)
        .execute(&self.pool)
        .await
        .map_err(sqlx_err)?
        .rows_affected();
        if rows_affected == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }
        self.get_capability_capsule_for_tenant(tenant, capability_capsule_id)
            .await?
            .ok_or(StorageError::InvalidData(
                "memory missing after status update",
            ))
    }

    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        // ATOMIC via BEGIN/COMMIT — Postgres has real transactions
        // so the trait's "two-op semantics" Lance limitation
        // disappears here. Trait contract spec'd in
        // `CapsuleStore::replace_pending_with_successor` doc:
        // original MUST end up `Rejected` (load-bearing for
        // service-layer chain walks).
        let now = crate::storage::current_timestamp();
        let mut tx = self.pool.begin().await.map_err(sqlx_err)?;

        let rows_affected = sqlx::query(
            "UPDATE capability_capsules SET status = 'rejected', updated_at = $1 \
             WHERE tenant = $2 AND capability_capsule_id = $3",
        )
        .bind(&now)
        .bind(tenant)
        .bind(original_memory_id)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?
        .rows_affected();
        if rows_affected == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }

        sqlx::query(
            "INSERT INTO capability_capsules (\
                capability_capsule_id, tenant, capability_capsule_type, status, scope, \
                visibility, version, summary, content, evidence, code_refs, project, repo, \
                module, task_type, tags, topics, confidence, decay_score, content_hash, \
                idempotency_key, session_id, supersedes_capability_capsule_id, source_agent, \
                created_at, updated_at, last_validated_at) \
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                        $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27)",
        )
        .bind(&successor.capability_capsule_id)
        .bind(&successor.tenant)
        .bind(enum_to_str(&successor.capability_capsule_type)?)
        .bind(enum_to_str(&successor.status)?)
        .bind(enum_to_str(&successor.scope)?)
        .bind(enum_to_str(&successor.visibility)?)
        .bind(successor.version)
        .bind(&successor.summary)
        .bind(&successor.content)
        .bind(&successor.evidence)
        .bind(&successor.code_refs)
        .bind(&successor.project)
        .bind(&successor.repo)
        .bind(&successor.module)
        .bind(&successor.task_type)
        .bind(&successor.tags)
        .bind(&successor.topics)
        .bind(successor.confidence)
        .bind(successor.decay_score)
        .bind(&successor.content_hash)
        .bind(&successor.idempotency_key)
        .bind(&successor.session_id)
        .bind(&successor.supersedes_capability_capsule_id)
        .bind(&successor.source_agent)
        .bind(&successor.created_at)
        .bind(&successor.updated_at)
        .bind(&successor.last_validated_at)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;
        tx.commit().await.map_err(sqlx_err)?;
        Ok(successor)
    }

    async fn apply_feedback(
        &self,
        memory: &CapabilityCapsuleRecord,
        feedback: FeedbackEvent,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let kind = FeedbackKind::from_db_str(&feedback.feedback_kind)
            .ok_or(StorageError::InvalidData("invalid feedback kind"))?;

        // ATOMIC via BEGIN/COMMIT — same Postgres-has-transactions
        // advantage as `replace_pending_with_successor`. Lance
        // backend's "audit row + parent update could partial-commit"
        // hazard doesn't apply here.
        let mut tx = self.pool.begin().await.map_err(sqlx_err)?;

        // 1. Insert audit row.
        sqlx::query(
            "INSERT INTO feedback_events (feedback_id, capability_capsule_id, \
                feedback_kind, created_at, note) \
                VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&feedback.feedback_id)
        .bind(&feedback.capability_capsule_id)
        .bind(&feedback.feedback_kind)
        .bind(&feedback.created_at)
        .bind(feedback.note.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;

        // 2. Update parent capsule. Build the SET list dynamically
        // based on which deltas are non-zero — keeps the SQL
        // straightforward without dynamic-binding tricks.
        let new_confidence = (memory.confidence + kind.confidence_delta()).clamp(0.0, 1.0);
        let new_decay = (memory.decay_score + kind.decay_delta()).clamp(0.0, 1.0);
        let new_status = kind.status_after();
        let new_validated_at = if kind.marks_validated() {
            Some(feedback.created_at.clone())
        } else {
            None
        };

        // Always-set: confidence, decay_score, updated_at.
        // Conditional: status (if status_after returns Some),
        // last_validated_at (if marks_validated).
        if let (Some(status_after), Some(validated_at)) = (&new_status, &new_validated_at) {
            sqlx::query(
                "UPDATE capability_capsules SET confidence = $1, decay_score = $2, \
                 updated_at = $3, status = $4, last_validated_at = $5 \
                 WHERE capability_capsule_id = $6",
            )
            .bind(new_confidence)
            .bind(new_decay)
            .bind(&feedback.created_at)
            .bind(enum_to_str(status_after)?)
            .bind(validated_at)
            .bind(&memory.capability_capsule_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        } else if let Some(status_after) = &new_status {
            sqlx::query(
                "UPDATE capability_capsules SET confidence = $1, decay_score = $2, \
                 updated_at = $3, status = $4 \
                 WHERE capability_capsule_id = $5",
            )
            .bind(new_confidence)
            .bind(new_decay)
            .bind(&feedback.created_at)
            .bind(enum_to_str(status_after)?)
            .bind(&memory.capability_capsule_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        } else if let Some(validated_at) = &new_validated_at {
            sqlx::query(
                "UPDATE capability_capsules SET confidence = $1, decay_score = $2, \
                 updated_at = $3, last_validated_at = $4 \
                 WHERE capability_capsule_id = $5",
            )
            .bind(new_confidence)
            .bind(new_decay)
            .bind(&feedback.created_at)
            .bind(validated_at)
            .bind(&memory.capability_capsule_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        } else {
            sqlx::query(
                "UPDATE capability_capsules SET confidence = $1, decay_score = $2, \
                 updated_at = $3 \
                 WHERE capability_capsule_id = $4",
            )
            .bind(new_confidence)
            .bind(new_decay)
            .bind(&feedback.created_at)
            .bind(&memory.capability_capsule_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        }
        tx.commit().await.map_err(sqlx_err)?;

        // Return the updated row by re-fetching. The Lance backend
        // returns an in-Rust-mutated copy; we go round-trip for
        // consistency — Postgres triggers / defaults could have
        // changed values, and we'd rather surface that.
        self.get_capability_capsule(memory.capability_capsule_id.clone())
            .await?
            .ok_or(StorageError::InvalidData(
                "memory missing after feedback apply",
            ))
    }

    async fn delete_capability_capsule_hard(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        let rows_affected = sqlx::query(
            "DELETE FROM capability_capsules \
             WHERE tenant = $1 AND capability_capsule_id = $2",
        )
        .bind(tenant)
        .bind(capability_capsule_id)
        .execute(&self.pool)
        .await
        .map_err(sqlx_err)?
        .rows_affected();
        if rows_affected == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }
        Ok(())
    }

    async fn feedback_summary(
        &self,
        capability_capsule_id: &str,
    ) -> Result<FeedbackSummary, StorageError> {
        // One round-trip with GROUP BY — cleaner than fetching all
        // rows and counting in Rust like the Lance impl has to.
        let rows = sqlx::query(
            "SELECT feedback_kind, COUNT(*) AS cnt FROM feedback_events \
             WHERE capability_capsule_id = $1 \
             GROUP BY feedback_kind",
        )
        .bind(capability_capsule_id)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_err)?;

        let mut summary = FeedbackSummary::default();
        for row in &rows {
            let kind: String = row.try_get("feedback_kind").map_err(sqlx_err)?;
            let cnt: i64 = row.try_get("cnt").map_err(sqlx_err)?;
            let cnt = u64::try_from(cnt).unwrap_or(0);
            summary.total += cnt;
            match kind.as_str() {
                "useful" => summary.useful += cnt,
                "outdated" => summary.outdated += cnt,
                "incorrect" => summary.incorrect += cnt,
                "applies_here" => summary.applies_here += cnt,
                "does_not_apply_here" => summary.does_not_apply_here += cnt,
                _ => {} // auto_promoted etc. don't have a count slot on FeedbackSummary
            }
        }
        Ok(summary)
    }
}
