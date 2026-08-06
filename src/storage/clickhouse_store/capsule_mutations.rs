//! ClickHouse mutations that need synchronous ordering guarantees.

use clickhouse::Row;
use serde::Deserialize;

use super::{
    backend::ClickHouseBackend,
    capsule_store::{ch_err, enum_to_str},
};
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, FeedbackKind,
};
use crate::storage::{current_timestamp, StorageError};

#[derive(Debug, Row, Deserialize)]
struct ChValueRow {
    value: String,
}

enum SatelliteTable {
    FeedbackEvents,
    EmbeddingJobs,
    CapsuleEmbeddings,
}

impl SatelliteTable {
    fn name(&self) -> &'static str {
        match self {
            Self::FeedbackEvents => "feedback_events",
            Self::EmbeddingJobs => "embedding_jobs",
            Self::CapsuleEmbeddings => "capability_capsule_embeddings",
        }
    }
}

impl ClickHouseBackend {
    /// Claim a pending review verdict with compare-and-set semantics.
    pub(super) async fn commit_pending_verdict(
        &self,
        tenant: &str,
        id: &str,
        status: &CapabilityCapsuleStatus,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        crate::storage::capsule_store::validate_pending_verdict(status)?;
        // A winner token must be unique across independent `mem serve`
        // processes. `now_version()` only guarantees process-local uniqueness,
        // so use UUIDv7 here and reserve the timestamp counter for row versions.
        let token = uuid::Uuid::now_v7().to_string();
        self.mutate_pending_verdict(tenant, id, status, &token)
            .await?;

        let record = self.latest(Some(tenant), id).await?;
        let winner_token = self.latest_review_token(tenant, id).await?;
        match (record, winner_token) {
            (Some(record), Some(winner_token))
                if record.status == *status && winner_token == token =>
            {
                Ok(record)
            }
            (Some(_), Some(_)) => Err(StorageError::Conflict("review conflict")),
            _ => Err(StorageError::NotFound("capability capsule")),
        }
    }

    async fn mutate_pending_verdict(
        &self,
        tenant: &str,
        id: &str,
        status: &CapabilityCapsuleStatus,
        token: &str,
    ) -> Result<(), StorageError> {
        self.client
            .query(
                "ALTER TABLE capability_capsules UPDATE \
                   status = ?, updated_at = ?, review_token = ? \
                 WHERE tenant = ? AND capability_capsule_id = ? \
                   AND status = 'pending_confirmation' \
                 SETTINGS mutations_sync = 1",
            )
            .bind(enum_to_str(status))
            .bind(current_timestamp())
            .bind(token)
            .bind(tenant)
            .bind(id)
            .execute()
            .await
            .map_err(ch_err)
    }

    async fn latest_review_token(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<String>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT review_token AS value FROM capability_capsules FINAL \
                 WHERE tenant = ? AND capability_capsule_id = ? LIMIT 1",
            )
            .bind(tenant)
            .bind(id)
            .fetch_all::<ChValueRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().next().map(|row| row.value))
    }

    /// Apply a feedback delta against live values, not the caller's snapshot.
    pub(super) async fn apply_feedback_delta(
        &self,
        memory: &CapabilityCapsuleRecord,
        kind: FeedbackKind,
        occurred_at: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let status = kind.status_after().map(|value| enum_to_str(&value));
        let validated_at = if kind.marks_validated() {
            occurred_at
        } else {
            ""
        };
        self.mutate_feedback(memory, &kind, status.as_deref(), validated_at, occurred_at)
            .await?;
        self.latest(Some(&memory.tenant), &memory.capability_capsule_id)
            .await?
            .ok_or(StorageError::InvalidData(
                "memory missing after feedback apply",
            ))
    }

    async fn mutate_feedback(
        &self,
        memory: &CapabilityCapsuleRecord,
        kind: &FeedbackKind,
        status: Option<&str>,
        validated_at: &str,
        occurred_at: &str,
    ) -> Result<(), StorageError> {
        let status = status.unwrap_or_default();
        self.client
            .query(
                "ALTER TABLE capability_capsules UPDATE \
                   confidence = least(toFloat32(1.0), confidence + ?), \
                   decay_score = least(toFloat32(1.0), decay_score + ?), \
                   status = if(? = '', status, ?), \
                   last_validated_at = if(? = '', last_validated_at, ?), \
                   updated_at = ? \
                 WHERE tenant = ? AND capability_capsule_id = ? \
                 SETTINGS mutations_sync = 1",
            )
            .bind(kind.confidence_delta())
            .bind(kind.decay_delta())
            .bind(status)
            .bind(status)
            .bind(validated_at)
            .bind(validated_at)
            .bind(occurred_at)
            .bind(&memory.tenant)
            .bind(&memory.capability_capsule_id)
            .execute()
            .await
            .map_err(ch_err)
    }

    /// Delete satellites synchronously and retain the parent as the tenant
    /// authorization anchor until every cleanup step has succeeded.
    pub(super) async fn hard_delete_capsule(
        &self,
        tenant: &str,
        capsule_id: &str,
    ) -> Result<(), StorageError> {
        let _lifecycle_guard = self.capsule_lifecycle_gate.write().await;
        if self.latest(Some(tenant), capsule_id).await?.is_none() {
            return Err(StorageError::NotFound("capability capsule"));
        }

        for table in [
            SatelliteTable::FeedbackEvents,
            SatelliteTable::EmbeddingJobs,
            SatelliteTable::CapsuleEmbeddings,
        ] {
            self.delete_satellite(table, capsule_id).await?;
        }
        self.close_capsule_edges(capsule_id).await?;
        self.delete_capsule_parent(tenant, capsule_id).await?;
        self.deleted_capsule_ids
            .write()
            .expect("deleted_capsule_ids lock poisoned")
            .insert(capsule_id.to_owned());
        Ok(())
    }

    async fn delete_satellite(
        &self,
        table: SatelliteTable,
        capsule_id: &str,
    ) -> Result<(), StorageError> {
        let sql = format!(
            "ALTER TABLE {} DELETE WHERE capability_capsule_id = ? \
             SETTINGS mutations_sync = 1",
            table.name()
        );
        self.client
            .query(&sql)
            .bind(capsule_id)
            .execute()
            .await
            .map_err(ch_err)
    }

    async fn close_capsule_edges(&self, capsule_id: &str) -> Result<(), StorageError> {
        let node = format!("capability_capsule:{capsule_id}");
        self.client
            .query(
                "ALTER TABLE graph_edges UPDATE valid_to = ? \
                 WHERE valid_to = '' AND (from_node_id = ? OR to_node_id = ?) \
                 SETTINGS mutations_sync = 1",
            )
            .bind(current_timestamp())
            .bind(&node)
            .bind(&node)
            .execute()
            .await
            .map_err(ch_err)
    }

    async fn delete_capsule_parent(
        &self,
        tenant: &str,
        capsule_id: &str,
    ) -> Result<(), StorageError> {
        self.client
            .query(
                "ALTER TABLE capability_capsules DELETE \
                 WHERE tenant = ? AND capability_capsule_id = ? \
                 SETTINGS mutations_sync = 1",
            )
            .bind(tenant)
            .bind(capsule_id)
            .execute()
            .await
            .map_err(ch_err)
    }
}
