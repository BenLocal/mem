//! ClickHouse `EmbeddingVectorStore` impl — capsule + conversation-message
//! vector upsert / get / delete.
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P3).**
//!
//! Vectors live in `embedding Array(Float32)` columns
//! (`migrations/clickhouse/0002`). Each upsert is an **append**: all rows of
//! one call share a fresh `row_version` ("generation"), and `chunk_index`
//! keeps the N chunk-vectors of one id distinct (ReplacingMergeTree would
//! otherwise collapse them). Reads take the latest row via
//! `ORDER BY row_version DESC` (the capsule side is single-row). Delete is a
//! rare `ALTER … DELETE` mutation — embeddings are derived data, so the async
//! mutation latency is acceptable here (unlike the hot lifecycle writes, which
//! stay versioned-insert). See docs/clickhouse-backend.md §4(a)/§4(d) + §10 P3.

use async_trait::async_trait;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::capsule_store::{ch_err, now_version};
use super::ClickHouseBackend;
use crate::embedding::wire::decode_f32_blob;
use crate::storage::embedding_vector_store::EmbeddingVectorStore;
use crate::storage::types::StorageError;

/// `capability_capsule_embeddings` row (mirrors migration 0002 columns).
#[derive(Debug, Row, Serialize, Deserialize)]
struct ChCapsuleEmbeddingRow {
    capability_capsule_id: String,
    tenant: String,
    embedding_model: String,
    embedding_dim: i64,
    embedding: Vec<f32>,
    content_hash: String,
    source_updated_at: String,
    created_at: String,
    updated_at: String,
    chunk_index: u32,
    row_version: u64,
}

/// `conversation_message_embeddings` row (same shape, `message_block_id` key).
#[derive(Debug, Row, Serialize, Deserialize)]
struct ChMsgEmbeddingRow {
    message_block_id: String,
    tenant: String,
    embedding_model: String,
    embedding_dim: i64,
    embedding: Vec<f32>,
    content_hash: String,
    source_updated_at: String,
    created_at: String,
    updated_at: String,
    chunk_index: u32,
    row_version: u64,
}

/// Projection: just the vector column (`get_*_embedding_vector`).
#[derive(Debug, Row, Deserialize)]
struct VecOnlyRow {
    embedding: Vec<f32>,
}

/// Projection: the `(model, content_hash, source_updated_at)` triple
/// (`get_capability_capsule_embedding_row`).
#[derive(Debug, Row, Deserialize)]
struct MetaTripleRow {
    embedding_model: String,
    content_hash: String,
    source_updated_at: String,
}

impl ClickHouseBackend {
    /// Insert N capsule-embedding rows (one generation, shared `row_version`).
    async fn insert_capsule_embedding_rows(
        &self,
        rows: &[ChCapsuleEmbeddingRow],
    ) -> Result<(), StorageError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut insert = self
            .client
            .insert::<ChCapsuleEmbeddingRow>("capability_capsule_embeddings")
            .await
            .map_err(ch_err)?;
        for row in rows {
            insert.write(row).await.map_err(ch_err)?;
        }
        insert.end().await.map_err(ch_err)?;
        Ok(())
    }

    /// Insert N message-embedding rows (one generation, shared `row_version`).
    async fn insert_msg_embedding_rows(
        &self,
        rows: &[ChMsgEmbeddingRow],
    ) -> Result<(), StorageError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut insert = self
            .client
            .insert::<ChMsgEmbeddingRow>("conversation_message_embeddings")
            .await
            .map_err(ch_err)?;
        for row in rows {
            insert.write(row).await.map_err(ch_err)?;
        }
        insert.end().await.map_err(ch_err)?;
        Ok(())
    }
}

