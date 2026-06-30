use clap::{Parser, Subcommand};
use holmes_cli::{chat, setup, tui};
use holmes_harness::{HarnessRunner, HarnessScenario};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "holmes", about = "Holmes — AI-powered security research agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Resume a previous session
    #[arg(short, long)]
    resume: Option<String>,

    /// Continue the most recent session
    #[arg(short, long)]
    r#continue: bool,

    /// One-shot query (non-interactive)
    #[arg(short, long)]
    query: Option<String>,

    /// Start the legacy line REPL instead of the default full-screen TUI
    #[arg(long)]
    repl: bool,

    /// Start the full-screen TUI explicitly
    #[arg(long)]
    tui: bool,

    /// Model to use
    #[arg(short, long)]
    model: Option<String>,

    /// Session mode
    #[arg(long, default_value = "pentest")]
    mode: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Start interactive chat (full-screen TUI by default)
    Chat {
        /// Start the legacy line REPL instead of the default full-screen TUI
        #[arg(long)]
        repl: bool,
        /// Start the full-screen TUI explicitly
        #[arg(long)]
        tui: bool,
    },
    /// Start full-screen TUI
    Tui,
    /// Start legacy line REPL
    Repl,
    /// List recent sessions
    Sessions,
    /// Configure LLM provider (interactive wizard)
    Setup,
    /// Run a deterministic Holmes harness scenario
    Harness { scenario: PathBuf },
    /// Show version
    Version,
}

fn holmes_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("holmes")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        None if cli.query.is_some() || cli.repl => {
            chat::run_chat(cli.resume, cli.r#continue, cli.query, cli.model, cli.mode).await?;
        }
        None => {
            tui::run_tui(cli.resume, cli.r#continue, cli.model, cli.mode).await?;
        }
        Some(Commands::Chat {
            repl: chat_repl,
            tui: _chat_tui,
        }) => {
            if cli.query.is_some() || cli.repl || chat_repl {
                chat::run_chat(cli.resume, cli.r#continue, cli.query, cli.model, cli.mode).await?;
            } else {
                tui::run_tui(cli.resume, cli.r#continue, cli.model, cli.mode).await?;
            }
        }
        Some(Commands::Tui) => {
            if cli.query.is_some() {
                eprintln!("tui is interactive; ignoring --query and starting the TUI.");
            }
            tui::run_tui(cli.resume, cli.r#continue, cli.model, cli.mode).await?;
        }
        Some(Commands::Repl) => {
            chat::run_chat(cli.resume, cli.r#continue, cli.query, cli.model, cli.mode).await?;
        }
        Some(Commands::Sessions) => {
            chat::list_sessions().await?;
        }
        Some(Commands::Setup) => {
            let data_dir = holmes_data_dir();
            setup::run_setup(&data_dir)?;
        }
        Some(Commands::Harness { scenario }) => {
            let scenario = HarnessScenario::from_path(&scenario)?;
            let report = HarnessRunner::new().run(scenario).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.success {
                std::process::exit(1);
            }
        }
        Some(Commands::Version) => {
            println!("Holmes v{}", env!("CARGO_PKG_VERSION"));
        }
    }
    Ok(())
}
