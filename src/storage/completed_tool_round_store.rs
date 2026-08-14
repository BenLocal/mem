use async_trait::async_trait;

use crate::domain::{
    CompletedToolRound, CompletedToolRoundIndexBuild, ConversationMessage,
    LatestCompletedToolRounds,
};

use super::{StorageError, Store};

/// Narrow persistence seam for the rebuildable completed-tool-round index.
/// It deliberately includes the source read used by the projector so tests
/// can inject read and publication failures without implementing `Backend`'s
/// unrelated storage surfaces.
#[async_trait]
pub trait CompletedToolRoundStore: Send + Sync {
    async fn load_round_source_messages(
        &self,
        tenant: &str,
        session_id: &str,
        max_blocks: usize,
        max_bytes: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError>;

    async fn publish_completed_tool_round_generation(
        &self,
        build: &CompletedToolRoundIndexBuild,
        rounds: &[CompletedToolRound],
    ) -> Result<(), StorageError>;

    async fn latest_completed_tool_rounds(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<LatestCompletedToolRounds, StorageError>;
}

#[async_trait]
impl CompletedToolRoundStore for Store {
    async fn load_round_source_messages(
        &self,
        tenant: &str,
        session_id: &str,
        max_blocks: usize,
        max_bytes: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.lance
            .get_conversation_messages_by_session_capped(tenant, session_id, max_blocks, max_bytes)
            .await
    }

    async fn publish_completed_tool_round_generation(
        &self,
        build: &CompletedToolRoundIndexBuild,
        rounds: &[CompletedToolRound],
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .publish_completed_tool_round_generation(build, rounds)
                .await,
        )
        .await
    }

    async fn latest_completed_tool_rounds(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<LatestCompletedToolRounds, StorageError> {
        self.lance
            .latest_completed_tool_rounds(tenant, session_id)
            .await
    }
}

/// Explicit adapter used when `mem serve` runs on a backend whose round-index
/// parity has not landed yet. Returning 501 is safer than silently pretending
/// an empty index is authoritative.
pub struct UnsupportedCompletedToolRoundStore;

#[async_trait]
impl CompletedToolRoundStore for UnsupportedCompletedToolRoundStore {
    async fn load_round_source_messages(
        &self,
        _tenant: &str,
        _session_id: &str,
        _max_blocks: usize,
        _max_bytes: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        Err(StorageError::Unsupported("completed tool round index"))
    }

    async fn publish_completed_tool_round_generation(
        &self,
        _build: &CompletedToolRoundIndexBuild,
        _rounds: &[CompletedToolRound],
    ) -> Result<(), StorageError> {
        Err(StorageError::Unsupported("completed tool round index"))
    }

    async fn latest_completed_tool_rounds(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> Result<LatestCompletedToolRounds, StorageError> {
        Err(StorageError::Unsupported("completed tool round index"))
    }
}
