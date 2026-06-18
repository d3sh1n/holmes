use clap::{Parser, Subcommand};
use holmes_cli::{chat, setup};
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

    /// Model to use
    #[arg(short, long)]
    model: Option<String>,

    /// Session mode
    #[arg(long, default_value = "pentest")]
    mode: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Start interactive chat (default)
    Chat,
    /// List recent sessions
    Sessions,
    /// Configure LLM provider (interactive wizard)
    Setup,
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

    match cli.command.unwrap_or(Commands::Chat) {
        Commands::Chat => {
            chat::run_chat(
                cli.resume, cli.r#continue, cli.query, cli.model, cli.mode,
            ).await?;
        }
        Commands::Sessions => {
            chat::list_sessions().await?;
        }
        Commands::Setup => {
            let data_dir = holmes_data_dir();
            setup::run_setup(&data_dir)?;
        }
        Commands::Version => {
            println!("Holmes v{}", env!("CARGO_PKG_VERSION"));
        }
    }
    Ok(())
}