#[async_trait]
impl EmbeddingVectorStore for ClickHouseBackend {
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
        let vector = decode_f32_blob(embedding_blob, embedding_dim as usize)
            .map_err(|e| StorageError::InvalidInput(format!("embedding blob decode: {e}")))?;
        let row = ChCapsuleEmbeddingRow {
            capability_capsule_id: capability_capsule_id.to_owned(),
            tenant: tenant.to_owned(),
            embedding_model: embedding_model.to_owned(),
            embedding_dim,
            embedding: vector,
            content_hash: content_hash.to_owned(),
            source_updated_at: source_updated_at.to_owned(),
            created_at: now.to_owned(),
            updated_at: now.to_owned(),
            chunk_index: 0,
            row_version: now_version(),
        };
        self.insert_capsule_embedding_rows(&[row]).await
    }

    async fn upsert_capability_capsule_embedding_chunks(
        &self,
        capability_capsule_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        vectors: &[Vec<f32>],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        if vectors.is_empty() {
            return Ok(());
        }
        let rv = now_version();
        let rows: Vec<ChCapsuleEmbeddingRow> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| ChCapsuleEmbeddingRow {
                capability_capsule_id: capability_capsule_id.to_owned(),
                tenant: tenant.to_owned(),
                embedding_model: embedding_model.to_owned(),
                embedding_dim,
                embedding: v.clone(),
                content_hash: content_hash.to_owned(),
                source_updated_at: source_updated_at.to_owned(),
                created_at: now.to_owned(),
                updated_at: now.to_owned(),
                chunk_index: i as u32,
                row_version: rv,
            })
            .collect();
        self.insert_capsule_embedding_rows(&rows).await
    }

    async fn delete_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        self.client
            .query(
                "ALTER TABLE capability_capsule_embeddings DELETE \
                 WHERE capability_capsule_id = ?",
            )
            .bind(capability_capsule_id)
            .execute()
            .await
            .map_err(ch_err)
    }

    async fn get_capability_capsule_embedding_row(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM capability_capsule_embeddings \
                 WHERE capability_capsule_id = ? ORDER BY row_version DESC LIMIT 1",
            )
            .bind(capability_capsule_id)
            .fetch_all::<MetaTripleRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows
            .into_iter()
            .next()
            .map(|r| (r.embedding_model, r.content_hash, r.source_updated_at)))
    }

    async fn get_capability_capsule_embedding_vector(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<Vec<f32>>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM capability_capsule_embeddings \
                 WHERE capability_capsule_id = ? ORDER BY row_version DESC LIMIT 1",
            )
            .bind(capability_capsule_id)
            .fetch_all::<VecOnlyRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().next().map(|r| r.embedding))
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
        let vector = decode_f32_blob(embedding_blob, embedding_dim as usize)
            .map_err(|e| StorageError::InvalidInput(format!("embedding blob decode: {e}")))?;
        let row = ChMsgEmbeddingRow {
            message_block_id: message_block_id.to_owned(),
            tenant: tenant.to_owned(),
            embedding_model: embedding_model.to_owned(),
            embedding_dim,
            embedding: vector,
            content_hash: content_hash.to_owned(),
            source_updated_at: source_updated_at.to_owned(),
            created_at: now.to_owned(),
            updated_at: now.to_owned(),
            chunk_index: 0,
            row_version: now_version(),
        };
        self.insert_msg_embedding_rows(&[row]).await
    }

    async fn upsert_conversation_message_embedding_chunks(
        &self,
        message_block_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        vectors: &[Vec<f32>],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        if vectors.is_empty() {
            return Ok(());
        }
        let rv = now_version();
        let rows: Vec<ChMsgEmbeddingRow> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| ChMsgEmbeddingRow {
                message_block_id: message_block_id.to_owned(),
                tenant: tenant.to_owned(),
                embedding_model: embedding_model.to_owned(),
                embedding_dim,
                embedding: v.clone(),
                content_hash: content_hash.to_owned(),
                source_updated_at: source_updated_at.to_owned(),
                created_at: now.to_owned(),
                updated_at: now.to_owned(),
                chunk_index: i as u32,
                row_version: rv,
            })
            .collect();
        self.insert_msg_embedding_rows(&rows).await
    }

    async fn delete_conversation_message_embedding(
        &self,
        message_block_id: &str,
    ) -> Result<(), StorageError> {
        self.client
            .query(
                "ALTER TABLE conversation_message_embeddings DELETE \
                 WHERE message_block_id = ?",
            )
            .bind(message_block_id)
            .execute()
            .await
            .map_err(ch_err)
    }
}
