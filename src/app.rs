use axum::Router;

use crate::{config::Config, http, service::MemoryService, storage::DuckDbRepository};

#[derive(Clone)]
pub struct AppState {
    pub memory_service: MemoryService,
}

impl AppState {
    pub async fn local() -> anyhow::Result<Self> {
        let config = Config::local();
        let repository = DuckDbRepository::open(&config.db_path).await?;

        Ok(Self {
            memory_service: MemoryService::new(repository),
        })
    }
}

pub async fn router() -> anyhow::Result<Router> {
    let state = AppState::local().await?;
    Ok(http::router().with_state(state))
}
