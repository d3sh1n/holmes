use anyhow::Context;
use holmes_core::config::{ApiFormat, Config, HolmesConfig};
use holmes_core::event::Event;
use holmes_core::session::RuntimeSession;
use holmes_core::tool_types::{Message, Role};
use holmes_core::types::*;
use holmes_guards::GuardChain;
use holmes_llm::client::LlmClient;
use holmes_mind_palace::MindPalace;
use holmes_session::db::{CreateSessionParams, SessionDB};
use holmes_session::memory_store::MemoryStore;
use holmes_session::selector::Selector;
use holmes_tools::ToolRegistry;
use rustyline::DefaultEditor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::commands::CommandRegistry;
use crate::workflows;

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

fn holmes_data_dir(profile: Option<&str>) -> PathBuf {
    crate::profile::HolmesProfiles::new().resolve(profile)
}

fn load_config(path: &Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let cfg: HolmesConfig = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;
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

fn api_format_label(fmt: &ApiFormat) -> &'static str {
    match fmt {
        ApiFormat::Openai => "openai",
        ApiFormat::Anthropic => "anthropic",
    }
}

/// Mutable runtime context for the chat REPL — shared with all slash command handlers.
pub struct ChatContext {
    pub session_id: String,
    pub session_db: Arc<SessionDB>,
    pub memory_store: Arc<MemoryStore>,
    pub llm: Arc<LlmClient>,
    pub registry: Arc<ToolRegistry>,
    pub guards: Arc<Mutex<GuardChain>>,
    pub selector: Selector,
    pub runtime_session: RuntimeSession,
    pub mind_palace: MindPalace,
    pub config: Config,
    pub profile_dir: PathBuf,
    pub command_registry: CommandRegistry,
}

