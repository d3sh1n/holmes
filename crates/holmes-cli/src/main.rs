use clap::{Parser, Subcommand};
use holmes_cli::{chat, inline_ui, setup, tui};
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

    /// Start the legacy line REPL instead of the default inline TUI
    #[arg(long)]
    repl: bool,

    /// Start the legacy full-screen TUI explicitly
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
    /// Start interactive chat (inline TUI by default)
    Chat {
        /// Start the legacy line REPL instead of the default inline TUI
        #[arg(long)]
        repl: bool,
        /// Start the legacy full-screen TUI explicitly
        #[arg(long)]
        tui: bool,
    },
    /// Start the legacy full-screen TUI
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
    let cli = Cli::parse();
    // Interactive terminal UIs (inline/classic TUI, REPL) own the screen — tracing logs
    // written to stderr would corrupt the display (interleave with the ratatui viewport).
    // Route logs to a file for those; keep stderr for one-shot / non-interactive commands.
    init_tracing(is_interactive_session(&cli));

    match cli.command {
        None if cli.query.is_some() || cli.repl => {
            chat::run_chat(cli.resume, cli.r#continue, cli.query, cli.model, cli.mode).await?;
        }
        None => {
            launch_ui(cli.resume, cli.r#continue, cli.model, cli.mode).await?;
        }
        Some(Commands::Chat {
            repl: chat_repl,
            tui: _chat_tui,
        }) => {
            if cli.query.is_some() || cli.repl || chat_repl {
                chat::run_chat(cli.resume, cli.r#continue, cli.query, cli.model, cli.mode).await?;
            } else {
                launch_ui(cli.resume, cli.r#continue, cli.model, cli.mode).await?;
            }
        }
        Some(Commands::Tui) => {
            if cli.query.is_some() {
                eprintln!("tui is interactive; ignoring --query and starting the TUI.");
            }
            launch_ui(cli.resume, cli.r#continue, cli.model, cli.mode).await?;
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

/// Whether this invocation runs an interactive terminal UI (TUI/REPL) that owns the screen
/// — in which case tracing logs must NOT go to stderr (they'd corrupt the display).
fn is_interactive_session(cli: &Cli) -> bool {
    if cli.query.is_some() {
        return false;
    }
    matches!(
        &cli.command,
        None | Some(Commands::Tui) | Some(Commands::Repl) | Some(Commands::Chat { .. })
    )
}

/// Initialize tracing. For interactive sessions, logs go to `<data_dir>/holmes.log` so the
/// terminal UI isn't corrupted; otherwise they go to stderr as before.
fn init_tracing(interactive: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        // Default `warn`, but silence chromiumoxide's noisy CDP stream (Chrome emits event
        // variants chromiumoxide can't deserialize → a WARN per ignored message).
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,chromiumoxide=error"));

    if interactive {
        let dir = holmes_data_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("holmes.log"))
        {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(move || file.try_clone().expect("clone holmes.log handle"))
                .init();
            return;
        }
    }
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Launch the interactive UI. Defaults to the new inline (Claude-Code-style) UI; set
/// `HOLMES_CLASSIC_TUI=1` to use the legacy full-screen TUI during the transition.
async fn launch_ui(
    resume: Option<String>,
    continue_last: bool,
    model: Option<String>,
    mode: String,
) -> anyhow::Result<()> {
    if std::env::var_os("HOLMES_CLASSIC_TUI").is_some() {
        tui::run_tui(resume, continue_last, model, mode).await
    } else {
        inline_ui::run(resume, continue_last, model, mode).await
    }
}
