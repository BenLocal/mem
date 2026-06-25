//! `CapsuleStore` for the ClickHouse backend.
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P1).** The SQL below is written against the
//! clickhouse-rs 0.15 API and the §6 DDL but has never executed.
//!
//! Model (clickhouse-backend.md §4a): `capability_capsules` is a
//! `ReplacingMergeTree(row_version)`. Every logical update — status
//! transitions, decay, supersede, feedback deltas — is a **versioned
//! re-insert** of the whole row with a fresh `row_version`. Reads take the
//! latest version per `(tenant, capability_capsule_id)` via `FINAL` (the
//! correctness-simple form; `argMax(...) GROUP BY pk` is the hot-path
//! optimisation noted in the doc). Optional columns are `String` with `''`
//! standing in for `None` (no `Nullable`).

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::backend::ClickHouseBackend;
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapsuleStats, FeedbackKind, FeedbackSummary,
};
use crate::storage::capsule_store::CapsuleStore;
use crate::storage::time::current_timestamp;
use crate::storage::types::{FeedbackEvent, StorageError};

// ── helpers ────────────────────────────────────────────────────────────

/// Map a clickhouse-rs error into the shared [`StorageError`].
pub(super) fn ch_err(e: clickhouse::error::Error) -> StorageError {
    StorageError::InvalidInput(format!("clickhouse: {e}"))
}

/// `''` ⇒ `None`, else `Some(s)` — the empty-string-as-absent convention
/// the §6 DDL uses (CH `String` columns, not `Nullable`).
pub(super) fn opt(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// A monotonically-increasing-ish `row_version` for ReplacingMergeTree.
/// Wall-clock ms; two writes inside the same ms collide (a known scaffold
/// caveat — see the pain inventory; a per-process `AtomicU64` would harden
/// it). Never run, so the collision window is theoretical here.
pub(super) fn now_version() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Serialize a snake_case serde enum to its string form for a CH column.
pub(super) fn enum_to_str<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|j| j.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Parse a CH enum string back to a serde enum (falls back to `Default`
/// on an unrecognised value — forward-compat with future variants).
pub(super) fn enum_from_str<T: serde::de::DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).unwrap_or_default()
}

/// Parse a `feedback_kind` string into the typed enum.
fn parse_feedback_kind(s: &str) -> Option<FeedbackKind> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
}

// ── row structs ────────────────────────────────────────────────────────

/// RowBinary mapping for `capability_capsules`. Field names + order match
/// the §6 DDL exactly (the clickhouse-rs `?fields` placeholder emits this
/// list). Enums are stored as their snake_case strings; optional domain
/// fields collapse `None` → `""`.
#[derive(Debug, Row, Serialize, Deserialize)]
pub(super) struct ChCapsuleRow {
    capability_capsule_id: String,
    tenant: String,
    capability_capsule_type: String,
    status: String,
    scope: String,
    visibility: String,
    version: i64,
    summary: String,
    content: String,
    evidence: Vec<String>,
    code_refs: Vec<String>,
    project: String,
    repo: String,
    module: String,
    task_type: String,
    tags: Vec<String>,
    topics: Vec<String>,
    confidence: f32,
    decay_score: f32,
    content_hash: String,
    idempotency_key: String,
    session_id: String,
    supersedes_capability_capsule_id: String,
    source_agent: String,
    created_at: String,
    updated_at: String,
    last_validated_at: String,
    last_used_at: String,
    last_recalled_at: String,
    expires_at: String,
    row_version: u64,
}

