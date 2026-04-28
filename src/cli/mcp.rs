use crate::error;

/// Entry point for `mem mcp` — run the MCP (Model Context Protocol) stdio server.
///
/// This is a thin CLI wrapper around the protocol implementation in [`crate::mcp`].
pub async fn run() -> error::Result<()> {
    crate::mcp::run().await
}
