use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::warn;

use crate::domain::{
    CompletedToolRound, CompletedToolRoundIndexBuild, RoundIndexBuildStatus, RoundIntegrity,
    RoundSealKind, SourceAdapter, COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
    COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
};
use crate::pipeline::completed_tool_round::project_completed_tool_rounds;
use crate::storage::{current_timestamp, CompletedToolRoundStore, StorageError};

const MAX_TENANT_BYTES: usize = 256;
const MAX_SESSION_ID_BYTES: usize = 1_024;
const MAX_SOURCE_BLOCKS: usize = 20_000;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RETURNED_ROUNDS: usize = 1_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletedToolRoundRebuildReport {
    pub status: String,
    pub generation_id: Option<String>,
    pub projector_version: u32,
    pub source_blocks: u64,
    pub completed_rounds: u64,
    pub clean_rounds: u64,
    pub gapped_rounds: u64,
    pub incomplete_segments: u64,
    pub auxiliary_tool_results: u64,
    pub degraded: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletedToolRoundRead {
    pub generation_id: Option<String>,
    pub projector_version: Option<u32>,
    pub total_rounds: u64,
    pub truncated: bool,
    pub rounds: Vec<CompletedToolRoundView>,
    pub degraded: bool,
}

/// Data-minimized admin view. Internal locators and fingerprints remain in
/// Lance for rebuild/debug composition but are not serialized over HTTP.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletedToolRoundView {
    pub round_id: String,
    pub tenant: String,
    pub caller_agent: String,
    pub source_adapter: SourceAdapter,
    pub session_id: Option<String>,
    pub start_line_number: u64,
    pub start_block_index: u32,
    pub end_line_number: u64,
    pub end_block_index: u32,
    pub tool_names: Vec<String>,
    pub tool_call_count: u32,
    pub matched_result_count: u32,
    pub missing_result_count: u32,
    pub orphan_result_count: u32,
    pub error_result_count: u32,
    pub unknown_result_status_count: u32,
    pub seal_kind: RoundSealKind,
    pub integrity: RoundIntegrity,
    pub projector_version: u32,
}

impl From<CompletedToolRound> for CompletedToolRoundView {
    fn from(round: CompletedToolRound) -> Self {
        Self {
            round_id: round.round_id,
            tenant: round.tenant,
            caller_agent: round.caller_agent,
            source_adapter: round.source_adapter,
            session_id: round.session_id,
            start_line_number: round.start_line_number,
            start_block_index: round.start_block_index,
            end_line_number: round.end_line_number,
            end_block_index: round.end_block_index,
            tool_names: round
                .tool_names
                .into_iter()
                .map(public_tool_family)
                .collect(),
            tool_call_count: round.tool_call_count,
            matched_result_count: round.matched_result_count,
            missing_result_count: round.missing_result_count,
            orphan_result_count: round.orphan_result_count,
            error_result_count: round.error_result_count,
            unknown_result_status_count: round.unknown_result_status_count,
            seal_kind: round.seal_kind,
            integrity: round.integrity,
            projector_version: round.projector_version,
        }
    }
}

fn public_tool_family(name: String) -> String {
    match name.as_str() {
        "shell" | "read_file" | "search_files" | "edit_file" | "other" => name,
        _ => "other".to_string(),
    }
}

#[derive(Clone)]
pub struct CompletedToolRoundService {
    store: Arc<dyn CompletedToolRoundStore>,
    rebuild_gate: Arc<Mutex<()>>,
}

impl CompletedToolRoundService {
    pub fn new(store: Arc<dyn CompletedToolRoundStore>) -> Self {
        Self {
            store,
            rebuild_gate: Arc::new(Mutex::new(())),
        }
    }

