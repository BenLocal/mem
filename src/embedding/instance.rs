use std::sync::Arc;

use crate::config::{EmbeddingProviderKind, EmbeddingSettings};

use super::{EmbeddingError, EmbeddingProvider, FakeEmbeddingProvider, OpenAiEmbeddingProvider};

pub fn arc_embedding_provider(
    settings: &EmbeddingSettings,
) -> Result<Arc<dyn EmbeddingProvider>, EmbeddingError> {
    match settings.provider {
        EmbeddingProviderKind::Fake => Ok(Arc::new(FakeEmbeddingProvider::from_settings(settings))),
        EmbeddingProviderKind::Real => {
            Ok(Arc::new(OpenAiEmbeddingProvider::from_settings(settings)?))
        }
    }
}
