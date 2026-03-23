use mem::{app, config, error};
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> error::Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(env_filter).with_target(false).init();

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