    pub async fn rebuild_session(
        &self,
        tenant: &str,
        session_id: &str,
        dry_run: bool,
    ) -> Result<CompletedToolRoundRebuildReport, StorageError> {
        validate_scope(tenant, session_id)?;
        let _guard = self.rebuild_gate.try_lock().map_err(|_| {
            StorageError::Conflict("completed tool round rebuild already in progress")
        })?;
        crate::metrics::metrics().inc_round_projection_rebuild();
        let messages = match self
            .store
            .load_round_source_messages(tenant, session_id, MAX_SOURCE_BLOCKS + 1, MAX_SOURCE_BYTES)
            .await
        {
            Ok(messages) => messages,
            Err(error @ StorageError::Unsupported(_)) => return Err(error),
            Err(error @ StorageError::InvalidInput(_)) => return Err(error),
            Err(error) => {
                crate::metrics::metrics().inc_round_projection_error();
                warn!(tenant, session_id, error = %error, "completed tool round source read degraded");
                return Ok(degraded_rebuild_report());
            }
        };
        if messages.len() > MAX_SOURCE_BLOCKS {
            return Err(StorageError::InvalidInput(format!(
                "session exceeds completed tool round source limit of {MAX_SOURCE_BLOCKS} blocks"
            )));
        }
        let source_bytes = messages.iter().fold(0usize, |total, message| {
            total.saturating_add(message.owned_string_bytes())
        });
        if source_bytes > MAX_SOURCE_BYTES {
            return Err(StorageError::InvalidInput(format!(
                "session exceeds completed tool round source limit of {MAX_SOURCE_BYTES} bytes"
            )));
        }
        let projection = project_completed_tool_rounds(&messages);
        let clean_rounds = projection
            .rounds
            .iter()
            .filter(|round| round.integrity == RoundIntegrity::Clean)
            .count() as u64;
        let gapped_rounds = projection.rounds.len() as u64 - clean_rounds;
        crate::metrics::metrics().record_round_projection(
            projection.source_block_count,
            projection.rounds.len() as u64,
            gapped_rounds,
            projection.incomplete_segments,
        );

        let report =
            |status: &str, generation_id: Option<String>| CompletedToolRoundRebuildReport {
                status: status.to_string(),
                generation_id,
                projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
                source_blocks: projection.source_block_count,
                completed_rounds: projection.rounds.len() as u64,
                clean_rounds,
                gapped_rounds,
                incomplete_segments: projection.incomplete_segments,
                auxiliary_tool_results: projection.auxiliary_tool_result_count,
                degraded: false,
            };
        if dry_run {
            return Ok(report("dry_run", None));
        }

        let latest = match self
            .store
            .latest_completed_tool_rounds(tenant, session_id)
            .await
        {
            Ok(latest) => latest,
            Err(error @ StorageError::Unsupported(_)) => return Err(error),
            Err(error) => {
                crate::metrics::metrics().inc_round_projection_error();
                warn!(tenant, session_id, error = %error, "completed tool round index read degraded before rebuild");
                return Ok(degraded_rebuild_report());
            }
        };
        if let Some(build) = latest.build {
            if build.projector_version == COMPLETED_TOOL_ROUND_PROJECTOR_VERSION
                && build.task_signal_version == COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION
                && build.source_fingerprint == projection.source_fingerprint
                && build.round_count == projection.rounds.len() as u64
                && latest.stored_round_count == build.round_count
            {
                return Ok(report("unchanged", Some(build.generation_id)));
            }
        }

        let now = current_timestamp();
        let generation_id = uuid::Uuid::now_v7().to_string();
        let build = CompletedToolRoundIndexBuild {
            generation_id: generation_id.clone(),
            tenant: tenant.to_string(),
            session_id: session_id.to_string(),
            projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
            task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
            status: RoundIndexBuildStatus::Completed,
            source_block_count: projection.source_block_count,
            source_fingerprint: projection.source_fingerprint,
            round_count: projection.rounds.len() as u64,
            started_at: now.clone(),
            completed_at: Some(now),
        };
        if let Err(error) = self
            .store
            .publish_completed_tool_round_generation(&build, &projection.rounds)
            .await
        {
            crate::metrics::metrics().inc_round_projection_error();
            return Err(error);
        }
        Ok(report("published", Some(generation_id)))
    }

