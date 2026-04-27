use std::env;

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub base_url: String,
    pub default_tenant: String,
    pub expose_embeddings: bool,
}

impl McpConfig {
    pub fn from_env() -> Self {
        let base_url = env::var("MEM_BASE_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());

        let default_tenant = env::var("MEM_TENANT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "local".to_string());

        let expose_embeddings = matches!(env::var("MEM_MCP_EXPOSE_EMBEDDINGS").as_deref(), Ok("1"));

        Self {
            base_url,
            default_tenant,
            expose_embeddings,
        }
    }
}
