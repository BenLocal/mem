use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;

static APP_DB_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProviderKind {
    Fake,
    Real,
}

#[derive(Debug, Clone)]
pub struct EmbeddingSettings {
    pub provider: EmbeddingProviderKind,
    pub model: String,
    pub dim: usize,
    pub worker_poll_interval_ms: u64,
    pub max_retries: u32,
    pub batch_size: usize,
    pub openai_api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub db_path: PathBuf,
    pub embedding: EmbeddingSettings,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid EMBEDDING_PROVIDER: {0} (expected fake or real)")]
    InvalidEmbeddingProvider(String),
    #[error("OPENAI_API_KEY is required when EMBEDDING_PROVIDER=real")]
    MissingOpenAiApiKey,
    #[error("invalid EMBEDDING_DIM: {0}")]
    InvalidEmbeddingDim(String),
    #[error("invalid EMBEDDING_WORKER_POLL_INTERVAL_MS: {0}")]
    InvalidPollInterval(String),
    #[error("invalid EMBEDDING_MAX_RETRIES: {0}")]
    InvalidMaxRetries(String),
    #[error("invalid EMBEDDING_BATCH_SIZE: {0}")]
    InvalidBatchSize(String),
}

impl EmbeddingSettings {
    pub fn development_defaults() -> Self {
        Self {
            provider: EmbeddingProviderKind::Fake,
            model: "fake".to_string(),
            dim: 256,
            worker_poll_interval_ms: 1000,
            // Failure attempts allowed before permanent `failed` (initial pending try + retries).
            max_retries: 4,
            batch_size: 1,
            openai_api_key: None,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();

        if let Some(value) = get("EMBEDDING_PROVIDER") {
            s.provider = match value.to_ascii_lowercase().as_str() {
                "fake" => EmbeddingProviderKind::Fake,
                "real" => EmbeddingProviderKind::Real,
                other => return Err(ConfigError::InvalidEmbeddingProvider(other.to_string())),
            };
        }

        if let Some(model) = get("EMBEDDING_MODEL") {
            if !model.is_empty() {
                s.model = model;
            }
        }

        if let Some(raw) = get("EMBEDDING_DIM") {
            let dim: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidEmbeddingDim(raw.clone()))?;
            if dim == 0 {
                return Err(ConfigError::InvalidEmbeddingDim(raw));
            }
            s.dim = dim;
        }

        if let Some(raw) = get("EMBEDDING_WORKER_POLL_INTERVAL_MS") {
            let ms: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidPollInterval(raw.clone()))?;
            if ms == 0 {
                return Err(ConfigError::InvalidPollInterval(raw));
            }
            s.worker_poll_interval_ms = ms;
        }

        if let Some(raw) = get("EMBEDDING_MAX_RETRIES") {
            let n: u32 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidMaxRetries(raw.clone()))?;
            s.max_retries = n;
        }

        if let Some(raw) = get("EMBEDDING_BATCH_SIZE") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidBatchSize(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidBatchSize(raw));
            }
            s.batch_size = n;
        }

        if let Some(key) = get("OPENAI_API_KEY") {
            if !key.is_empty() {
                s.openai_api_key = Some(key);
            }
        }

        if s.provider == EmbeddingProviderKind::Real {
            if s.openai_api_key.as_deref().unwrap_or("").is_empty() {
                return Err(ConfigError::MissingOpenAiApiKey);
            }
        }

        Ok(s)
    }

    /// Stored on `embedding_jobs.provider` to dedupe work; matches configured backend.
    pub fn job_provider_id(&self) -> &'static str {
        match self.provider {
            EmbeddingProviderKind::Fake => "fake",
            EmbeddingProviderKind::Real => "openai",
        }
    }
}

impl Config {
    pub fn local() -> Self {
        Self {
            bind_addr: "127.0.0.1:3000".to_string(),
            db_path: default_db_path(),
            embedding: EmbeddingSettings::development_defaults(),
        }
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
        Ok(Self {
            bind_addr,
            db_path: default_db_path(),
            embedding: EmbeddingSettings::from_env_vars(|k| std::env::var(k).ok())?,
        })
    }
}

fn default_db_path() -> PathBuf {
    if let Ok(path) = std::env::var("MEM_DB_PATH") {
        return PathBuf::from(path);
    }

    let sequence = APP_DB_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mem-app-{}-{sequence}.duckdb", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            map.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn embedding_defaults_when_empty() {
        let s = EmbeddingSettings::from_env_vars(|_| None).unwrap();
        assert_eq!(s.provider, EmbeddingProviderKind::Fake);
        assert_eq!(s.model, "fake");
        assert_eq!(s.dim, 256);
        assert_eq!(s.openai_api_key, None);
    }

    #[test]
    fn embedding_real_requires_api_key() {
        let err = EmbeddingSettings::from_env_vars(env(&[("EMBEDDING_PROVIDER", "real")])).unwrap_err();
        assert!(matches!(err, ConfigError::MissingOpenAiApiKey));
    }

    #[test]
    fn embedding_real_with_key_ok() {
        let s = EmbeddingSettings::from_env_vars(env(&[
            ("EMBEDDING_PROVIDER", "real"),
            ("OPENAI_API_KEY", "sk-test"),
            ("EMBEDDING_MODEL", "text-embedding-3-small"),
        ]))
        .unwrap();
        assert_eq!(s.provider, EmbeddingProviderKind::Real);
        assert_eq!(s.model, "text-embedding-3-small");
        assert_eq!(s.openai_api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn embedding_fake_ignores_empty_openai_key() {
        let s = EmbeddingSettings::from_env_vars(env(&[
            ("EMBEDDING_PROVIDER", "fake"),
            ("OPENAI_API_KEY", ""),
        ]))
        .unwrap();
        assert_eq!(s.provider, EmbeddingProviderKind::Fake);
        assert_eq!(s.openai_api_key, None);
    }
}
