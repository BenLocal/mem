//! Adapter wrapping `crate::embedding::EmbeddingProvider` (async) into
//! `lancedb::embeddings::EmbeddingFunction` (sync) so LanceDB can call
//! mem's existing provider stack when it auto-embeds writes / queries.
//!
//! Why this exists: LanceDB tables can declare a vector column as
//! "auto-embed from this text column" — registering an
//! `EmbeddingFunction` lets `Table::add(text_only_batch)` fill the
//! vector internally, and `Table::vector_search(text_query)` similarly
//! embeds the query string before ANN. Without this adapter we'd have
//! to manually call `provider.embed_text(...)` on every write/query
//! before talking to LanceDB — duplicating logic the DuckDB worker
//! already encapsulates and giving up LanceDB's main ergonomic win.
//!
//! Sync→async bridge: `EmbeddingFunction` methods are sync but
//! `EmbeddingProvider::embed_batch` is async. We use
//! `tokio::task::block_in_place` + `Handle::current().block_on(...)` to
//! bridge — requires a **multi-thread** tokio runtime (LanceDB itself
//! is async-native so its callers always have one in production).
//! Tests that exercise this path must use
//! `#[tokio::test(flavor = "multi_thread")]`.

use std::borrow::Cow;
use std::sync::Arc;

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder};
use arrow_array::{Array, StringArray};
use lancedb::arrow::arrow_schema::{DataType, Field};
use lancedb::embeddings::EmbeddingFunction;
use lancedb::Error as LanceError;
use lancedb::Result as LanceResult;

use crate::embedding::EmbeddingProvider;

/// `lancedb::embeddings::EmbeddingFunction` wrapping mem's
/// `EmbeddingProvider`. Constructed once at `LanceDbRepository::open_*`
/// time and registered with the connection's
/// `lancedb::embeddings::MemoryRegistry` under the name
/// `"<provider>-<model>"`.
pub(super) struct ProviderEmbeddingFunction {
    provider: Arc<dyn EmbeddingProvider>,
    /// Cached `format!("{}-{}", provider.name(), provider.model())` —
    /// the `name()` method on `EmbeddingFunction` returns `&str` so the
    /// owning value must outlive the borrow.
    name: String,
    /// Cached `provider.dim() as i32` — `FixedSizeList` requires i32.
    dim: i32,
}

impl std::fmt::Debug for ProviderEmbeddingFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEmbeddingFunction")
            .field("name", &self.name)
            .field("dim", &self.dim)
            .finish_non_exhaustive()
    }
}

impl ProviderEmbeddingFunction {
    pub fn new(provider: Arc<dyn EmbeddingProvider>) -> Self {
        let name = format!("{}-{}", provider.name(), provider.model());
        let dim = i32::try_from(provider.dim())
            .expect("embedding provider dim must fit in i32 for FixedSizeList");
        Self {
            provider,
            name,
            dim,
        }
    }

    fn embed_strings(&self, source: Arc<dyn Array>) -> LanceResult<Arc<dyn Array>> {
        let strs: &StringArray =
            source
                .as_any()
                .downcast_ref()
                .ok_or_else(|| LanceError::Schema {
                    message: format!(
                        "EmbeddingFunction expects Utf8 source, got {:?}",
                        source.data_type(),
                    ),
                })?;
        let texts: Vec<&str> = (0..strs.len()).map(|i| strs.value(i)).collect();

        // Sync→async bridge. `block_in_place` lets the current worker
        // park while the async work runs on the same multi-thread
        // runtime — see module-level docs for the runtime requirement.
        let batch_results = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.provider.embed_batch(&texts))
        })
        .map_err(|e| LanceError::Runtime {
            message: format!("embed_batch: {e}"),
        })?;

        if batch_results.len() != texts.len() {
            return Err(LanceError::Runtime {
                message: format!(
                    "embed_batch returned {} vectors for {} inputs",
                    batch_results.len(),
                    texts.len(),
                ),
            });
        }

        let mut builder =
            FixedSizeListBuilder::with_capacity(Float32Builder::new(), self.dim, texts.len());
        for (i, item) in batch_results.into_iter().enumerate() {
            let vec = item.map_err(|e| LanceError::Runtime {
                message: format!("embed_batch[{i}]: {e}"),
            })?;
            if i32::try_from(vec.len()).unwrap_or(-1) != self.dim {
                return Err(LanceError::Runtime {
                    message: format!(
                        "embed_batch[{i}] dim mismatch: got {} expected {}",
                        vec.len(),
                        self.dim,
                    ),
                });
            }
            for v in vec {
                builder.values().append_value(v);
            }
            builder.append(true);
        }
        Ok(Arc::new(builder.finish()))
    }
}

impl EmbeddingFunction for ProviderEmbeddingFunction {
    fn name(&self) -> &str {
        &self.name
    }

    fn source_type(&self) -> LanceResult<Cow<'_, DataType>> {
        Ok(Cow::Owned(DataType::Utf8))
    }

    fn dest_type(&self) -> LanceResult<Cow<'_, DataType>> {
        Ok(Cow::Owned(DataType::FixedSizeList(
            Arc::new(Field::new("item", DataType::Float32, true)),
            self.dim,
        )))
    }

    fn compute_source_embeddings(&self, source: Arc<dyn Array>) -> LanceResult<Arc<dyn Array>> {
        self.embed_strings(source)
    }

    fn compute_query_embeddings(&self, input: Arc<dyn Array>) -> LanceResult<Arc<dyn Array>> {
        self.embed_strings(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::FakeEmbeddingProvider;
    use arrow_array::FixedSizeListArray;

    /// `compute_query_embeddings` must round-trip a batch of strings into
    /// a `FixedSizeList<Float32, dim>` of correct length. Exercises the
    /// async→sync bridge via `block_in_place` — must run on a
    /// multi-thread runtime.
    #[tokio::test(flavor = "multi_thread")]
    async fn provider_embedding_function_compute_query_embeddings() {
        let provider: Arc<dyn EmbeddingProvider> =
            Arc::new(FakeEmbeddingProvider::new("test-model", 8));
        let func = ProviderEmbeddingFunction::new(provider);

        // sanity: name + types
        assert!(func.name().contains("fake"));
        assert!(matches!(
            func.source_type().unwrap().as_ref(),
            DataType::Utf8
        ));
        let dest = func.dest_type().unwrap();
        match dest.as_ref() {
            DataType::FixedSizeList(_, n) => assert_eq!(*n, 8),
            other => panic!("expected FixedSizeList<.., 8>, got {other:?}"),
        }

        // round-trip
        let inputs: Arc<dyn Array> = Arc::new(StringArray::from(vec!["hello", "world", "x"]));
        let out = func.compute_query_embeddings(inputs).unwrap();
        let fsl = out
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .expect("output should be FixedSizeList");
        assert_eq!(fsl.len(), 3);
        assert_eq!(fsl.value_length(), 8);
    }
}
