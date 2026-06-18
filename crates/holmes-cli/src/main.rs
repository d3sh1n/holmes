use clap::{Parser, Subcommand};
use holmes_cli::{chat, profile};

#[derive(Parser)]
#[command(name = "holmes", about = "Holmes — AI-powered security research agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long)]
    resume: Option<String>,

    #[arg(short, long)]
    r#continue: bool,

    #[arg(short, long)]
    query: Option<String>,

    #[arg(short, long)]
    model: Option<String>,

    #[arg(long, default_value = "pentest")]
    mode: String,

    #[arg(short = 'p', long)]
    profile: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    Chat,
    Sessions,
    Version,
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
}

#[derive(Subcommand)]
enum ProfileAction {
    List,
    Use { name: String },
    Create { name: String, #[arg(long)] clone: Option<String> },
    Delete { name: String },
    Show { name: Option<String> },
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
                cli.profile.as_deref(),
            ).await?;
        }
        Commands::Sessions => {
            chat::list_sessions(cli.profile.as_deref()).await?;
        }
        Commands::Version => {
            println!("Holmes v{}", env!("CARGO_PKG_VERSION"));
        }
        Commands::Profile { action } => {
            let profiles = profile::HolmesProfiles::new();
            match action {
                ProfileAction::List => {
                    let list = profiles.list()?;
                    let active = profiles.resolve(None);
                    let active_name = active.file_name().unwrap().to_string_lossy();
                    for name in &list {
                        if name == active_name.as_ref() { println!("* {}", name); }
                        else { println!("  {}", name); }
                    }
                }
                ProfileAction::Use { name } => {
                    profiles.set_active(&name)?;
                    println!("Switched to profile '{}'", name);
                }
                ProfileAction::Create { name, clone } => {
                    profiles.create(&name, clone.as_deref())?;
                }
                ProfileAction::Delete { name } => {
                    profiles.delete(&name)?;
                }
                ProfileAction::Show { name } => {
                    profiles.show(name.as_deref())?;
                }
            }
        }
    }
    Ok(())
}
