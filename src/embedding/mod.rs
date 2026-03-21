mod fake;
mod instance;
mod openai;
mod provider;

pub use fake::{deterministic_embedding, FakeEmbeddingProvider};
pub use instance::arc_embedding_provider;
pub use openai::OpenAiEmbeddingProvider;
pub use provider::{EmbeddingError, EmbeddingProvider};
