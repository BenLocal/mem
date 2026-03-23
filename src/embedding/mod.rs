mod embed_anything;
mod fake;
mod instance;
mod openai;
mod provider;

pub use embed_anything::EmbedAnythingEmbeddingProvider;
pub use fake::{deterministic_embedding, FakeEmbeddingProvider};
pub use instance::arc_embedding_provider;
pub use openai::OpenAiEmbeddingProvider;
pub use provider::{EmbeddingError, EmbeddingProvider};
