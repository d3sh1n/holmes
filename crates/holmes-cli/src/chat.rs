use anyhow::Context;
use holmes_core::config::{Config, HolmesConfig};
use holmes_core::event::Event;
use holmes_core::state::AttackState;
use holmes_core::tool_types::Message;
use holmes_core::types::*;
use holmes_guards::GuardChain;
use holmes_llm::client::LlmClient;
use holmes_mind_palace::MindPalace;
use holmes_session::db::{CreateSessionParams, SessionDB};
use holmes_session::memory_store::MemoryStore;
use holmes_tools::ToolRegistry;
use rustyline::DefaultEditor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::agent_loop;

const SYSTEM_PROMPT: &str = r#"你是 Holmes，一个渗透测试、安全研究和逆向工程的 AI Agent。

## 核心原则
- 你与用户（Watson）协作进行安全研究。用户主导，你执行并建议。
- 诚实透明：不确定的事情明确说。不伪造结果。
- 安全第一：仅在授权范围内操作。GuardChain 会阻止越界行为。
- 方法优先：先理解再行动。不要盲目扫描。

## 工作方式
- 用户提出任务 → 你分析理解 → 提出方案 → 执行 → 汇报结果
- 维护记忆宫殿：记录发现、更新态势、关联历史经验
- 遇到停滞时主动反思，建议替代方案

## 工具使用
- 每次工具调用前思考目的
- 工具结果驱动下一步决策
- 工具被 Guard 阻断时，分析原因并调整策略
"#;

fn holmes_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("holmes")
}

fn load_config(path: &Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let cfg: HolmesConfig = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;
    // Resolve API keys from env if requested.
    let mut cfg = cfg;
    for provider in cfg.llm.providers.iter_mut() {
        if provider.api_key.is_empty() {
            if let Some(env_var) = &provider.api_key_env {
                if let Ok(v) = std::env::var(env_var) {
                    provider.api_key = v;
                }
            }
        }
    }
    Ok(cfg)
}

fn parse_mode(s: &str) -> SessionMode {
    match s.to_lowercase().as_str() {
        "code_audit" | "audit" | "code-audit" => SessionMode::CodeAudit,
        "reverse" | "re" => SessionMode::Reverse,
        "security_research" | "research" | "security-research" => SessionMode::SecurityResearch,
        "mixed" => SessionMode::Mixed,
        _ => SessionMode::Pentest,
    }
}

