pub mod client;
pub mod compiler;
pub mod config;
pub mod result;
pub mod server;

use anyhow::Result;
use rmcp::{transport::stdio, ServiceExt};
use tracing::info;

pub use config::{McpConfig, McpProfile};
pub use server::MemMcpServer;

pub async fn run(profile: McpProfile) -> Result<()> {
    let config = McpConfig::from_env(profile);
    info!(
        base_url = %config.base_url,
        default_tenant = %config.default_tenant,
        expose_embeddings = config.expose_embeddings,
        profile = ?config.profile,
        "mem-mcp stdio server starting"
    );

    let server = MemMcpServer::new(config)?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
