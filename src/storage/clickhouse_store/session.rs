//! `SessionStore` for [`ClickHouseBackend`].
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P5).** `sessions` + `episodes`, both
//! `ReplacingMergeTree(row_version)`. Session lifecycle mutations
//! (touch/close) are read-latest + versioned re-insert (§4(a)); episodes
//! are append. `workflow_candidate` is stored as a JSON string ('' = None).

use async_trait::async_trait;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::backend::ClickHouseBackend;
use super::capsule_store::{ch_err, enum_from_str, enum_to_str, now_version, opt};
use crate::domain::capability_capsule::{Scope, Visibility};
use crate::domain::episode::EpisodeRecord;
use crate::domain::session::Session;
use crate::domain::workflow::WorkflowCandidate;
use crate::storage::types::StorageError;
use crate::storage::SessionStore;

#[derive(Row, Serialize, Deserialize)]
struct ChSessionRow {
    session_id: String,
    tenant: String,
    caller_agent: String,
    started_at: String,
    last_seen_at: String,
    ended_at: String,
    goal: String,
    memory_count: u32,
    row_version: u64,
}

impl ChSessionRow {
    fn into_session(self) -> Session {
        Session {
            session_id: self.session_id,
            tenant: self.tenant,
            caller_agent: self.caller_agent,
            started_at: self.started_at,
            last_seen_at: self.last_seen_at,
            ended_at: opt(self.ended_at),
            goal: opt(self.goal),
            memory_count: self.memory_count,
        }
    }
}

#[derive(Row, Serialize, Deserialize)]
struct ChEpisodeRow {
    episode_id: String,
    tenant: String,
    goal: String,
    steps: Vec<String>,
    outcome: String,
    evidence: Vec<String>,
    scope: String,
    visibility: String,
    project: String,
    repo: String,
    module: String,
    tags: Vec<String>,
    source_agent: String,
    idempotency_key: String,
    created_at: String,
    updated_at: String,
    workflow_candidate: String,
    row_version: u64,
}

impl ChEpisodeRow {
    fn from_record(e: &EpisodeRecord) -> Self {
        Self {
            episode_id: e.episode_id.clone(),
            tenant: e.tenant.clone(),
            goal: e.goal.clone(),
            steps: e.steps.clone(),
            outcome: e.outcome.clone(),
            evidence: e.evidence.clone(),
            scope: enum_to_str(&e.scope),
            visibility: enum_to_str(&e.visibility),
            project: e.project.clone().unwrap_or_default(),
            repo: e.repo.clone().unwrap_or_default(),
            module: e.module.clone().unwrap_or_default(),
            tags: e.tags.clone(),
            source_agent: e.source_agent.clone(),
            idempotency_key: e.idempotency_key.clone().unwrap_or_default(),
            created_at: e.created_at.clone(),
            updated_at: e.updated_at.clone(),
            workflow_candidate: e
                .workflow_candidate
                .as_ref()
                .and_then(|w| serde_json::to_string(w).ok())
                .unwrap_or_default(),
            row_version: now_version(),
        }
    }

    fn into_record(self) -> EpisodeRecord {
        EpisodeRecord {
            episode_id: self.episode_id,
            tenant: self.tenant,
            goal: self.goal,
            steps: self.steps,
            outcome: self.outcome,
            evidence: self.evidence,
            scope: enum_from_str::<Scope>(&self.scope),
            visibility: enum_from_str::<Visibility>(&self.visibility),
            project: opt(self.project),
            repo: opt(self.repo),
            module: opt(self.module),
            tags: self.tags,
            source_agent: self.source_agent,
            idempotency_key: opt(self.idempotency_key),
            created_at: self.created_at,
            updated_at: self.updated_at,
            workflow_candidate: if self.workflow_candidate.is_empty() {
                None
            } else {
                serde_json::from_str::<WorkflowCandidate>(&self.workflow_candidate).ok()
            },
        }
    }
}

impl ClickHouseBackend {
    async fn ch_session_by_id(
        &self,
        session_id: &str,
    ) -> Result<Option<ChSessionRow>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM sessions FINAL WHERE session_id = ? \
                 ORDER BY row_version DESC LIMIT 1",
            )
            .bind(session_id)
            .fetch_all::<ChSessionRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().next())
    }

    async fn ch_write_session(&self, row: ChSessionRow) -> Result<(), StorageError> {
        let mut insert = self
            .client
            .insert::<ChSessionRow>("sessions")
            .await
            .map_err(ch_err)?;
        insert.write(&row).await.map_err(ch_err)?;
        insert.end().await.map_err(ch_err)?;
        Ok(())
    }
}

#[async_trait]
impl SessionStore for ClickHouseBackend {
    async fn touch_session(
        &self,
        session_id: &str,
        last_active_at: &str,
    ) -> Result<(), StorageError> {
        let Some(mut row) = self.ch_session_by_id(session_id).await? else {
            return Ok(());
        };
        row.last_seen_at = last_active_at.to_owned();
        row.memory_count = row.memory_count.saturating_add(1);
        row.row_version = now_version();
        self.ch_write_session(row).await
    }

    async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        if let Some(existing) = self.ch_session_by_id(session_id).await? {
            return Ok(existing.into_session());
        }
        let row = ChSessionRow {
            session_id: session_id.to_owned(),
            tenant: tenant.to_owned(),
            caller_agent: caller_agent.to_owned(),
            started_at: now.to_owned(),
            last_seen_at: now.to_owned(),
            ended_at: String::new(),
            goal: String::new(),
            memory_count: 0,
            row_version: now_version(),
        };
        self.ch_write_session(row).await?;
        Ok(Session {
            session_id: session_id.to_owned(),
            tenant: tenant.to_owned(),
            caller_agent: caller_agent.to_owned(),
            started_at: now.to_owned(),
            last_seen_at: now.to_owned(),
            ended_at: None,
            goal: None,
            memory_count: 0,
        })
    }

    async fn close_session(&self, session_id: &str, ended_at: &str) -> Result<(), StorageError> {
        let Some(mut row) = self.ch_session_by_id(session_id).await? else {
            return Ok(());
        };
        row.ended_at = ended_at.to_owned();
        row.row_version = now_version();
        self.ch_write_session(row).await
    }

    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM sessions FINAL \
                 WHERE tenant = ? AND caller_agent = ? AND ended_at = '' \
                 ORDER BY last_seen_at DESC LIMIT 1",
            )
            .bind(tenant)
            .bind(caller_agent)
            .fetch_all::<ChSessionRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().next().map(ChSessionRow::into_session))
    }

    async fn insert_episode(&self, episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError> {
        let mut insert = self
            .client
            .insert::<ChEpisodeRow>("episodes")
            .await
            .map_err(ch_err)?;
        insert
            .write(&ChEpisodeRow::from_record(&episode))
            .await
            .map_err(ch_err)?;
        insert.end().await.map_err(ch_err)?;
        Ok(episode)
    }

    async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM episodes FINAL \
                 WHERE tenant = ? AND outcome IN ('success', 'succeeded') \
                 ORDER BY created_at DESC",
            )
            .bind(tenant)
            .fetch_all::<ChEpisodeRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().map(ChEpisodeRow::into_record).collect())
    }
}
