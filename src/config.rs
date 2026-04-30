use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;

static APP_DB_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProviderKind {
    Fake,
    OpenAi,
    EmbedAnything,
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
    pub vector_index_flush_every: usize,
    /// Read directly via `std::env::var` in `DuckDbRepository::semantic_search_memories`.
    /// This struct field is populated for diagnostic/logging visibility but is not the
    /// source of truth at search time. See `mempalace-diff §8 #3` Task 14 carryover.
    #[allow(dead_code)]
    pub vector_index_oversample: usize,
    /// Read directly via `std::env::var` in `DuckDbRepository::semantic_search_memories`.
    /// This struct field is populated for diagnostic/logging visibility but is not the
    /// source of truth at search time. See `mempalace-diff §8 #3` Task 14 carryover.
    #[allow(dead_code)]
    pub vector_index_use_legacy: bool,
    /// When `true`, `app.rs` skips spawning the transcript embedding worker.
    /// Set via `MEM_TRANSCRIPT_EMBED_DISABLED` (`"1"` or `"true"`,
    /// case-insensitive). Used by the cli/mine.rs offline pipeline and tests
    /// that want transcript ingest without a background worker.
    pub transcript_disabled: bool,
    /// Sidecar flush cadence (writes between persists) for the transcript HNSW
    /// index. Set via `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY`. Defaults to
    /// `256`; `0` is rejected at parse time and falls back to the default.
    pub transcript_vector_index_flush_every: usize,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub db_path: PathBuf,
    pub embedding: EmbeddingSettings,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid EMBEDDING_PROVIDER: {0} (expected fake, openai, or embedanything)")]
    InvalidEmbeddingProvider(String),
    #[error("OPENAI_API_KEY is required when EMBEDDING_PROVIDER=openai (or real alias)")]
    MissingOpenAiApiKey,
    #[error("invalid EMBEDDING_DIM: {0}")]
    InvalidEmbeddingDim(String),
    #[error("invalid EMBEDDING_WORKER_POLL_INTERVAL_MS: {0}")]
    InvalidPollInterval(String),
    #[error("invalid EMBEDDING_MAX_RETRIES: {0}")]
    InvalidMaxRetries(String),
    #[error("invalid EMBEDDING_BATCH_SIZE: {0}")]
    InvalidBatchSize(String),
    #[error("invalid MEM_VECTOR_INDEX_FLUSH_EVERY: {0}")]
    InvalidVectorIndexFlushEvery(String),
    #[error("invalid MEM_VECTOR_INDEX_OVERSAMPLE: {0}")]
    InvalidVectorIndexOversample(String),
    #[error("invalid MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY: {0}")]
    InvalidTranscriptVectorIndexFlushEvery(String),
}

impl EmbeddingSettings {
    pub fn development_defaults() -> Self {
        Self {
            provider: EmbeddingProviderKind::EmbedAnything,
            model: "Qwen/Qwen3-Embedding-0.6B".to_string(),
            dim: 1024,
            worker_poll_interval_ms: 1000,
            // Failure attempts allowed before permanent `failed` (initial pending try + retries).
            max_retries: 4,
            batch_size: 1,
            openai_api_key: None,
            vector_index_flush_every: 100,
            vector_index_oversample: 4,
            vector_index_use_legacy: false,
            transcript_disabled: false,
            transcript_vector_index_flush_every: 256,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();

        if let Some(value) = get("EMBEDDING_PROVIDER") {
            s.provider = match value.to_ascii_lowercase().as_str() {
                "fake" => EmbeddingProviderKind::Fake,
                "real" | "openai" => EmbeddingProviderKind::OpenAi,
                "embedanything" | "embed_anything" => EmbeddingProviderKind::EmbedAnything,
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

        if s.provider == EmbeddingProviderKind::OpenAi
            && s.openai_api_key.as_deref().unwrap_or("").is_empty()
        {
            return Err(ConfigError::MissingOpenAiApiKey);
        }

        if let Some(raw) = get("MEM_VECTOR_INDEX_FLUSH_EVERY") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidVectorIndexFlushEvery(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidVectorIndexFlushEvery(raw));
            }
            s.vector_index_flush_every = n;
        }
        if let Some(raw) = get("MEM_VECTOR_INDEX_OVERSAMPLE") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidVectorIndexOversample(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidVectorIndexOversample(raw));
            }
            s.vector_index_oversample = n;
        }
        if let Some(raw) = get("MEM_VECTOR_INDEX_USE_LEGACY") {
            s.vector_index_use_legacy = matches!(raw.as_str(), "1" | "true" | "yes");
        }

        if let Some(raw) = get("MEM_TRANSCRIPT_EMBED_DISABLED") {
            s.transcript_disabled =
                matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }

        if let Some(raw) = get("MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidTranscriptVectorIndexFlushEvery(raw.clone()))?;
            // `0` is meaningless (would flush on every write and never amortize
            // disk I/O); reject loudly rather than silently falling back, to
            // keep parity with `MEM_VECTOR_INDEX_FLUSH_EVERY`'s contract.
            if n == 0 {
                return Err(ConfigError::InvalidTranscriptVectorIndexFlushEvery(raw));
            }
            s.transcript_vector_index_flush_every = n;
        }