pub async fn run_chat(
    resume_id: Option<String>,
    continue_last: bool,
    query: Option<String>,
    model: Option<String>,
    mode_str: String,
) -> anyhow::Result<()> {
    let data_dir = holmes_data_dir();
    std::fs::create_dir_all(&data_dir)?;

    // Load config.
    let config_path = data_dir.join("config.yaml");
    let config = if config_path.exists() {
        load_config(&config_path)?
    } else {
        // Create default config and ask the user to fill it in.
        let default_config = HolmesConfig::default();
        let yaml = serde_yaml::to_string(&default_config)?;
        std::fs::write(&config_path, yaml)?;
        eprintln!("Created default config at {}", config_path.display());
        eprintln!("Please edit it to configure your LLM provider and API key.");
        return Ok(());
    };

    // Open databases.
    let db_path = data_dir.join("holmes.db");
    let session_db = Arc::new(SessionDB::open(&db_path).await?);

    let memory_path = data_dir.join("memory.db");
    let memory_store = Arc::new(MemoryStore::open(&memory_path).await?);

    // Initialize tools.
    let mut registry = ToolRegistry::new();
    holmes_tools::builtin::register_all(&mut registry, &config);

    // Initialize guards.
    let guards = Arc::new(Mutex::new(GuardChain::from_config(&config.guards)));

    // Initialize LLM client.
    let llm = LlmClient::new(&config);

    // Initialize AttackState (needed by guards).
    let state = Arc::new(Mutex::new(AttackState::new(
        "unknown".to_string(),
        String::new(),
        String::new(),
        String::new(),
        Vec::new(),
    )));

    let mode = parse_mode(&mode_str);

    // Resolve session.
    let (session_id, mut mind_palace, is_resume) = if let Some(id) = resume_id {
        let events = session_db.get_events(&id).await?;
        let mut mp = MindPalace::new(session_db.clone(), memory_store.clone());
        for se in &events {
            mp.ingest(se.event.clone());
        }
        eprintln!("↻ Resumed session {}", &id[..8.min(id.len())]);
        (id, mp, true)
    } else if continue_last {
        let filter = SessionFilter {
            limit: Some(1),
            ..Default::default()
        };
        let sessions = session_db.list_sessions(&filter).await?;
        if let Some(s) = sessions.first() {
            let events = session_db.get_events(&s.id).await?;
            let mut mp = MindPalace::new(session_db.clone(), memory_store.clone());
            for se in &events {
                mp.ingest(se.event.clone());
            }
            eprintln!("↻ Continued session {}", &s.id[..8.min(s.id.len())]);
            (s.id.clone(), mp, true)
        } else {
            let session = session_db
                .create_session(CreateSessionParams {
                    id: None,
                    title: None,
                    mode: Some(mode.clone()),
                    model: model.clone(),
                    system_prompt: Some(SYSTEM_PROMPT.to_string()),
                    parent_session_id: None,
                    fork_point: None,
                    source: Some("cli".into()),
                    tags: vec![],
                })
                .await?;
            let mp = MindPalace::new(session_db.clone(), memory_store.clone());
            (session.id, mp, false)
        }
    } else {
        let session = session_db
            .create_session(CreateSessionParams {
                id: None,
                title: None,
                mode: Some(mode.clone()),
                model: model.clone(),
                system_prompt: Some(SYSTEM_PROMPT.to_string()),
                parent_session_id: None,
                fork_point: None,
                source: Some("cli".into()),
                tags: vec![],
            })
            .await?;
        let mp = MindPalace::new(session_db.clone(), memory_store.clone());
        (session.id, mp, false)
    };

    // Handle one-shot query.
    if let Some(q) = query {
        let mut messages = vec![Message::system(SYSTEM_PROMPT)];
        if is_resume {
            // Restore conversation from events.
            let events = session_db.get_events(&session_id).await?;
            for se in &events {
                if let Event::UserMessage { content, .. } = &se.event {
                    messages.push(Message::user(content.clone()));
                }
            }
        }
        messages.push(Message::user(q));

        let result = agent_loop::run_agent_loop(
            &mut messages,
            &llm,
            &registry,
            guards.clone(),
            state.clone(),
            session_db.as_ref(),
            &session_id,
            &mut mind_palace,
            &config.agent,
        )
        .await?;

        println!("{}", result);
        session_db
            .end_session(&session_id, EndReason::UserQuit)
            .await?;
        return Ok(());
    }

    // Interactive REPL.
    let mut rl = DefaultEditor::new()?;
    let history_path = data_dir.join("history.txt");
    let _ = rl.load_history(&history_path);

    if !is_resume {
        println!("╔══════════════════════════════════════════════╗");
        println!("║  Holmes — AI Security Research Agent         ║");
        println!("║  Type /help for commands, /quit to exit      ║");
        println!("╚══════════════════════════════════════════════╝");
        println!();
    }

    let mut messages: Vec<Message> = vec![Message::system(SYSTEM_PROMPT)];
    if is_resume {
        let events = session_db.get_events(&session_id).await?;
        for se in &events {
            if let Event::UserMessage { content, .. } = &se.event {
                messages.push(Message::user(content.clone()));
            }
        }
        if messages.len() > 1 {
            println!("(Restored {} previous messages)", messages.len() - 1);
        }
    }

    loop {
        let prompt = if messages.len() <= 1 { "> " } else { "» " };
        let Ok(line) = rl.readline(prompt) else { break };
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }

        // Handle slash commands.
        if trimmed.starts_with('/') {
            match handle_slash_command(trimmed, &session_id, session_db.as_ref()).await {
                SlashResult::Quit => break,
                SlashResult::Handled => continue,
                SlashResult::NotHandled(cmd) => {
                    messages.push(Message::user(format!("/{}", cmd)));
                }
            }
        } else {
            let _ = rl.add_history_entry(trimmed);
            let _ = rl.save_history(&history_path);

            // Record user message.
            let user_event = Event::UserMessage {
                content: trimmed.to_string(),
                timestamp: chrono::Utc::now(),
            };
            session_db.append_event(&session_id, &user_event).await?;
            mind_palace.ingest(user_event);

            messages.push(Message::user(trimmed.to_string()));

            // Run agent loop.
            print!("🤔 ");
            use std::io::Write;
            let _ = std::io::stdout().flush();

            match agent_loop::run_agent_loop(
                &mut messages,
                &llm,
                &registry,
                guards.clone(),
                state.clone(),
                session_db.as_ref(),
                &session_id,
                &mut mind_palace,
                &config.agent,
            )
            .await
            {
                Ok(response) => {
                    println!("\n{}", response);
                }
                Err(e) => {
                    eprintln!("\n✗ Error: {}", e);
                }
            }

            println!();
        }
    }

    let _ = rl.save_history(&history_path);
    session_db
        .end_session(&session_id, EndReason::UserQuit)
        .await?;
    println!("Goodbye.");
    Ok(())
}

