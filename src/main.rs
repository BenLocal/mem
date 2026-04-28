use clap::{Parser, Subcommand};
use mem::error;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Debug, Parser)]
#[command(
    name = "mem",
    version,
    about = "Local-first memory service for multi-agent workflows"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP memory service (default).
    Serve,
    /// Run the MCP (Model Context Protocol) stdio server.
    Mcp,
    /// Diagnose or rebuild the vector index sidecar.
    Repair(mem::cli::repair::RepairArgs),
}

#[tokio::main]
async fn main() -> error::Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve);

    init_tracing(matches!(command, Command::Mcp | Command::Repair(_)));

    match command {
        Command::Serve => mem::cli::serve::run().await,
        Command::Mcp => mem::cli::mcp::run().await,
        Command::Repair(args) => {
            let code = mem::cli::repair::run(args).await;
            std::process::exit(code);
        }
    }
}

fn init_tracing(stdio_protocol: bool) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = fmt().with_env_filter(env_filter).with_target(false);
    if stdio_protocol {
        // MCP uses stdout for JSON-RPC framing; logs MUST go to stderr.
        builder.with_writer(std::io::stderr).with_ansi(false).init();
    } else {
        builder.init();
    }
}