impl ChCapsuleRow {
    fn from_record(r: &CapabilityCapsuleRecord) -> Self {
        Self {
            capability_capsule_id: r.capability_capsule_id.clone(),
            tenant: r.tenant.clone(),
            capability_capsule_type: enum_to_str(&r.capability_capsule_type),
            status: enum_to_str(&r.status),
            scope: enum_to_str(&r.scope),
            visibility: enum_to_str(&r.visibility),
            version: r.version,
            summary: r.summary.clone(),
            content: r.content.clone(),
            evidence: r.evidence.clone(),
            code_refs: r.code_refs.clone(),
            project: r.project.clone().unwrap_or_default(),
            repo: r.repo.clone().unwrap_or_default(),
            module: r.module.clone().unwrap_or_default(),
            task_type: r.task_type.clone().unwrap_or_default(),
            tags: r.tags.clone(),
            topics: r.topics.clone(),
            confidence: r.confidence,
            decay_score: r.decay_score,
            content_hash: r.content_hash.clone(),
            idempotency_key: r.idempotency_key.clone().unwrap_or_default(),
            session_id: r.session_id.clone().unwrap_or_default(),
            supersedes_capability_capsule_id: r
                .supersedes_capability_capsule_id
                .clone()
                .unwrap_or_default(),
            source_agent: r.source_agent.clone(),
            created_at: r.created_at.clone(),
            updated_at: r.updated_at.clone(),
            last_validated_at: r.last_validated_at.clone().unwrap_or_default(),
            last_used_at: r.last_used_at.clone().unwrap_or_default(),
            last_recalled_at: r.last_recalled_at.clone().unwrap_or_default(),
            expires_at: r.expires_at.clone().unwrap_or_default(),
            row_version: now_version(),
        }
    }

    pub(super) fn into_record(self) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: self.capability_capsule_id,
            tenant: self.tenant,
            capability_capsule_type: enum_from_str(&self.capability_capsule_type),
            status: enum_from_str(&self.status),
            scope: enum_from_str(&self.scope),
            visibility: enum_from_str(&self.visibility),
            version: self.version,
            summary: self.summary,
            content: self.content,
            evidence: self.evidence,
            code_refs: self.code_refs,
            project: opt(self.project),
            repo: opt(self.repo),
            module: opt(self.module),
            task_type: opt(self.task_type),
            tags: self.tags,
            topics: self.topics,
            confidence: self.confidence,
            decay_score: self.decay_score,
            content_hash: self.content_hash,
            idempotency_key: opt(self.idempotency_key),
            session_id: opt(self.session_id),
            supersedes_capability_capsule_id: opt(self.supersedes_capability_capsule_id),
            source_agent: self.source_agent,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_validated_at: opt(self.last_validated_at),
            last_used_at: opt(self.last_used_at),
            last_recalled_at: opt(self.last_recalled_at),
            expires_at: opt(self.expires_at),
        }
    }
}

#[derive(Debug, Row, Serialize)]
struct ChFeedbackRow {
    feedback_id: String,
    capability_capsule_id: String,
    feedback_kind: String,
    created_at: String,
    note: String,
}

#[derive(Debug, Row, Deserialize)]
struct ChStatsRow {
    total: i64,
    pending_confirmation: i64,
    provisional: i64,
    active: i64,
    archived: i64,
    rejected: i64,
}

#[derive(Debug, Row, Deserialize)]
struct ChFeedbackSummaryRow {
    total: u64,
    useful: u64,
    outdated: u64,
    incorrect: u64,
    applies_here: u64,
    does_not_apply_here: u64,
    auto_promoted: u64,
}

#[derive(Debug, Row, Deserialize)]
struct ChStringRow {
    value: String,
}

#[derive(Debug, Row, Deserialize)]
struct ChProjectRepoRow {
    project: String,
    repo: String,
}

// ── ClickHouseBackend internals ────────────────────────────────────────

