use clap::{Parser, Subcommand};
use mem::error;
use tracing_subscriber::{fmt, EnvFilter};

// jemalloc as the global allocator — replaces glibc malloc, whose per-thread
// arenas ratcheted RSS on this many-core box and never returned freed memory
// to the OS. Covers all Rust-side allocations (the embedding inference churn).
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

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
    /// Claude Code hook entry points. Each subcommand reads the hook's
    /// JSON payload on stdin and prints the hook-output envelope (or `{}`).
    /// The shell hooks `exec mem hook <event>` instead of parsing payloads
    /// in jq. Always exits 0 — a hook must not block the user's work.
    #[command(subcommand)]
    Hook(mem::cli::hook::HookCommand),
    /// Query and format memories for session start injection.
    WakeUp(mem::cli::wake_up::WakeUpArgs),
    /// Scan a transcript and POST `applies_here` feedback for memories
    /// whose retrieved text was referenced in subsequent assistant
    /// blocks. Wired into the Stop / PreCompact hooks so the lifecycle
    /// signals close even when the agent forgets to call `capability_capsule_feedback`.
    FeedbackFromTranscript(mem::cli::feedback::FeedbackFromTranscriptArgs),
}

fn main() -> error::Result<()> {
    // Turn on jemalloc's background purge thread so decayed/idle memory is
    // returned to the OS on a timer. It defaults OFF — without it jemalloc
    // only purges on allocation activity, so a quiet `mem serve` would ratchet
    // RSS much like glibc did. jemalloc's default decay (dirty 10s) handles the
    // rest. Best-effort: `background_thread` is unsupported on some musl builds,
    // where this no-ops (decay still works, just lazily).
    let _ = tikv_jemalloc_ctl::background_thread::write(true);

    // Build the runtime explicitly instead of `#[tokio::main]` so we can
    // cap `max_blocking_threads`. tokio's default is 512; on a many-core
    // box mem's heavy `spawn_blocking` load (local embedding inference is
    // blocking) balloons the blocking pool toward that ceiling. A long-
    // lived `mem serve` was holding 500-800 `tokio-rt-worker` threads
    // (~11 GB RSS) with periodic CPU spikes — confirmed via `kernel_clone`
    // tracing. A large blocking pool buys no throughput for the inference
    // workload; it just piles up idle thread stacks. 32 is plenty for the
    // embedding inference load.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(32)
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> error::Result<()> {
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
        Command::Hook(command) => {
            let code = mem::cli::hook::run(command).await;
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
