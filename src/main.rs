use clap::{Parser, Subcommand};
use mem::{app, config, error, mcp};
use tracing::info;
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
}

#[tokio::main]
async fn main() -> error::Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve);

    init_tracing(matches!(command, Command::Mcp));

    match command {
        Command::Serve => run_serve().await,
        Command::Mcp => mcp::run().await,
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

async fn run_serve() -> error::Result<()> {
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
