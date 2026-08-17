use crate::error;
use clap::{Args, ValueEnum};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpProfileArg {
    Default,
    Compiler,
}

#[derive(Debug, Args)]
pub struct McpArgs {
    #[arg(long, value_enum, default_value_t = McpProfileArg::Default)]
    pub profile: McpProfileArg,
}

/// Entry point for `mem mcp` — run the MCP (Model Context Protocol) stdio server.
///
/// This is a thin CLI wrapper around the protocol implementation in [`crate::mcp`].
pub async fn run(args: McpArgs) -> error::Result<()> {
    let profile = match args.profile {
        McpProfileArg::Default => crate::mcp::McpProfile::Default,
        McpProfileArg::Compiler => crate::mcp::McpProfile::Compiler,
    };
    crate::mcp::run(profile).await
}