enum SlashResult {
    Quit,
    Handled,
    NotHandled(String),
}

async fn handle_slash_command(input: &str, session_id: &str, db: &SessionDB) -> SlashResult {
    let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let _args = parts.get(1).copied().unwrap_or("");

    match cmd.as_str() {
        "quit" | "exit" | "q" => SlashResult::Quit,
        "help" => {
            println!("Commands:");
            println!("  /help       — Show this help");
            println!("  /quit       — Exit Holmes");
            println!("  /sessions   — List recent sessions");
            println!("  /dashboard  — Show current dashboard");
            println!("  /status     — Show current session status");
            println!("  /clear      — Clear conversation (start fresh)");
            println!("  /goal       — Set a goal (/goal <condition>)");
            SlashResult::Handled
        }
        "sessions" => {
            match db
                .list_sessions(&SessionFilter {
                    limit: Some(10),
                    ..Default::default()
                })
                .await
            {
                Ok(sessions) => {
                    println!("Recent sessions:");
                    for s in &sessions {
                        let status = if s.ended_at.is_some() { "ended" } else { "active" };
                        let title = s.title.as_deref().unwrap_or("(untitled)");
                        println!(
                            "  {}  {}  {}",
                            &s.id[..8.min(s.id.len())],
                            status,
                            title
                        );
                    }
                }
                Err(e) => eprintln!("Error listing sessions: {}", e),
            }
            SlashResult::Handled
        }
        "status" => {
            println!("Session: {}", &session_id[..8.min(session_id.len())]);
            SlashResult::Handled
        }
        _ => SlashResult::NotHandled(cmd),
    }
}

pub async fn list_sessions() -> anyhow::Result<()> {
    let data_dir = holmes_data_dir();
    let db_path = data_dir.join("holmes.db");
    let db = SessionDB::open(&db_path).await?;
    let sessions = db
        .list_sessions(&SessionFilter {
            limit: Some(20),
            ..Default::default()
        })
        .await?;
    println!("Recent sessions:");
    for s in &sessions {
        let status = if s.ended_at.is_some() { "ended" } else { "active" };
        let title = s.title.as_deref().unwrap_or("(untitled)");
        println!("  {}  {:<8}  {}", &s.id[..8.min(s.id.len())], status, title);
    }
    Ok(())
}