pub async fn run_chat(
    resume_id: Option<String>,
    continue_last: bool,
    query: Option<String>,
    model: Option<String>,
    mode_str: String,
    profile: Option<&str>,
) -> anyhow::Result<()> {
    let data_dir = holmes_data_dir(profile);
    std::fs::create_dir_all(&data_dir)?;

    let config_path = data_dir.join("config.yaml");
    let config = if config_path.exists() {
        load_config(&config_path)?
    } else {
        let default_config = HolmesConfig::default();
        let yaml = serde_yaml::to_string(&default_config)?;
        std::fs::write(&config_path, yaml)?;
        eprintln!("Created default config at {}", config_path.display());
        eprintln!("Please edit it to configure your LLM provider and API key.");
        return Ok(());
    };

    let db_path = data_dir.join("holmes.db");
    let session_db = Arc::new(SessionDB::open(&db_path).await?);

    let memory_path = data_dir.join("memory.db");
    let memory_store = Arc::new(MemoryStore::open(&memory_path).await?);

    let registry = Arc::new({
        let mut r = ToolRegistry::new();
        holmes_tools::builtin::register_all(&mut r, &config);
        r
    });

    let guards = Arc::new(Mutex::new(GuardChain::from_config(&config.guards)));
    let llm = Arc::new(LlmClient::new(&config));
    let mode = parse_mode(&mode_str);

    // Build workflows and selector
    let mut selector = Selector::new();
    for wf in workflows::create_builtin_workflows(llm.clone(), registry.clone(), guards.clone()) {
        selector.register(wf);
    }

    // Create RuntimeSession
    let (session_id, runtime_session, mind_palace, is_resume) = if let Some(id) = resume_id {
        let events = session_db.get_events(&id).await?;
        let mut mp = MindPalace::new(session_db.clone(), memory_store.clone());
        let mut session = RuntimeSession::new(id.clone(), mode.clone())
            .with_system_prompt(SYSTEM_PROMPT);
        for se in &events {
            mp.ingest(se.event.clone());
            if let Event::UserMessage { content, .. } = &se.event {
                session.messages.push(Message::user(content.clone()));
            }
        }
        eprintln!("↻ Resumed session {}", &id[..8.min(id.len())]);
        (id, session, mp, true)
    } else if continue_last {
        let filter = SessionFilter { limit: Some(1), ..Default::default() };
        let sessions = session_db.list_sessions(&filter).await?;
        if let Some(s) = sessions.first() {
            let events = session_db.get_events(&s.id).await?;
            let mut mp = MindPalace::new(session_db.clone(), memory_store.clone());
            let mut session = RuntimeSession::new(s.id.clone(), mode.clone())
                .with_system_prompt(SYSTEM_PROMPT);
            for se in &events {
                mp.ingest(se.event.clone());
                if let Event::UserMessage { content, .. } = &se.event {
                    session.messages.push(Message::user(content.clone()));
                }
            }
            eprintln!("↻ Continued session {}", &s.id[..8.min(s.id.len())]);
            (s.id.clone(), session, mp, true)
        } else {
            let session = session_db.create_session(CreateSessionParams {
                id: None, title: None, mode: Some(mode.clone()), model: model.clone(),
                system_prompt: Some(SYSTEM_PROMPT.to_string()),
                parent_session_id: None, fork_point: None, source: Some("cli".into()), tags: vec![],
            }).await?;
            let mp = MindPalace::new(session_db.clone(), memory_store.clone());
            let sid = session.id.clone();
            (session.id, RuntimeSession::new(sid, mode.clone()).with_system_prompt(SYSTEM_PROMPT), mp, false)
        }
    } else {
        let session = session_db.create_session(CreateSessionParams {
            id: None, title: None, mode: Some(mode.clone()), model: model.clone(),
            system_prompt: Some(SYSTEM_PROMPT.to_string()),
            parent_session_id: None, fork_point: None, source: Some("cli".into()), tags: vec![],
        }).await?;
        let mp = MindPalace::new(session_db.clone(), memory_store.clone());
        let sid = session.id.clone();
        (session.id, RuntimeSession::new(sid, mode.clone()).with_system_prompt(SYSTEM_PROMPT), mp, false)
    };

    // One-shot query
    if let Some(q) = query {
        let mut runtime_session = runtime_session;
        runtime_session.messages.push(Message::user(q));
        run_selector_loop(&selector, &mut runtime_session, &llm, &session_db, &session_id).await?;
        session_db.end_session(&session_id, EndReason::UserQuit).await?;
        return Ok(());
    }

    // Interactive REPL
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

    let mut ctx = ChatContext {
        session_id,
        session_db: session_db.clone(),
        memory_store: memory_store.clone(),
        llm: llm.clone(),
        registry: registry.clone(),
        guards: guards.clone(),
        selector,
        runtime_session,
        mind_palace,
        config,
        profile_dir: data_dir.clone(),
        command_registry: CommandRegistry::default(),
    };

    loop {
        let prompt = if ctx.runtime_session.message_count() <= 1 { "> " } else { "» " };
        let Ok(line) = rl.readline(prompt) else { break };
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() { continue; }

        if trimmed.starts_with('/') {
            match handle_slash_command(&trimmed, &mut ctx).await {
                SlashResult::Quit => break,
                SlashResult::Handled => continue,
                SlashResult::NewSession(rs, mp, new_id) => {
                    ctx.runtime_session = rs;
                    ctx.mind_palace = mp;
                    ctx.session_id = new_id;
                    // Rebuild selector with new session context
                    let mut sel = Selector::new();
                    for wf in workflows::create_builtin_workflows(
                        ctx.llm.clone(), ctx.registry.clone(), ctx.guards.clone(),
                    ) {
                        sel.register(wf);
                    }
                    ctx.selector = sel;
                }
                SlashResult::NotHandled(cmd) => {
                    ctx.runtime_session.messages.push(Message::user(format!("/{}", cmd)));
                    print!("🤔 ");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    match run_selector_loop(
                        &ctx.selector, &mut ctx.runtime_session,
                        &ctx.llm, ctx.session_db.as_ref(), &ctx.session_id,
                    ).await {
                        Ok(()) => {}
                        Err(e) => eprintln!("\n✗ Error: {}", e),
                    }
                    println!();
                }
            }
        } else {
            let _ = rl.add_history_entry(trimmed.as_str());
            let _ = rl.save_history(&history_path);

            let user_event = Event::UserMessage {
                content: trimmed.clone(),
                timestamp: chrono::Utc::now(),
            };
            ctx.session_db.append_event(&ctx.session_id, &user_event).await?;
            ctx.mind_palace.ingest(user_event);

            ctx.runtime_session.messages.push(Message::user(trimmed));

            print!("🤔 ");
            use std::io::Write;
            let _ = std::io::stdout().flush();

            match run_selector_loop(
                &ctx.selector, &mut ctx.runtime_session,
                &ctx.llm, ctx.session_db.as_ref(), &ctx.session_id,
            ).await {
                Ok(()) => {}
                Err(e) => eprintln!("\n✗ Error: {}", e),
            }
            println!();
        }
    }

    let _ = rl.save_history(&history_path);
    ctx.session_db.end_session(&ctx.session_id, EndReason::UserQuit).await?;
    println!("Goodbye.");
    Ok(())
}