        Ok(s)
    }

    /// Stored on `embedding_jobs.provider` to dedupe work; matches configured backend.
    pub fn job_provider_id(&self) -> &'static str {
        match self.provider {
            EmbeddingProviderKind::Fake => "fake",
            EmbeddingProviderKind::OpenAi => "openai",
            EmbeddingProviderKind::EmbedAnything => "embedanything",
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

    if let Some(home) = std::env::var_os("HOME") {
        let mem_dir = PathBuf::from(home).join(".mem");
        if std::fs::create_dir_all(&mem_dir).is_ok() {
            return mem_dir.join("mem.duckdb");
        }
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
        let err =
            EmbeddingSettings::from_env_vars(env(&[("EMBEDDING_PROVIDER", "real")])).unwrap_err();
        assert!(matches!(err, ConfigError::MissingOpenAiApiKey));
    }

    #[test]
    fn embedding_real_with_key_ok() {
        let s = EmbeddingSettings::from_env_vars(env(&[
            ("EMBEDDING_PROVIDER", "openai"),
            ("OPENAI_API_KEY", "sk-test"),
            ("EMBEDDING_MODEL", "text-embedding-3-small"),
        ]))
        .unwrap();
        assert_eq!(s.provider, EmbeddingProviderKind::OpenAi);
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

    #[test]
    fn embedding_embedanything_ok_without_openai_key() {
        let s = EmbeddingSettings::from_env_vars(env(&[
            ("EMBEDDING_PROVIDER", "embedanything"),
            ("EMBEDDING_MODEL", "sentence-transformers/all-MiniLM-L6-v2"),
            ("EMBEDDING_DIM", "384"),
        ]))
        .unwrap();
        assert_eq!(s.provider, EmbeddingProviderKind::EmbedAnything);
        assert_eq!(s.openai_api_key, None);
        assert_eq!(s.job_provider_id(), "embedanything");
    }

    #[test]
    fn vector_index_settings_have_defaults() {
        let s = EmbeddingSettings::from_env_vars(|_| None).unwrap();
        assert_eq!(s.vector_index_flush_every, 100);
        assert_eq!(s.vector_index_oversample, 4);
        assert!(!s.vector_index_use_legacy);
    }

    #[test]
    fn vector_index_settings_read_from_env() {
        let s = EmbeddingSettings::from_env_vars(env(&[
            ("MEM_VECTOR_INDEX_FLUSH_EVERY", "50"),
            ("MEM_VECTOR_INDEX_OVERSAMPLE", "8"),
            ("MEM_VECTOR_INDEX_USE_LEGACY", "1"),
        ]))
        .unwrap();
        assert_eq!(s.vector_index_flush_every, 50);
        assert_eq!(s.vector_index_oversample, 8);
        assert!(s.vector_index_use_legacy);
    }

    #[test]
    fn transcript_embed_disabled_default_false() {
        let s = EmbeddingSettings::from_env_vars(|_| None).unwrap();
        assert!(!s.transcript_disabled);
    }

    #[test]
    fn transcript_embed_disabled_accepts_truthy_values() {
        for raw in ["1", "true", "TRUE", "True", "yes", "Yes", "YES"] {
            let s =
                EmbeddingSettings::from_env_vars(env(&[("MEM_TRANSCRIPT_EMBED_DISABLED", raw)]))
                    .unwrap_or_else(|e| panic!("parse failed for {raw:?}: {e}"));
            assert!(
                s.transcript_disabled,
                "expected MEM_TRANSCRIPT_EMBED_DISABLED={raw:?} to enable transcript_disabled"
            );
        }
    }

    #[test]
    fn transcript_embed_disabled_falsy_values_stay_disabled() {
        // Anything that isn't 1/true/yes (case-insensitive) leaves the flag false.
        for raw in ["0", "false", "no", ""] {
            let s =
                EmbeddingSettings::from_env_vars(env(&[("MEM_TRANSCRIPT_EMBED_DISABLED", raw)]))
                    .unwrap();
            assert!(
                !s.transcript_disabled,
                "expected MEM_TRANSCRIPT_EMBED_DISABLED={raw:?} to leave transcript_disabled=false"
            );
        }
    }

    #[test]
    fn transcript_vector_index_flush_every_default_256() {
        let s = EmbeddingSettings::from_env_vars(|_| None).unwrap();
        assert_eq!(s.transcript_vector_index_flush_every, 256);
    }

    #[test]
    fn transcript_vector_index_flush_every_parses_positive() {
        let s = EmbeddingSettings::from_env_vars(env(&[(
            "MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY",
            "512",
        )]))
        .unwrap();
        assert_eq!(s.transcript_vector_index_flush_every, 512);
    }

    #[test]
    fn transcript_vector_index_flush_every_rejects_zero() {
        let err = EmbeddingSettings::from_env_vars(env(&[(
            "MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY",
            "0",
        )]))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidTranscriptVectorIndexFlushEvery(ref s) if s == "0"
        ));
    }

    #[test]
    fn transcript_vector_index_flush_every_rejects_non_numeric() {
        let err = EmbeddingSettings::from_env_vars(env(&[(
            "MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY",
            "abc",
        )]))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidTranscriptVectorIndexFlushEvery(ref s) if s == "abc"
        ));
    }
}
