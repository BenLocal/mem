use mem::{app, config, error};

#[tokio::main]
async fn main() -> error::Result<()> {
    let config =
        config::Config::from_env().map_err(|e| anyhow::anyhow!("invalid configuration: {e}"))?;
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    axum::serve(listener, app::router_with_config(config).await?).await?;
    Ok(())
}
