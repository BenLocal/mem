use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static APP_DB_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub db_path: PathBuf,
}

impl Config {
    pub fn local() -> Self {
        Self {
            bind_addr: "127.0.0.1:3000".to_string(),
            db_path: default_db_path(),
        }
    }
}

fn default_db_path() -> PathBuf {
    if let Ok(path) = std::env::var("MEM_DB_PATH") {
        return PathBuf::from(path);
    }

    let sequence = APP_DB_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mem-app-{}-{sequence}.duckdb", std::process::id()))
}
