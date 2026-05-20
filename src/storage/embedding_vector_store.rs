//! Backend-agnostic embedding vector storage — Phase 3 sub-trait.
//!
//! Covers vector upsert / delete / row-lookup for both
//! `capability_capsule_embeddings` (capsule side) and
//! `conversation_message_embeddings` (transcript side).
//!
//! **LANCE-SPECIFIC bits**: both tables are lazy-created on first
//! upsert because the vector dim is provider-dependent and unknown
//! at `Store::open` time. Portable backends (pgvector,
//! external ANN) need a different bootstrap; the trait's
//! `upsert_*` methods accept `embedding_dim` as a parameter so each
//! backend can do its own dim-aware setup internally.
//!
//! See `docs/backend-coupling.md` §3.1 + §6.4.

use async_trait::async_trait;

use crate::storage::types::StorageError;
use crate::storage::Store;

#[async_trait]
pub trait EmbeddingVectorStore: Send + Sync {
    /// Upsert a capsule embedding row. `embedding_blob` is the
    /// native-endian f32 bytes (see `crate::embedding::wire`);
    /// `embedding_dim` is the vector length. Lazy-creates the
    /// underlying table on first call.
    #[allow(clippy::too_many_arguments)]
    async fn upsert_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        embedding_blob: &[u8],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    /// Delete the embedding row for one capsule. Idempotent — no
    /// error if no row exists.
    async fn delete_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError>;

    /// Read the embedding-metadata triple
    /// `(model, content_hash, source_updated_at)` for one capsule.
    /// Returns `None` if no embedding row exists.
    async fn get_capability_capsule_embedding_row(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError>;

    /// Read the raw embedding vector for `capability_capsule_id`. Used
    /// by the dedup worker for pairwise cosine. Returns `None` when
    /// the embeddings table doesn't exist yet or no row matches.
    async fn get_capability_capsule_embedding_vector(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<Vec<f32>>, StorageError>;

    /// Upsert a transcript-block embedding. Same wire format as
    /// the capsule side; targets the separate
    /// `conversation_message_embeddings` table.
    #[allow(clippy::too_many_arguments)]
    async fn upsert_conversation_message_embedding(
        &self,
        message_block_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        embedding_blob: &[u8],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    /// Delete a transcript-block embedding row. Idempotent.
    async fn delete_conversation_message_embedding(
        &self,
        message_block_id: &str,
    ) -> Result<(), StorageError>;
}

#[async_trait]
impl EmbeddingVectorStore for Store {
    async fn upsert_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        embedding_blob: &[u8],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        Store::upsert_capability_capsule_embedding(
            self,
            capability_capsule_id,
            tenant,
            embedding_model,
            embedding_dim,
            embedding_blob,
            content_hash,
            source_updated_at,
            now,
        )
        .await
    }

    async fn delete_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        Store::delete_capability_capsule_embedding(self, capability_capsule_id).await
    }

    async fn get_capability_capsule_embedding_row(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        self.lance
            .get_capability_capsule_embedding_row(capability_capsule_id)
            .await
    }

    async fn get_capability_capsule_embedding_vector(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<Vec<f32>>, StorageError> {
        self.lance
            .get_capability_capsule_embedding_vector(capability_capsule_id)
            .await
    }

    async fn upsert_conversation_message_embedding(
        &self,
        message_block_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        embedding_blob: &[u8],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        Store::upsert_conversation_message_embedding(
            self,
            message_block_id,
            tenant,
            embedding_model,
            embedding_dim,
            embedding_blob,
            content_hash,
            source_updated_at,
            now,
        )
        .await
    }

    async fn delete_conversation_message_embedding(
        &self,
        message_block_id: &str,
    ) -> Result<(), StorageError> {
        Store::delete_conversation_message_embedding(self, message_block_id).await
    }
}
