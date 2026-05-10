//! Shared CLI arg group for subcommands that talk to a running
//! `mem serve` over HTTP. Used by `mine`, `feedback-from-transcript`,
//! and `wake-up`. Each subcommand flattens this into its own
//! `Args` struct via `#[command(flatten)]`, so the user sees the
//! same `--tenant` / `--base-url` flags everywhere with the same
//! defaults and env-var fallback.

use clap::Args;

/// Remote-service connection parameters. The `env = "MEM_..."` clauses
/// make every subcommand pick up the same env-var fallback as the MCP
/// server (`mcp::run` reads `MEM_BASE_URL` / `MEM_TENANT`), so a single
/// shell-level export propagates to all surfaces.
///
/// Precedence: explicit CLI flag > env var > default.
#[derive(Debug, Clone, Args)]
pub struct RemoteArgs {
    /// Tenant identifier scoped to the request body.
    #[arg(long, env = "MEM_TENANT", default_value = "local")]
    pub tenant: String,

    /// Base URL of the local `mem serve` HTTP service.
    #[arg(long, env = "MEM_BASE_URL", default_value = "http://127.0.0.1:3000")]
    pub base_url: String,
}