    pub async fn latest(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<CompletedToolRoundRead, StorageError> {
        validate_scope(tenant, session_id)?;
        match self
            .store
            .latest_completed_tool_rounds(tenant, session_id)
            .await
        {
            Ok(latest) => {
                let generation_id = latest
                    .build
                    .as_ref()
                    .map(|build| build.generation_id.clone());
                let projector_version = latest.build.as_ref().map(|build| build.projector_version);
                let total_rounds = latest.stored_round_count;
                let expected_hydrated = total_rounds.min(MAX_RETURNED_ROUNDS as u64);
                let degraded = latest.build.as_ref().is_some_and(|build| {
                    build.round_count != total_rounds
                        || latest.rounds.len() as u64 != expected_hydrated
                });
                if degraded {
                    crate::metrics::metrics().inc_round_projection_read_degraded();
                    warn!(
                        tenant,
                        session_id,
                        declared_rounds = latest
                            .build
                            .as_ref()
                            .map(|build| build.round_count)
                            .unwrap_or_default(),
                        stored_rounds = total_rounds,
                        hydrated_rounds = latest.rounds.len(),
                        "completed tool round generation count mismatch"
                    );
                }
                let truncated = total_rounds > MAX_RETURNED_ROUNDS as u64;
                let rounds = latest
                    .rounds
                    .into_iter()
                    .take(expected_hydrated as usize)
                    .map(CompletedToolRoundView::from)
                    .collect();
                Ok(CompletedToolRoundRead {
                    generation_id,
                    projector_version,
                    total_rounds,
                    truncated,
                    rounds,
                    degraded,
                })
            }
            Err(error @ StorageError::Unsupported(_)) => Err(error),
            Err(error) => {
                crate::metrics::metrics().inc_round_projection_read_degraded();
                warn!(tenant, session_id, error = %error, "completed tool round index read degraded");
                Ok(CompletedToolRoundRead {
                    degraded: true,
                    ..CompletedToolRoundRead::default()
                })
            }
        }
    }
}

fn validate_scope(tenant: &str, session_id: &str) -> Result<(), StorageError> {
    if tenant.trim().is_empty() || tenant.chars().any(char::is_control) {
        return Err(StorageError::InvalidInput(
            "tenant must not be empty or contain control characters".into(),
        ));
    }
    if tenant.len() > MAX_TENANT_BYTES {
        return Err(StorageError::InvalidInput(format!(
            "tenant must not exceed {MAX_TENANT_BYTES} bytes"
        )));
    }
    if session_id.trim().is_empty() || session_id.chars().any(char::is_control) {
        return Err(StorageError::InvalidInput(
            "session_id must not be empty or contain control characters".into(),
        ));
    }
    if session_id.len() > MAX_SESSION_ID_BYTES {
        return Err(StorageError::InvalidInput(format!(
            "session_id must not exceed {MAX_SESSION_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn degraded_rebuild_report() -> CompletedToolRoundRebuildReport {
    CompletedToolRoundRebuildReport {
        status: "degraded".to_string(),
        generation_id: None,
        projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
        source_blocks: 0,
        completed_rounds: 0,
        clean_rounds: 0,
        gapped_rounds: 0,
        incomplete_segments: 0,
        auxiliary_tool_results: 0,
        degraded: true,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use async_trait::async_trait;

    use super::*;
    use crate::domain::{
        BlockType, CompletedToolRoundIndexBuild, ConversationMessage, LatestCompletedToolRounds,
        MessageRole,
    };

    #[derive(Default)]
    struct FakeState {
        messages: Vec<ConversationMessage>,
        latest: LatestCompletedToolRounds,
        fail_source: bool,
        fail_publish: bool,
        fail_latest: bool,
    }

    #[derive(Default)]
    struct FakeStore {
        state: StdMutex<FakeState>,
    }

    #[async_trait]
    impl CompletedToolRoundStore for FakeStore {
        async fn load_round_source_messages(
            &self,
            _tenant: &str,
            _session_id: &str,
            max_blocks: usize,
            _max_bytes: usize,
        ) -> Result<Vec<ConversationMessage>, StorageError> {
            let state = self.state.lock().unwrap();
            if state.fail_source {
                Err(StorageError::InvalidData("injected source failure"))
            } else {
                Ok(state.messages.iter().take(max_blocks).cloned().collect())
            }
        }

        async fn publish_completed_tool_round_generation(
            &self,
            build: &CompletedToolRoundIndexBuild,
            rounds: &[CompletedToolRound],
        ) -> Result<(), StorageError> {
            let mut state = self.state.lock().unwrap();
            if state.fail_publish {
                return Err(StorageError::InvalidData("injected publish failure"));
            }
            state.latest = LatestCompletedToolRounds {
                build: Some(build.clone()),
                stored_round_count: rounds.len() as u64,
                rounds: rounds.to_vec(),
            };
            Ok(())
        }

        async fn latest_completed_tool_rounds(
            &self,
            _tenant: &str,
            _session_id: &str,
        ) -> Result<LatestCompletedToolRounds, StorageError> {
            let state = self.state.lock().unwrap();
            if state.fail_latest {
                Err(StorageError::InvalidData("injected latest failure"))
            } else {
                Ok(state.latest.clone())
            }
        }
    }

    fn messages(final_text: &str) -> Vec<ConversationMessage> {
        let block = |line_number, role, block_type, content: &str, tool_use_id: Option<&str>| {
            ConversationMessage {
                message_block_id: format!("mb-{line_number}"),
                session_id: Some("s1".into()),
                tenant: "local".into(),
                caller_agent: "codex".into(),
                transcript_path: "/tmp/codex.jsonl".into(),
                line_number,
                block_index: 0,
                message_uuid: None,
                role,
                block_type,
                content: content.into(),
                tool_name: (block_type == BlockType::ToolUse).then(|| "health".into()),
                tool_use_id: tool_use_id.map(str::to_string),
                embed_eligible: false,
                created_at: format!("0000000000000000000{line_number}"),
                meta_json: None,
            }
        };
        vec![
            block(1, MessageRole::User, BlockType::Text, "check", None),
            block(
                2,
                MessageRole::Assistant,
                BlockType::ToolUse,
                "{}",
                Some("call-1"),
            ),
            block(
                3,
                MessageRole::User,
                BlockType::ToolResult,
                "ok",
                Some("call-1"),
            ),
            block(4, MessageRole::Assistant, BlockType::Text, final_text, None),
        ]
    }

    #[test]
    fn admin_view_rechecks_persisted_tool_family_allowlist() {
        assert_eq!(public_tool_family("shell".into()), "shell");
        assert_eq!(public_tool_family("secret/raw-tool-name".into()), "other");
    }

    #[tokio::test]
    async fn publish_failure_keeps_the_last_completed_generation_visible() {
        let store = Arc::new(FakeStore::default());
        store.state.lock().unwrap().messages = messages("first");
        let service = CompletedToolRoundService::new(store.clone());
        let first = service.rebuild_session("local", "s1", false).await.unwrap();
        let first_generation = first.generation_id.unwrap();

        {
            let mut state = store.state.lock().unwrap();
            state.messages = messages("changed");
            state.fail_publish = true;
        }
        assert!(service.rebuild_session("local", "s1", false).await.is_err());
        let latest = service.latest("local", "s1").await.unwrap();

        assert_eq!(
            latest.generation_id.as_deref(),
            Some(first_generation.as_str())
        );
        assert_eq!(latest.rounds.len(), 1);
        assert_eq!(latest.rounds[0].end_line_number, 4);
    }

    #[tokio::test]
    async fn source_and_index_read_failures_soft_degrade() {
        let store = Arc::new(FakeStore::default());
        store.state.lock().unwrap().fail_source = true;
        let service = CompletedToolRoundService::new(store.clone());

        let rebuild = service.rebuild_session("local", "s1", false).await.unwrap();
        assert!(rebuild.degraded);
        assert_eq!(rebuild.status, "degraded");

        {
            let mut state = store.state.lock().unwrap();
            state.fail_source = false;
            state.fail_latest = true;
        }
        let latest = service.latest("local", "s1").await.unwrap();
        assert!(latest.degraded);
        assert!(latest.rounds.is_empty());
    }

    #[tokio::test]
    async fn incomplete_latest_generation_is_rebuilt_instead_of_reported_unchanged() {
        let store = Arc::new(FakeStore::default());
        store.state.lock().unwrap().messages = messages("first");
        let service = CompletedToolRoundService::new(store.clone());
        let first = service.rebuild_session("local", "s1", false).await.unwrap();

        store.state.lock().unwrap().latest.stored_round_count = 0;
        let inconsistent = service.latest("local", "s1").await.unwrap();
        assert!(inconsistent.degraded);
        assert_eq!(inconsistent.total_rounds, 0);
        assert!(inconsistent.rounds.is_empty());
        let repaired = service.rebuild_session("local", "s1", false).await.unwrap();
        let latest = service.latest("local", "s1").await.unwrap();

        assert_eq!(repaired.status, "published");
        assert_ne!(repaired.generation_id, first.generation_id);
        assert_eq!(latest.rounds.len(), 1);
    }

    #[tokio::test]
    async fn stale_task_signal_version_forces_a_new_generation() {
        let store = Arc::new(FakeStore::default());
        store.state.lock().unwrap().messages = messages("first");
        let service = CompletedToolRoundService::new(store.clone());
        let first = service.rebuild_session("local", "s1", false).await.unwrap();

        store
            .state
            .lock()
            .unwrap()
            .latest
            .build
            .as_mut()
            .unwrap()
            .task_signal_version = 0;
        let repaired = service.rebuild_session("local", "s1", false).await.unwrap();

        assert_eq!(repaired.status, "published");
        assert_ne!(repaired.generation_id, first.generation_id);
    }
}
