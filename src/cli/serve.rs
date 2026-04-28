use crate::{app, config, error};
use tracing::info;

/// Entry point for `mem serve` — start the HTTP memory service.
pub async fn run() -> error::Result<()> {
    let config =
        config::Config::from_env().map_err(|e| anyhow::anyhow!("invalid configuration: {e}"))?;
    info!(
        bind_addr = %config.bind_addr,
        db_path = %config.db_path.display(),
        graph_backend = ?config.graph_backend,
        embedding_provider = ?config.embedding.provider,
        embedding_model = %config.embedding.model,
        "mem starting"
    );
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    info!(bind_addr = %config.bind_addr, "mem listening");
    axum::serve(listener, app::router_with_config(config).await?).await?;
    Ok(())
}