/// Run the Selector → Workflow loop until DONE
async fn run_selector_loop(
    selector: &Selector,
    session: &mut RuntimeSession,
    llm: &Arc<LlmClient>,
    session_db: &SessionDB,
    session_id: &str,
) -> anyhow::Result<()> {
    // Run the chat workflow first (handles user input directly)
    if let Some(chat_wf) = selector.get("chat") {
        chat_wf.forward(session).await.map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    // Then let the selector decide if more workflows are needed
    loop {
        match selector.select(session, llm).await {
            Ok(Some(name)) => {
                println!("\n  → {}", name);
                if let Some(wf) = selector.get(&name) {
                    wf.forward(session).await.map_err(|e| anyhow::anyhow!("{}", e))?;
                }
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("  Selector error: {}", e);
                break;
            }
        }
    }

    // Persist session events
    for msg in session.messages.iter().skip(session_db.get_events(session_id).await?.len()) {
        if let Some(ref content) = msg.content {
            session_db.append_event(session_id, &Event::Thinking {
                content: content.clone(),
                reasoning_type: None,
            }).await?;
        }
    }

    Ok(())
}

enum SlashResult {
    Quit,
    Handled,
    NewSession(RuntimeSession, MindPalace, String),
    NotHandled(String),
}

#[allow(clippy::too_many_lines)]
async fn handle_slash_command(input: &str, ctx: &mut ChatContext) -> SlashResult {
    let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let args = parts.get(1).copied().unwrap_or("").trim();

    // Resolve aliases
    let canonical = ctx.command_registry.resolve(&cmd).unwrap_or(&cmd);

    match canonical {
        // === Session management ===
        "new" | "reset" => {
            ctx.session_db.end_session(&ctx.session_id, EndReason::UserQuit).await.ok();
            let session = ctx.session_db.create_session(CreateSessionParams {
                id: None,
                title: None,
                mode: Some(ctx.runtime_session.mode.clone()),
                model: None,
                system_prompt: Some(SYSTEM_PROMPT.to_string()),
                parent_session_id: None,
                fork_point: None,
                source: Some("cli".into()),
                tags: vec![],
            }).await.ok();
            if let Some(s) = session {
                let new_id = s.id.clone();
                let mp = MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
                let rs = RuntimeSession::new(new_id.clone(), ctx.runtime_session.mode.clone())
                    .with_system_prompt(SYSTEM_PROMPT);
                println!("Started new session: {}", &new_id[..8.min(new_id.len())]);
                return SlashResult::NewSession(rs, mp, new_id);
            }
            SlashResult::Handled
        }

        "clear" => {
            print!("\x1B[2J\x1B[H");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            // Recurse into /new
            Box::pin(handle_slash_command("/new", ctx)).await
        }

        "resume" => {
            if args.is_empty() {
                println!("Usage: /resume <id|title>");
                return SlashResult::Handled;
            }
            let filter = SessionFilter { limit: Some(100), ..Default::default() };
            match ctx.session_db.list_sessions(&filter).await {
                Ok(sessions) => {
                    let target = sessions.iter().find(|s| {
                        s.id.starts_with(args) || s.title.as_deref() == Some(args)
                    });
                    if let Some(s) = target {
                        ctx.session_db.end_session(&ctx.session_id, EndReason::UserQuit).await.ok();
                        let events = ctx.session_db.get_events(&s.id).await.ok().unwrap_or_default();
                        let mut mp = MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
                        let mut rs = RuntimeSession::new(s.id.clone(), s.mode.clone())
                            .with_system_prompt(SYSTEM_PROMPT);
                        for se in &events {
                            mp.ingest(se.event.clone());
                            if let Event::UserMessage { content, .. } = &se.event {
                                rs.messages.push(Message::user(content.clone()));
                            }
                        }
                        println!(
                            "↻ Resumed session {} ({})",
                            &s.id[..8.min(s.id.len())],
                            s.title.as_deref().unwrap_or("untitled"),
                        );
                        return SlashResult::NewSession(rs, mp, s.id.clone());
                    }
                    println!("Session not found: {}", args);
                }
                Err(e) => eprintln!("Error: {}", e),
            }
            SlashResult::Handled
        }

        "sessions" | "history" => {
            match ctx.session_db.list_sessions(&SessionFilter { limit: Some(20), ..Default::default() }).await {
                Ok(sessions) => {
                    println!("Recent sessions:");
                    for s in &sessions {
                        let marker = if s.id == ctx.session_id { "→" } else { " " };
                        let status = if s.ended_at.is_some() { "ended" } else { "active" };
                        let title = s.title.as_deref().unwrap_or("(untitled)");
                        println!(
                            " {} {}  {}  {}",
                            marker,
                            &s.id[..8.min(s.id.len())],
                            status,
                            title,
                        );
                    }
                    println!("\nUse /resume <id> to switch, /session for details");
                }
                Err(e) => eprintln!("Error: {}", e),
            }
            SlashResult::Handled
        }

        "session" => {
            match ctx.session_db.get_session(&ctx.session_id).await {
                Ok(Some(s)) => {
                    println!("Session: {}", &s.id[..8.min(s.id.len())]);
                    println!("  Title: {}", s.title.as_deref().unwrap_or("(untitled)"));
                    println!("  Mode: {:?}", s.mode);
                    println!("  Messages: {}", s.message_count);
                    println!("  Tool calls: {}", s.tool_call_count);
                    println!("  Tokens: {} in / {} out", s.input_tokens, s.output_tokens);
                    println!("  Started: {}", s.started_at);
                    if let Some(end) = s.ended_at {
                        println!("  Ended: {}", end);
                    }
                    if let Some(ref goal) = s.goal_condition {
                        println!("  Goal: {}", goal);
                    }
                }
                Ok(None) => println!("Session not found"),
                Err(e) => eprintln!("Error: {}", e),
            }
            SlashResult::Handled
        }

        "rename" | "title" => {
            if args.is_empty() {
                if let Ok(Some(s)) = ctx.session_db.get_session(&ctx.session_id).await {
                    println!("Title: {}", s.title.as_deref().unwrap_or("(untitled)"));
                }
            } else {
                ctx.session_db.set_title(&ctx.session_id, args).await.ok();
                println!("Renamed to: {}", args);
            }
            SlashResult::Handled
        }

        "branch" | "fork" => {
            let title = if args.is_empty() { None } else { Some(args.to_string()) };
            let fork_point = ctx.runtime_session.message_count() as u64;
            match ctx.session_db.fork_session(
                &ctx.session_id,
                fork_point,
                title.as_deref().unwrap_or("branch"),
            ).await {
                Ok(new_session) => {
                    println!(
                        "Branched to: {} ({})",
                        &new_session.id[..8.min(new_session.id.len())],
                        new_session.title.as_deref().unwrap_or("untitled"),
                    );
                }
                Err(e) => eprintln!("Error: {}", e),
            }
            SlashResult::Handled
        }

        "compress" | "compact" => {
            ctx.mind_palace.compress();
            println!("Context compressed. Memory palace pruned.");
            SlashResult::Handled
        }

        "retry" => {
            // Drop trailing assistant/tool messages and re-queue last user input
            let last_user = ctx.runtime_session.messages.iter().rposition(|m| m.role == Role::User);
            if let Some(pos) = last_user {
                ctx.runtime_session.messages.truncate(pos);
                println!("Retrying last turn...");
                return SlashResult::NotHandled("retry".into());
            }
            println!("Nothing to retry.");
            SlashResult::Handled
        }

        "undo" => {
            let last_user = ctx.runtime_session.messages.iter().rposition(|m| m.role == Role::User);
            if let Some(pos) = last_user {
                ctx.runtime_session.messages.truncate(pos);
                println!("Undone last turn.");
            } else {
                println!("Nothing to undo.");
            }
            SlashResult::Handled
        }

        "save" | "export" => {
            let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
            let filename = format!("holmes_session_{}.json", ts);
            let json = serde_json::to_string_pretty(&ctx.runtime_session.messages).unwrap_or_default();
            if let Err(e) = std::fs::write(&filename, &json) {
                eprintln!("Save failed: {}", e);
            } else {
                println!("Saved to {}", filename);
            }
            SlashResult::Handled
        }

        // === Goal system ===
        "goal" => {
            if args.is_empty() {
                if let Ok(Some(s)) = ctx.session_db.get_session(&ctx.session_id).await {
                    if let Some(ref goal) = s.goal_condition {
                        println!("◎ Goal active");
                        println!("  Condition: {}", goal);
                        println!(
                            "  Turns: {}, Tokens: {} in / {} out",
                            s.message_count, s.input_tokens, s.output_tokens,
                        );
                    } else {
                        println!("No active goal. Use /goal <condition> to set one.");
                    }
                }
            } else if matches!(args, "clear" | "stop" | "off") {
                // TODO: clear goal in DB
                println!("Goal cleared.");
            } else {
                println!("◎ Goal set: {}", args);
                // TODO: start goal loop (Task 4)
            }
            SlashResult::Handled
        }

        // === Config & Model ===
        "model" => {
            if args.is_empty() || args == "list" {
                println!("Configured providers:");
                for p in &ctx.config.llm.providers {
                    println!("  {}: {} ({})", p.name, p.model, api_format_label(&p.api_format));
                }
                println!("\nUse /model <name> to switch.");
            } else {
                println!("Model switching requires restart. Use -m <model> when starting holmes.");
            }
            SlashResult::Handled
        }

        "provider" => {
            for p in &ctx.config.llm.providers {
                println!(
                    "{}: {} @ {} (priority: {})",
                    p.name, p.model, p.base_url, p.priority,
                );
            }
            SlashResult::Handled
        }

        "mode" => {
            if args.is_empty() {
                println!("Current mode: {:?}", ctx.runtime_session.mode);
                println!("Available: pentest, audit, reverse, research, mixed");
            } else {
                let new_mode = parse_mode(args);
                ctx.runtime_session.mode = new_mode.clone();
                println!("Mode switched to: {:?}", new_mode);
            }
            SlashResult::Handled
        }

        "config" => {
            if args.starts_with("set ") {
                println!(
                    "Config editing not yet supported in REPL. Edit {} directly.",
                    ctx.profile_dir.join("config.yaml").display(),
                );
            } else {
                println!("Config: {}", ctx.profile_dir.join("config.yaml").display());
                println!("  Providers: {}", ctx.config.llm.providers.len());
                println!("  Output dir: {}", ctx.config.output_dir);
                println!(
                    "  Browser: {}",
                    if ctx.config.browser.enabled { "enabled" } else { "disabled" },
                );
            }
            SlashResult::Handled
        }

        // === Tools ===
        "tools" => {
            let defs = ctx.registry.definitions();
            if args.is_empty() {
                println!("Available tools ({}):", defs.len());
                for d in &defs {
                    let desc: String = d.function.description.chars().take(80).collect();
                    println!("  {} — {}", d.function.name, desc);
                }
            } else if let Some(d) = defs.iter().find(|d| d.function.name == args) {
                println!("Tool: {}", d.function.name);
                println!("  Description: {}", d.function.description);
                println!(
                    "  Parameters: {}",
                    serde_json::to_string_pretty(&d.function.parameters).unwrap_or_default(),
                );
            } else {
                println!("Tool not found: {}", args);
            }
            SlashResult::Handled
        }

        "mcp" => {
            if args == "reload" {
                println!("MCP reload not yet implemented.");
            } else {
                println!("MCP servers: {} configured", ctx.config.mcp.servers.len());
                for s in &ctx.config.mcp.servers {
                    println!("  {}: {:?}", s.name, s.transport);
                }
            }
            SlashResult::Handled
        }

        // === Info ===
        "help" => {
            println!("Holmes Commands:\n");
            let categories = ctx.command_registry.list_by_category();
            for (cat, cmds) in &categories {
                println!("  {}:", cat);
                for cmd in cmds {
                    let alias_str = if cmd.aliases.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", cmd.aliases.join(", "))
                    };
                    let args_hint = cmd.args_hint.unwrap_or("");
                    let lhs = format!("{}{}", cmd.name, alias_str);
                    println!("    /{:<14} {}  {}", lhs, args_hint, cmd.description);
                }
                println!();
            }
            println!("  Direct tool: !<command>   — Execute shell command directly");
            println!("              !!           — Repeat last command");
            SlashResult::Handled
        }

        "status" => {
            let s = &ctx.runtime_session;
            println!("Session:   {}", &s.id[..8.min(s.id.len())]);
            println!("Mode:      {:?}", s.mode);
            println!("Messages:  {}", s.message_count());
            println!("Tokens:    {} in / {} out", s.tokens.input, s.tokens.output);
            let parent_short = s.lineage.parent_id.as_ref().map(|id| {
                let n = 8.min(id.len());
                id[..n].to_string()
            });
            println!(
                "Lineage:   parent={:?}, fork_point={:?}",
                parent_short, s.lineage.fork_point,
            );
            SlashResult::Handled
        }

        "dashboard" => {
            let dashboard = ctx.mind_palace.dashboard(&ctx.runtime_session.mode);
            if dashboard.sections.is_empty() {
                println!("Dashboard is empty. Start an engagement to populate it.");
            } else {
                for (_name, section) in &dashboard.sections {
                    println!("  [{}]", section.title);
                    println!("    {}", section.content_summary);
                    println!();
                }
            }
            SlashResult::Handled
        }

        "usage" => {
            match ctx.session_db.get_session(&ctx.session_id).await {
                Ok(Some(s)) => {
                    println!("Session token usage:");
                    println!("  Input:  {}", s.input_tokens);
                    println!("  Output: {}", s.output_tokens);
                    println!("  Total:  {}", s.input_tokens + s.output_tokens);
                    println!("  Cost:   ${:.4}", s.estimated_cost_usd);
                }
                _ => println!("Usage info unavailable."),
            }
            SlashResult::Handled
        }

        "version" => {
            println!("Holmes v{}", env!("CARGO_PKG_VERSION"));
            SlashResult::Handled
        }

        // === Workflow control ===
        "workflows" => {
            let names = ctx.selector.workflow_names();
            println!("Available workflows:");
            for name in &names {
                if let Some(wf) = ctx.selector.get(name) {
                    println!("  {} — {}", name, wf.description());
                }
            }
            SlashResult::Handled
        }

        "workflow" => {
            if args.is_empty() {
                println!("Usage: /workflow <name>");
                return SlashResult::Handled;
            }
            if let Some(wf) = ctx.selector.get(args) {
                match wf.forward(&mut ctx.runtime_session).await {
                    Ok(()) => println!("Workflow '{}' completed.", args),
                    Err(e) => eprintln!("Workflow error: {}", e),
                }
            } else {
                println!("Unknown workflow: {}. Use /workflows to list.", args);
            }
            SlashResult::Handled
        }

        "chat" => {
            println!("Chat mode active. Send a message to begin.");
            SlashResult::Handled
        }

        // === Exit ===
        "quit" | "exit" | "q" => SlashResult::Quit,

        // Unknown
        _ => SlashResult::NotHandled(cmd.to_string()),
    }
}

pub async fn list_sessions(profile: Option<&str>) -> anyhow::Result<()> {
    let data_dir = holmes_data_dir(profile);
    let db_path = data_dir.join("holmes.db");
    let db = SessionDB::open(&db_path).await?;
    let sessions = db.list_sessions(&SessionFilter { limit: Some(20), ..Default::default() }).await?;
    println!("Recent sessions:");
    for s in &sessions {
        let status = if s.ended_at.is_some() { "ended" } else { "active" };
        let title = s.title.as_deref().unwrap_or("(untitled)");
        println!("  {}  {:<8}  {}", &s.id[..8.min(s.id.len())], status, title);
    }
    Ok(())
}
