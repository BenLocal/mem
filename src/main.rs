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
    /// Scaffold a new `.mem/` directory with mode-based env defaults +
    /// a taxonomy starter file. First-run UX — analogous to mempalace's
    /// `onboarding.py`.
    Init(mem::cli::init::InitArgs),
    /// Mine memories from Claude Code transcript.
    Mine(mem::cli::mine::MineArgs),
    /// Query and format memories for session start injection.
    WakeUp(mem::cli::wake_up::WakeUpArgs),
    /// Scan a transcript and POST `applies_here` feedback for memories
    /// whose retrieved text was referenced in subsequent assistant
    /// blocks. Wired into the Stop / PreCompact hooks so the lifecycle
    /// signals close even when the agent forgets to call `capability_capsule_feedback`.
    FeedbackFromTranscript(mem::cli::feedback::FeedbackFromTranscriptArgs),
}

#[tokio::main]
async fn main() -> error::Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve);

    init_tracing(matches!(command, Command::Mcp));

    match command {
        Command::Serve => mem::cli::serve::run().await,
        Command::Mcp => mem::cli::mcp::run().await,
        Command::Init(args) => {
            let code = mem::cli::init::run(args);
            std::process::exit(code);
        }
        Command::Mine(args) => {
            let code = mem::cli::mine::run(args).await;
            std::process::exit(code);
        }
        Command::WakeUp(args) => match mem::cli::wake_up::run(args).await {
            Ok(output) => {
                print!("{}", output);
                Ok(())
            }
            Err(e) => {
                eprintln!("Failed to wake up: {}", e);
                std::process::exit(1);
            }
        },
        Command::FeedbackFromTranscript(args) => {
            let code = mem::cli::feedback::run(args).await;
            std::process::exit(code);
        }
    }
}

fn init_tracing(stdio_protocol: bool) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,\
             lance=warn,lance_core=warn,lance_io=warn,lance_table=warn,\
             lance_index=warn,lance_encoding=warn,lance_file=warn,\
             lance_datafusion=warn,lance_arrow=warn,lancedb=warn,\
             datafusion=warn",
        )
    });
    let builder = fmt().with_env_filter(env_filter).with_target(false);
    if stdio_protocol {
        // MCP uses stdout for JSON-RPC framing; logs MUST go to stderr.
        builder.with_writer(std::io::stderr).with_ansi(false).init();
    } else {
        builder.init();
    }
}