impl ClickHouseBackend {
    /// Append one capsule version (the universal write path — every CRUD /
    /// lifecycle op funnels through here as a versioned re-insert).
    async fn insert_capsule_rows(&self, rows: &[ChCapsuleRow]) -> Result<(), StorageError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut insert = self
            .client
            .insert::<ChCapsuleRow>("capability_capsules")
            .await
            .map_err(ch_err)?;
        for row in rows {
            insert.write(row).await.map_err(ch_err)?;
        }
        insert.end().await.map_err(ch_err)?;
        Ok(())
    }

    /// Latest version of `(tenant, id)` (FINAL = merge-on-read keep-latest),
    /// or `None`. Cross-tenant variant passes `tenant = None`.
    async fn latest(
        &self,
        tenant: Option<&str>,
        id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let rows = match tenant {
            Some(t) => self
                .client
                .query(
                    "SELECT ?fields FROM capability_capsules FINAL \
                     WHERE tenant = ? AND capability_capsule_id = ? LIMIT 1",
                )
                .bind(t)
                .bind(id)
                .fetch_all::<ChCapsuleRow>()
                .await
                .map_err(ch_err)?,
            None => self
                .client
                .query(
                    "SELECT ?fields FROM capability_capsules FINAL \
                     WHERE capability_capsule_id = ? LIMIT 1",
                )
                .bind(id)
                .fetch_all::<ChCapsuleRow>()
                .await
                .map_err(ch_err)?,
        };
        Ok(rows.into_iter().next().map(ChCapsuleRow::into_record))
    }
}

// ── CapsuleStore impl ──────────────────────────────────────────────────

#[async_trait]
impl CapsuleStore for ClickHouseBackend {
    async fn insert_capability_capsule(
        &self,
        memory: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        self.insert_capsule_rows(&[ChCapsuleRow::from_record(&memory)])
            .await?;
        Ok(memory)
    }

    async fn insert_capability_capsules(
        &self,
        memories: &[CapabilityCapsuleRecord],
    ) -> Result<(), StorageError> {
        let rows: Vec<ChCapsuleRow> = memories.iter().map(ChCapsuleRow::from_record).collect();
        self.insert_capsule_rows(&rows).await
    }

    async fn get_capability_capsule(
        &self,
        capability_capsule_id: String,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        self.latest(None, &capability_capsule_id).await
    }

