mod app;
mod config;
mod error;
mod http;

#[tokio::main]
async fn main() -> error::Result<()> {
    let config = config::Config::local();
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    axum::serve(listener, app::router()).await?;
    Ok(())
}