    async fn get_capability_capsule_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        self.latest(Some(tenant), capability_capsule_id).await
    }

    async fn get_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        Ok(self
            .latest(Some(tenant), capability_capsule_id)
            .await?
            .filter(|r| r.status == CapabilityCapsuleStatus::PendingConfirmation))
    }

    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let key = idempotency_key.clone().unwrap_or_default();
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM capability_capsules FINAL \
                 WHERE tenant = ? \
                   AND status NOT IN ('rejected', 'archived') \
                   AND ((? != '' AND idempotency_key = ?) OR content_hash = ?) \
                 LIMIT 1",
            )
            .bind(tenant)
            .bind(&key)
            .bind(&key)
            .bind(content_hash)
            .fetch_all::<ChCapsuleRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().next().map(ChCapsuleRow::into_record))
    }

    async fn list_capability_capsules_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM capability_capsules FINAL \
                 WHERE tenant = ? \
                   AND status NOT IN ('rejected', 'archived') \
                   AND capability_capsule_type != 'diary'",
            )
            .bind(tenant)
            .fetch_all::<ChCapsuleRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().map(ChCapsuleRow::into_record).collect())
    }

    async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM capability_capsules FINAL \
                 WHERE tenant = ? AND status = 'pending_confirmation' \
                 ORDER BY created_at ASC",
            )
            .bind(tenant)
            .fetch_all::<ChCapsuleRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().map(ChCapsuleRow::into_record).collect())
    }

    async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let id_vec: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM capability_capsules FINAL \
                 WHERE tenant = ? AND capability_capsule_id IN ?",
            )
            .bind(tenant)
            .bind(id_vec)
            .fetch_all::<ChCapsuleRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().map(ChCapsuleRow::into_record).collect())
    }

    async fn set_capsule_status(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        status: CapabilityCapsuleStatus,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let mut rec = self
            .latest(Some(tenant), capability_capsule_id)
            .await?
            .ok_or(StorageError::NotFound("capsule not found"))?;
        rec.status = status;
        rec.updated_at = current_timestamp();
        self.insert_capsule_rows(&[ChCapsuleRow::from_record(&rec)])
            .await?;
        Ok(rec)
    }

    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        // Trait contract: original ends `Rejected`. Two non-atomic writes
        // (CH has no transactions — clickhouse-backend.md §4c / Pain #4):
        // mark old Rejected, then insert the successor.
        if let Some(mut original) = self.latest(Some(tenant), original_memory_id).await? {
            original.status = CapabilityCapsuleStatus::Rejected;
            original.updated_at = current_timestamp();
            self.insert_capsule_rows(&[ChCapsuleRow::from_record(&original)])
                .await?;
        }
        self.insert_capsule_rows(&[ChCapsuleRow::from_record(&successor)])
            .await?;
        Ok(successor)
    }

    async fn apply_feedback(
        &self,
        memory: &CapabilityCapsuleRecord,
        feedback: FeedbackEvent,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        // Non-atomic (Pain #4): audit insert, then a versioned capsule
        // re-insert applying the kind's deltas.
        let fb_row = ChFeedbackRow {
            feedback_id: feedback.feedback_id.clone(),
            capability_capsule_id: feedback.capability_capsule_id.clone(),
            feedback_kind: feedback.feedback_kind.clone(),
            created_at: feedback.created_at.clone(),
            note: feedback.note.clone().unwrap_or_default(),
        };
        let mut insert = self
            .client
            .insert::<ChFeedbackRow>("feedback_events")
            .await
            .map_err(ch_err)?;
        insert.write(&fb_row).await.map_err(ch_err)?;
        insert.end().await.map_err(ch_err)?;

        let mut rec = memory.clone();
        if let Some(kind) = parse_feedback_kind(&feedback.feedback_kind) {
            rec.confidence = (rec.confidence + kind.confidence_delta()).clamp(0.0, 1.0);
            rec.decay_score = (rec.decay_score + kind.decay_delta()).clamp(0.0, 1.0);
            if let Some(next) = kind.status_after() {
                rec.status = next;
            }
        }
        rec.updated_at = current_timestamp();
        self.insert_capsule_rows(&[ChCapsuleRow::from_record(&rec)])
            .await?;
        Ok(rec)
    }

    async fn delete_capability_capsule_hard(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        // Hard delete = async `ALTER … DELETE` mutation (rare admin path —
        // clickhouse-backend.md §4b). P1 cascades to the two in-scope
        // tables; the embedding / job satellites land with their tables in
        // P3 (see pain inventory).
        if self
            .latest(Some(tenant), capability_capsule_id)
            .await?
            .is_none()
        {
            return Err(StorageError::InvalidData("memory not found"));
        }
        self.client
            .query(
                "ALTER TABLE capability_capsules DELETE \
                 WHERE tenant = ? AND capability_capsule_id = ?",
            )
            .bind(tenant)
            .bind(capability_capsule_id)
            .execute()
            .await
            .map_err(ch_err)?;
        self.client
            .query("ALTER TABLE feedback_events DELETE WHERE capability_capsule_id = ?")
            .bind(capability_capsule_id)
            .execute()
            .await
            .map_err(ch_err)?;
        Ok(())
    }

    async fn feedback_summary(
        &self,
        capability_capsule_id: &str,
    ) -> Result<FeedbackSummary, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT \
                    count() AS total, \
                    countIf(feedback_kind = 'useful') AS useful, \
                    countIf(feedback_kind = 'outdated') AS outdated, \
                    countIf(feedback_kind = 'incorrect') AS incorrect, \
                    countIf(feedback_kind = 'applies_here') AS applies_here, \
                    countIf(feedback_kind = 'does_not_apply_here') AS does_not_apply_here, \
                    countIf(feedback_kind = 'auto_promoted') AS auto_promoted \
                 FROM feedback_events WHERE capability_capsule_id = ?",
            )
            .bind(capability_capsule_id)
            .fetch_all::<ChFeedbackSummaryRow>()
            .await
            .map_err(ch_err)?;
        let s = rows.into_iter().next();
        Ok(match s {
            Some(s) => FeedbackSummary {
                total: s.total,
                useful: s.useful,
                outdated: s.outdated,
                incorrect: s.incorrect,
                applies_here: s.applies_here,
                does_not_apply_here: s.does_not_apply_here,
                auto_promoted: s.auto_promoted,
            },
            None => FeedbackSummary::default(),
        })
    }

    async fn list_wings(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT DISTINCT project AS value FROM capability_capsules FINAL \
                 WHERE tenant = ? AND project != '' ORDER BY project ASC",
            )
            .bind(tenant)
            .fetch_all::<ChStringRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().map(|r| r.value).collect())
    }

    async fn capsule_stats(&self, tenant: &str) -> Result<CapsuleStats, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT \
                    count() AS total, \
                    countIf(status = 'pending_confirmation') AS pending_confirmation, \
                    countIf(status = 'provisional') AS provisional, \
                    countIf(status = 'active') AS active, \
                    countIf(status = 'archived') AS archived, \
                    countIf(status = 'rejected') AS rejected \
                 FROM capability_capsules FINAL WHERE tenant = ?",
            )
            .bind(tenant)
            .fetch_all::<ChStatsRow>()
            .await
            .map_err(ch_err)?;
        let s = rows.into_iter().next();
        Ok(match s {
            Some(s) => CapsuleStats {
                total: s.total,
                pending_confirmation: s.pending_confirmation,
                provisional: s.provisional,
                active: s.active,
                archived: s.archived,
                rejected: s.rejected,
            },
            None => CapsuleStats::default(),
        })
    }

    async fn get_taxonomy(&self, tenant: &str) -> Result<Vec<(String, Vec<String>)>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT DISTINCT project, repo FROM capability_capsules FINAL \
                 WHERE tenant = ? AND project != '' ORDER BY project ASC, repo ASC",
            )
            .bind(tenant)
            .fetch_all::<ChProjectRepoRow>()
            .await
            .map_err(ch_err)?;
        // Fold consecutive same-project rows into (project, repos) — repos
        // with `''` (no recorded repo) drop out of the inner vec.
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for row in rows {
            match out.last_mut() {
                Some((p, repos)) if *p == row.project => {
                    if !row.repo.is_empty() {
                        repos.push(row.repo);
                    }
                }
                _ => {
                    let repos = if row.repo.is_empty() {
                        Vec::new()
                    } else {
                        vec![row.repo]
                    };
                    out.push((row.project, repos));
                }
            }
        }
        Ok(out)
    }

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
    ) -> Result<(Vec<CapabilityCapsuleRecord>, bool), StorageError> {
        // Build the predicate incrementally. Each `Option` filter is a
        // bound `?`; `None` ⇒ the clause is omitted. `LIMIT N+1` drives
        // `has_more`. (Scaffold: optional binds are pushed as owned Strings
        // so the borrow set stays simple.)
        let mut sql =
            String::from("SELECT ?fields FROM capability_capsules FINAL WHERE tenant = ?");
        let mut binds: Vec<String> = vec![tenant.to_owned()];
        for (col, val) in [
            ("project", project),
            ("repo", repo),
            ("module", module),
            ("capability_capsule_type", capsule_type),
            ("status", status),
            ("source_agent", source_agent),
        ] {
            if let Some(v) = val {
                sql.push_str(&format!(" AND {col} = ?"));
                binds.push(v.to_owned());
            }
        }
        if let Some((cur_updated, cur_id)) = cursor {
            sql.push_str(" AND (updated_at < ? OR (updated_at = ? AND capability_capsule_id > ?))");
            binds.push(cur_updated.to_owned());
            binds.push(cur_updated.to_owned());
            binds.push(cur_id.to_owned());
        }
        sql.push_str(" ORDER BY updated_at DESC, capability_capsule_id ASC LIMIT ?");

        let want = limit.saturating_add(1);
        let mut q = self.client.query(&sql);
        for b in &binds {
            q = q.bind(b);
        }
        q = q.bind(want as u64);
        let rows = q.fetch_all::<ChCapsuleRow>().await.map_err(ch_err)?;

        let mut records: Vec<CapabilityCapsuleRecord> =
            rows.into_iter().map(ChCapsuleRow::into_record).collect();
        let has_more = records.len() > limit;
        records.truncate(limit);
        Ok((records, has_more))
    }
}
