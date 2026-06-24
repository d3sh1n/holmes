use anyhow::Context;
use holmes_core::config::{ApiFormat, Config, HolmesConfig};
use holmes_core::event::{Event, ReportGenerator, ReportType, StoredEvent};
use holmes_core::session::RuntimeSession;
use holmes_core::tool_types::{Message, Role};
use holmes_core::types::*;
use holmes_guards::GuardChain;
use holmes_llm::client::LlmClient;
use holmes_mind_palace::MindPalace;
use holmes_runtime::deliberation::LlmBackend;
use holmes_runtime::runtime::{AgentRuntime, TurnOutcome};
use holmes_runtime::{RuntimeContext, RuntimeSink, RuntimeState, RuntimeYield, StreamEvent};
use holmes_session::db::{CreateSessionParams, SessionDB};
use holmes_session::memory_store::MemoryStore;
use holmes_session::selector::Selector;
use holmes_tools::ToolRegistry;
use reedline::{
    default_emacs_keybindings, Completer, Emacs, FileBackedHistory, IdeMenu, KeyCode, KeyModifiers,
    Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, Suggestion, MenuBuilder,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::commands::CommandRegistry;
use crate::project_knowledge::build_system_prompt;
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
        ApiFormat::Openai => "openai config, anthropic wire",
        ApiFormat::Anthropic => "anthropic",
    }
}

async fn build_tool_registry(
    config: &Config,
    session_db: Option<Arc<SessionDB>>,
    memory_store: Option<Arc<MemoryStore>>,
    llm: Option<Arc<LlmClient>>,
    session_id: Option<String>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    
    let runner = if let (Some(db), Some(ms), Some(l), Some(sid)) = (session_db, memory_store, llm, session_id) {
        Some(std::sync::Arc::new(crate::subagent::CliSubagentRunner {
            session_db: db,
            memory_store: ms,
            llm: l,
            config: config.clone(),
            parent_session_id: sid,
        }) as Arc<dyn holmes_core::subagent::SubagentRunner>)
    } else {
        None
    };

    holmes_tools::builtin::register_all(&mut registry, config, runner);
    holmes_tools::mcp::register_mcp_tools(&mut registry, &config.mcp.servers).await;
    registry
}

fn replay_event_into_runtime(
    session: &mut RuntimeSession,
    mind_palace: &mut MindPalace,
    event: Event,
) {
    mind_palace.ingest(event.clone());
    match event {
        Event::UserMessage { content, .. } => {
            session.messages.push(Message::user(content));
        }
        Event::Thinking { content, .. } => {
            session.messages.push(Message::assistant(content));
        }
        Event::ToolResult { name, content, .. } => {
            let call_id = format!("replayed-{}", session.messages.len());
            session
                .messages
                .push(Message::tool_result(call_id, name, content));
        }
        Event::SessionModeSet { mode } => {
            session.mode = mode;
        }
        _ => {}
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
    pub runtime_guards: GuardChain,
    pub selector: Selector,
    pub runtime_session: RuntimeSession,
    pub mind_palace: MindPalace,
    pub runtime_state: RuntimeState,
    pub queued_turns: VecDeque<String>,
    pub steering_notes: Vec<String>,
    pub system_prompt: String,
    pub config: Config,
    pub data_dir: PathBuf,
    pub command_registry: CommandRegistry,
}

struct CliRuntimeSink;

impl RuntimeSink for CliRuntimeSink {
    fn emit(&mut self, event: StreamEvent) {
        match event.data {
            RuntimeYield::MessageToUser { content } | RuntimeYield::PlanUpdate { content } => {
                print_holmes(&content);
            }
            RuntimeYield::ToolStarted { name, call_id } => {
                print_tool_started(&name, call_id.as_deref());
            }
            RuntimeYield::PermissionDecision {
                tool_name,
                allowed,
                reason,
                ..
            } => {
                if !allowed || should_show_tool_output() {
                    print_permission_decision(&tool_name, allowed, &reason);
                }
            }
            RuntimeYield::ToolFinished {
                name,
                success,
                content,
                ..
            } => {
                print_tool_finished(&name, success, &content);
            }
            RuntimeYield::EvidenceUpdate { content } => {
                println!("  evidence: {}", content);
            }
            RuntimeYield::NeedsUserInput { prompt } => {
                print_holmes(&prompt);
            }
            RuntimeYield::CompactionBoundary {
                before_count,
                after_count,
                method,
                ..
            } => {
                println!(
                    "  context: compacted {} -> {} messages ({})",
                    before_count, after_count, method
                );
            }
            RuntimeYield::FinalAnswer { content, .. } => {
                print_holmes(&content);
            }
            RuntimeYield::Error { message } => {
                eprintln!("Holmes error: {}", message);
            }
        }
    }
}

fn print_permission_decision(tool_name: &str, allowed: bool, reason: &str) {
    let status = if allowed { "allowed" } else { "blocked" };
    println!("  permission: {} {} - {}", tool_name, status, reason);
}

fn print_tool_started(name: &str, call_id: Option<&str>) {
    if let Some(call_id) = call_id {
        println!("  tool: {} started ({})", name, short_call_id(call_id));
    } else {
        println!("  tool: {} started", name);
    }
}

fn print_tool_finished(name: &str, success: bool, content: &str) {
    let status = if success { "ok" } else { "failed" };
    println!(
        "  tool: {} {} - {}",
        name,
        status,
        folded_tool_output_summary(content)
    );

    if should_show_tool_output() && !content.trim().is_empty() {
        println!("{}", indent_block(content));
        return;
    }

    if !success {
        if let Some(preview) = folded_tool_output_preview(content) {
            println!("    preview: {}", preview);
        }
    }
}

fn print_holmes(content: &str) {
    let content = content.trim();
    if content.starts_with("Holmes:") {
        println!("{}", content);
    } else {
        println!("Holmes: {}", content);
    }
}

fn should_show_tool_output() -> bool {
    std::env::var("HOLMES_SHOW_TOOL_OUTPUT")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "full"
            )
        })
        .unwrap_or(false)
}

fn short_call_id(call_id: &str) -> String {
    const HEAD: usize = 12;
    const TAIL: usize = 6;
    let char_count = call_id.chars().count();
    if char_count <= HEAD + TAIL + 1 {
        return call_id.to_string();
    }

    let head = call_id.chars().take(HEAD).collect::<String>();
    let tail = call_id
        .chars()
        .rev()
        .take(TAIL)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{head}...{tail}")
}

fn folded_tool_output_summary(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return "no output".into();
    }

    if let Some(summary) = command_result_summary(trimmed) {
        return summary;
    }

    let chars = trimmed.chars().count();
    let lines = trimmed.lines().count().max(1);
    format!("output folded ({} chars, {} lines)", chars, lines)
}

fn command_result_summary(content: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let object = value.as_object()?;
    let exit_code = object.get("exit_code").and_then(|value| value.as_i64());
    let stdout_len = object
        .get("stdout")
        .and_then(|value| value.as_str())
        .map(|value| value.chars().count())
        .unwrap_or(0);
    let stderr_len = object
        .get("stderr")
        .and_then(|value| value.as_str())
        .map(|value| value.chars().count())
        .unwrap_or(0);

    if exit_code.is_none() && !object.contains_key("stdout") && !object.contains_key("stderr") {
        return None;
    }

    let exit = exit_code
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".into());
    Some(format!(
        "output folded (exit {}, stdout {} chars, stderr {} chars)",
        exit, stdout_len, stderr_len
    ))
}

fn folded_tool_output_preview(content: &str) -> Option<String> {
    const MAX_PREVIEW_CHARS: usize = 180;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let preview_source = serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|value| {
            value
                .get("stderr")
                .and_then(|stderr| stderr.as_str())
                .filter(|stderr| !stderr.trim().is_empty())
                .or_else(|| {
                    value
                        .get("stdout")
                        .and_then(|stdout| stdout.as_str())
                        .filter(|stdout| !stdout.trim().is_empty())
                })
                .map(str::to_string)
        })
        .unwrap_or_else(|| trimmed.to_string());

    let single_line = preview_source
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if single_line.is_empty() {
        return None;
    }

    Some(truncate_chars(&single_line, MAX_PREVIEW_CHARS))
}

fn truncate_chars(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }

    let mut out = content
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

fn indent_block(content: &str) -> String {
    content
        .trim()
        .lines()
        .map(|line| format!("    {}", line))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run_runtime_input(
    ctx: &mut ChatContext,
    input: String,
    oneshot: bool,
) -> anyhow::Result<TurnOutcome> {
    apply_steering_notes(ctx).await?;

    let mode = ctx.runtime_session.mode.clone();
    let placeholder_session = RuntimeSession::new(ctx.session_id.clone(), mode.clone());
    let session = std::mem::replace(&mut ctx.runtime_session, placeholder_session);
    let placeholder_palace = MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
    let mind_palace = std::mem::replace(&mut ctx.mind_palace, placeholder_palace);
    let placeholder_guards = GuardChain::from_config(&ctx.config.guards);
    let runtime_guards = std::mem::replace(&mut ctx.runtime_guards, placeholder_guards);
    let placeholder_state = RuntimeState::new(mode);
    let runtime_state = std::mem::replace(&mut ctx.runtime_state, placeholder_state);
    let llm: Arc<dyn LlmBackend> = ctx.llm.clone();

    let runtime_context = RuntimeContext::new(
        session,
        ctx.session_db.clone(),
        ctx.memory_store.clone(),
        mind_palace,
        llm,
        ctx.registry.clone(),
        runtime_guards,
        runtime_state,
        ctx.config.clone(),
    );
    let mut runtime = AgentRuntime::new(runtime_context);
    let mut sink = CliRuntimeSink;
    let result = if oneshot {
        runtime.run_oneshot(input, &mut sink).await
    } else {
        runtime.run_turn(input, &mut sink).await
    };
    let runtime_context = runtime.into_context();

    ctx.session_id = runtime_context.session_id.clone();
    ctx.runtime_session = runtime_context.session;
    ctx.mind_palace = runtime_context.mind_palace;
    ctx.runtime_guards = runtime_context.guards;
    ctx.runtime_state = runtime_context.state;

    result.map_err(Into::into)
}

async fn compact_chat_context(
    ctx: &mut ChatContext,
) -> anyhow::Result<Option<holmes_runtime::CompressionResult>> {
    let mode = ctx.runtime_session.mode.clone();
    let placeholder_session = RuntimeSession::new(ctx.session_id.clone(), mode.clone());
    let session = std::mem::replace(&mut ctx.runtime_session, placeholder_session);
    let placeholder_palace = MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
    let mind_palace = std::mem::replace(&mut ctx.mind_palace, placeholder_palace);
    let placeholder_guards = GuardChain::from_config(&ctx.config.guards);
    let runtime_guards = std::mem::replace(&mut ctx.runtime_guards, placeholder_guards);
    let placeholder_state = RuntimeState::new(mode);
    let runtime_state = std::mem::replace(&mut ctx.runtime_state, placeholder_state);
    let llm: Arc<dyn LlmBackend> = ctx.llm.clone();

    let runtime_context = RuntimeContext::new(
        session,
        ctx.session_db.clone(),
        ctx.memory_store.clone(),
        mind_palace,
        llm,
        ctx.registry.clone(),
        runtime_guards,
        runtime_state,
        ctx.config.clone(),
    );
    let mut runtime = AgentRuntime::new(runtime_context);
    let result = runtime.compact_now().await;
    let runtime_context = runtime.into_context();

    ctx.session_id = runtime_context.session_id.clone();
    ctx.runtime_session = runtime_context.session;
    ctx.mind_palace = runtime_context.mind_palace;
    ctx.runtime_guards = runtime_context.guards;
    ctx.runtime_state = runtime_context.state;

    result.map_err(Into::into)
}

async fn apply_steering_notes(ctx: &mut ChatContext) -> anyhow::Result<()> {
    if ctx.steering_notes.is_empty() {
        return Ok(());
    }

    let notes = std::mem::take(&mut ctx.steering_notes);
    for note in notes {
        let event = Event::HumanFeedback {
            content: note.clone(),
            target_event: None,
            timestamp: chrono::Utc::now(),
        };
        ctx.session_db.append_event(&ctx.session_id, &event).await?;
        ctx.mind_palace.ingest(event);
        ctx.runtime_state
            .observations
            .push(format!("Watson steering: {note}"));
    }

    Ok(())
}

async fn drain_queued_turns(ctx: &mut ChatContext) {
    while let Some(input) = ctx.queued_turns.pop_front() {
        println!("Queued turn: {}", input);
        match run_runtime_input(ctx, input, false).await {
            Ok(_) => {}
            Err(error) => eprintln!("\n✗ Error: {}", error),
        }
        println!();
    }
}

async fn rebuild_runtime_from_events(ctx: &mut ChatContext) -> anyhow::Result<()> {
    let session_record = ctx.session_db.get_session(&ctx.session_id).await?;
    let mode = session_record
        .as_ref()
        .map(|session| session.mode.clone())
        .unwrap_or_else(|| ctx.runtime_session.mode.clone());
    let events = ctx.session_db.get_events(&ctx.session_id).await?;

    let mut mind_palace = MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
    let mut runtime_session =
        RuntimeSession::new(ctx.session_id.clone(), mode).with_system_prompt(&ctx.system_prompt);
    for stored in events {
        replay_event_into_runtime(&mut runtime_session, &mut mind_palace, stored.event);
    }

    let mut runtime_state = RuntimeState::new(runtime_session.mode.clone());
    if let Some(session) = session_record {
        runtime_state.active_goal = session.goal_condition;
    }

    ctx.runtime_session = runtime_session;
    ctx.mind_palace = mind_palace;
    ctx.runtime_state = runtime_state;
    ctx.runtime_guards = GuardChain::from_config(&ctx.config.guards);
    ctx.queued_turns.clear();
    ctx.steering_notes.clear();
    Ok(())
}

fn snapshot_events(events: &[StoredEvent]) -> Vec<&StoredEvent> {
    events
        .iter()
        .filter(|event| matches!(event.event, Event::ContextSnapshotTaken { .. }))
        .collect()
}

fn print_snapshots(snapshots: &[&StoredEvent]) {
    if snapshots.is_empty() {
        println!("No snapshots.");
        return;
    }

    println!("Snapshots:");
    for (idx, snapshot) in snapshots.iter().rev().enumerate() {
        let summary = match &snapshot.event {
            Event::ContextSnapshotTaken { summary, .. } => summary.as_str(),
            _ => "",
        };
        println!(
            "  {}. event_index={}  {}",
            idx + 1,
            snapshot.event_index,
            summary
        );
    }
}

fn select_snapshot_index(events: &[StoredEvent], selector: &str) -> Option<u64> {
    let snapshots = snapshot_events(events);
    if snapshots.is_empty() {
        return None;
    }
    let selector = selector.trim();
    if selector.is_empty() {
        return snapshots.last().map(|snapshot| snapshot.event_index);
    }

    let Ok(value) = selector.parse::<u64>() else {
        return None;
    };

    snapshots
        .iter()
        .find(|snapshot| snapshot.event_index == value)
        .map(|snapshot| snapshot.event_index)
        .or_else(|| {
            let ordinal = value as usize;
            if ordinal == 0 || ordinal > snapshots.len() {
                None
            } else {
                snapshots
                    .iter()
                    .rev()
                    .nth(ordinal - 1)
                    .map(|snapshot| snapshot.event_index)
            }
        })
}

fn render_case_report(
    session_id: &str,
    mode: &SessionMode,
    active_goal: Option<&str>,
    events: &[StoredEvent],
) -> String {
    let mut out = String::new();
    out.push_str("# Holmes Case Report\n\n");
    out.push_str(&format!("- Session: `{session_id}`\n"));
    out.push_str(&format!("- Mode: `{:?}`\n", mode));
    if let Some(goal) = active_goal {
        out.push_str(&format!("- Goal: {goal}\n"));
    }
    out.push_str(&format!(
        "- Generated: {}\n\n",
        chrono::Utc::now().to_rfc3339()
    ));

    let mut user_messages = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();
    let mut evidence = Vec::new();
    let mut reflections = Vec::new();
    let mut finals = Vec::new();

    for event in events {
        match &event.event {
            Event::UserMessage { content, .. } => user_messages.push(content.clone()),
            Event::ToolCall {
                name, arguments, ..
            } => tool_calls.push(format!("{name} `{}`", arguments)),
            Event::ToolResult {
                name,
                success,
                content,
                ..
            } => tool_results.push(format!(
                "{} [{}]\n{}",
                name,
                if *success { "ok" } else { "failed" },
                content.trim()
            )),
            Event::AttackSurfaceUpdate {
                services,
                tech_stack,
                endpoints,
                notes,
                ..
            } => {
                if !services.is_empty() {
                    evidence.push(format!("Services: {:?}", services));
                }
                if !tech_stack.is_empty() {
                    evidence.push(format!("Tech stack: {}", tech_stack.join(", ")));
                }
                if !endpoints.is_empty() {
                    evidence.push(format!("Endpoints: {}", endpoints.join(", ")));
                }
                if let Some(notes) = notes {
                    evidence.push(notes.clone());
                }
            }
            Event::VulnerabilityFound {
                title,
                severity,
                location,
                evidence: finding_evidence,
                status,
                ..
            } => evidence.push(format!(
                "{:?} {:?}: {} at {} — {}",
                severity, status, title, location, finding_evidence
            )),
            Event::MemoryStored { content, .. } => evidence.push(content.clone()),
            Event::ReflectionRecorded {
                diagnosis,
                lessons_learned,
                ..
            } => reflections.push(format!("{diagnosis}\nNext: {lessons_learned}")),
            Event::GoalEvaluated {
                satisfied, reason, ..
            } => finals.push(format!(
                "Goal evaluated: {} — {}",
                if *satisfied {
                    "satisfied"
                } else {
                    "not satisfied"
                },
                reason
            )),
            Event::Thinking { content, .. } => finals.push(content.clone()),
            _ => {}
        }
    }

    push_report_section(&mut out, "User Requests", &user_messages);
    push_report_section(&mut out, "Tool Calls", &tool_calls);
    push_report_section(&mut out, "Tool Results", &tool_results);
    push_report_section(&mut out, "Evidence", &evidence);
    push_report_section(&mut out, "Reflection", &reflections);
    push_report_section(&mut out, "Narrative / Conclusions", &finals);
    out
}

fn push_report_section(out: &mut String, title: &str, items: &[String]) {
    out.push_str(&format!("## {title}\n\n"));
    if items.is_empty() {
        out.push_str("_None recorded._\n\n");
        return;
    }

    for item in items {
        out.push_str("- ");
        out.push_str(&item.replace('\n', "\n  "));
        out.push_str("\n");
    }
    out.push('\n');
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
    let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let system_prompt = build_system_prompt(SYSTEM_PROMPT, &config, &project_dir);

    let db_path = data_dir.join("holmes.db");
    let session_db = Arc::new(SessionDB::open(&db_path).await?);

    let memory_path = data_dir.join("memory.db");
    let memory_store = Arc::new(MemoryStore::open(&memory_path).await?);

    let guards = Arc::new(Mutex::new(GuardChain::from_config(&config.guards)));
    let runtime_guards = GuardChain::from_config(&config.guards);
    let llm = Arc::new(LlmClient::new(&config));
    let mode = parse_mode(&mode_str);

    // Create RuntimeSession
    let (session_id, runtime_session, mind_palace, is_resume) = if let Some(id) = resume_id {
        let events = session_db.get_events(&id).await?;
        let mut mp = MindPalace::new(session_db.clone(), memory_store.clone());
        let mut session =
            RuntimeSession::new(id.clone(), mode.clone()).with_system_prompt(&system_prompt);
        for se in &events {
            replay_event_into_runtime(&mut session, &mut mp, se.event.clone());
        }
        eprintln!("↻ Resumed session {}", &id[..8.min(id.len())]);
        (id, session, mp, true)
    } else if continue_last {
        let filter = SessionFilter {
            limit: Some(1),
            ..Default::default()
        };
        let sessions = session_db.list_sessions(&filter).await?;
        if let Some(s) = sessions.first() {
            let events = session_db.get_events(&s.id).await?;
            let mut mp = MindPalace::new(session_db.clone(), memory_store.clone());
            let mut session =
                RuntimeSession::new(s.id.clone(), mode.clone()).with_system_prompt(&system_prompt);
            for se in &events {
                replay_event_into_runtime(&mut session, &mut mp, se.event.clone());
            }
            eprintln!("↻ Continued session {}", &s.id[..8.min(s.id.len())]);
            (s.id.clone(), session, mp, true)
        } else {
            let session = session_db
                .create_session(CreateSessionParams {
                    id: None,
                    title: None,
                    mode: Some(mode.clone()),
                    model: model.clone(),
                    system_prompt: Some(system_prompt.clone()),
                    parent_session_id: None,
                    fork_point: None,
                    source: Some("cli".into()),
                    tags: vec![],
                })
                .await?;
            let mp = MindPalace::new(session_db.clone(), memory_store.clone());
            let sid = session.id.clone();
            (
                session.id,
                RuntimeSession::new(sid, mode.clone()).with_system_prompt(&system_prompt),
                mp,
                false,
            )
        }
    } else {
        let session = session_db
            .create_session(CreateSessionParams {
                id: None,
                title: None,
                mode: Some(mode.clone()),
                model: model.clone(),
                system_prompt: Some(system_prompt.clone()),
                parent_session_id: None,
                fork_point: None,
                source: Some("cli".into()),
                tags: vec![],
            })
            .await?;
        let mp = MindPalace::new(session_db.clone(), memory_store.clone());
        let sid = session.id.clone();
        (
            session.id,
            RuntimeSession::new(sid, mode.clone()).with_system_prompt(&system_prompt),
            mp,
            false,
        )
    };

    let registry = Arc::new(
        build_tool_registry(
            &config,
            Some(session_db.clone()),
            Some(memory_store.clone()),
            Some(llm.clone()),
            Some(session_id.clone()),
        )
        .await,
    );

    let mut selector = Selector::new();
    for wf in workflows::create_builtin_workflows(llm.clone(), registry.clone(), guards.clone()) {
        selector.register(wf);
    }

    let mut runtime_state = RuntimeState::new(runtime_session.mode.clone());
    if let Some(session_record) = session_db.get_session(&session_id).await? {
        runtime_state.active_goal = session_record.goal_condition;
    }
    let mut ctx = ChatContext {
        session_id,
        session_db: session_db.clone(),
        memory_store: memory_store.clone(),
        llm: llm.clone(),
        registry: registry.clone(),
        guards: guards.clone(),
        runtime_guards,
        selector,
        runtime_session,
        mind_palace,
        runtime_state,
        queued_turns: VecDeque::new(),
        steering_notes: Vec::new(),
        system_prompt,
        config,
        data_dir: data_dir.clone(),
        command_registry: CommandRegistry::default(),
    };

    // One-shot query
    if let Some(q) = query {
        let _ = run_runtime_input(&mut ctx, q, true).await?;
        ctx.session_db
            .end_session(&ctx.session_id, EndReason::UserQuit)
            .await?;
        return Ok(());
    }

    // Interactive REPL
    #[derive(Clone)]
    struct CommandCompleter {
        commands: Vec<(String, String)>,
    }

    impl Completer for CommandCompleter {
        fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
            let mut suggestions = Vec::new();
            if line.starts_with('/') {
                let word = &line[..pos];
                for (cmd, desc) in &self.commands {
                    if cmd.starts_with(word) {
                        suggestions.push(Suggestion {
                            value: cmd.clone(),
                            description: Some(desc.clone()),
                            extra: None,
                            span: Span::new(0, pos),
                            append_whitespace: true,
                            match_indices: None,
                            display_override: None,
                            style: None,
                        });
                    }
                }
            }
            suggestions
        }
    }

    let completer = Box::new(CommandCompleter {
        commands: ctx.command_registry.all_command_hints(),
    });

    let completion_menu = Box::new(
        IdeMenu::default()
            .with_name("completion_menu")
    );

    let history_path = data_dir.join("history.txt");
    let history = match FileBackedHistory::with_file(1000, history_path) {
        Ok(h) => Box::new(h),
        Err(_) => Box::new(reedline::FileBackedHistory::default()),
    };

    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );

    let edit_mode = Box::new(Emacs::new(keybindings));

    let mut rl = Reedline::create()
        .with_completer(completer)
        .with_quick_completions(true)
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_edit_mode(edit_mode)
        .with_history(history);

    if !is_resume {
        println!("╔══════════════════════════════════════════════╗");
        println!("║  Holmes — AI Security Research Agent         ║");
        println!("║  Type /help for commands, /quit to exit      ║");
        println!("╚══════════════════════════════════════════════╝");
        println!();
    }

    #[derive(Clone)]
    struct SimplePrompt {
        left: String,
    }
    impl reedline::Prompt for SimplePrompt {
        fn render_prompt_left(&self) -> std::borrow::Cow<str> { std::borrow::Cow::Borrowed(&self.left) }
        fn render_prompt_right(&self) -> std::borrow::Cow<str> { std::borrow::Cow::Borrowed("") }
        fn render_prompt_indicator(&self, _: reedline::PromptEditMode) -> std::borrow::Cow<str> { std::borrow::Cow::Borrowed("") }
        fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<str> { std::borrow::Cow::Borrowed("::: ") }
        fn render_prompt_history_search_indicator(&self, _: reedline::PromptHistorySearch) -> std::borrow::Cow<str> { std::borrow::Cow::Borrowed("? ") }
    }

    loop {
        let prompt_str = if ctx.runtime_session.message_count() <= 1 {
            "> "
        } else {
            "» "
        };
        let prompt = SimplePrompt { left: prompt_str.to_string() };

        let sig = rl.read_line(&prompt);
        let trimmed = match sig {
            Ok(Signal::Success(buffer)) => buffer.trim().to_string(),
            Ok(Signal::CtrlC) | Ok(Signal::CtrlD) => {
                break;
            }
            Ok(_) => continue,
            Err(e) => {
                eprintln!("REPL Error: {}", e);
                break;
            }
        };

        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('/') {
            match handle_slash_command(&trimmed, &mut ctx).await {
                SlashResult::Quit => break,
                SlashResult::Handled => continue,
                SlashResult::NewSession(rs, mp, new_id) => {
                    ctx.runtime_session = rs;
                    ctx.mind_palace = mp;
                    ctx.session_id = new_id;
                    ctx.runtime_guards = GuardChain::from_config(&ctx.config.guards);
                    ctx.runtime_state = RuntimeState::new(ctx.runtime_session.mode.clone());
                    if let Ok(Some(session_record)) =
                        ctx.session_db.get_session(&ctx.session_id).await
                    {
                        ctx.runtime_state.active_goal = session_record.goal_condition;
                    }
                    ctx.queued_turns.clear();
                    ctx.steering_notes.clear();
                    // Rebuild selector with new session context
                    let mut sel = Selector::new();
                    for wf in workflows::create_builtin_workflows(
                        ctx.llm.clone(),
                        ctx.registry.clone(),
                        ctx.guards.clone(),
                    ) {
                        sel.register(wf);
                    }
                    ctx.selector = sel;
                }
                SlashResult::NotHandled(input) => {
                    match run_runtime_input(&mut ctx, input, false).await {
                        Ok(_) => {}
                        Err(e) => eprintln!("\n✗ Error: {}", e),
                    }
                    println!();
                    drain_queued_turns(&mut ctx).await;
                }
            }
        } else {
            match run_runtime_input(&mut ctx, trimmed, false).await {
                Ok(_) => {}
                Err(e) => eprintln!("\n✗ Error: {}", e),
            }
            println!();
            drain_queued_turns(&mut ctx).await;
        }
    }

    ctx.session_db
        .end_session(&ctx.session_id, EndReason::UserQuit)
        .await?;
    println!("Goodbye.");
    Ok(())
}

/// Run the Selector → Workflow loop until DONE
#[allow(dead_code)]
async fn run_selector_loop(
    selector: &Selector,
    session: &mut RuntimeSession,
    llm: &Arc<LlmClient>,
    session_db: &SessionDB,
    session_id: &str,
) -> anyhow::Result<()> {
    // Run the chat workflow first (handles user input directly)
    if let Some(chat_wf) = selector.get("chat") {
        chat_wf
            .forward(session)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    // Then let the selector decide if more workflows are needed
    loop {
        match selector.select(session, llm).await {
            Ok(Some(name)) => {
                println!("\n  → {}", name);
                if let Some(wf) = selector.get(&name) {
                    wf.forward(session)
                        .await
                        .map_err(|e| anyhow::anyhow!("{}", e))?;
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
    for msg in session
        .messages
        .iter()
        .skip(session_db.get_events(session_id).await?.len())
    {
        if let Some(ref content) = msg.content {
            session_db
                .append_event(
                    session_id,
                    &Event::Thinking {
                        content: content.clone(),
                        reasoning_type: None,
                    },
                )
                .await?;
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
            ctx.session_db
                .end_session(&ctx.session_id, EndReason::UserQuit)
                .await
                .ok();
            let session = ctx
                .session_db
                .create_session(CreateSessionParams {
                    id: None,
                    title: None,
                    mode: Some(ctx.runtime_session.mode.clone()),
                    model: None,
                    system_prompt: Some(ctx.system_prompt.clone()),
                    parent_session_id: None,
                    fork_point: None,
                    source: Some("cli".into()),
                    tags: vec![],
                })
                .await
                .ok();
            if let Some(s) = session {
                let new_id = s.id.clone();
                let mp = MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
                let rs = RuntimeSession::new(new_id.clone(), ctx.runtime_session.mode.clone())
                    .with_system_prompt(&ctx.system_prompt);
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
            let filter = SessionFilter {
                limit: Some(100),
                ..Default::default()
            };
            match ctx.session_db.list_sessions(&filter).await {
                Ok(sessions) => {
                    let target = sessions
                        .iter()
                        .find(|s| s.id.starts_with(args) || s.title.as_deref() == Some(args));
                    if let Some(s) = target {
                        ctx.session_db
                            .end_session(&ctx.session_id, EndReason::UserQuit)
                            .await
                            .ok();
                        let events = ctx
                            .session_db
                            .get_events(&s.id)
                            .await
                            .ok()
                            .unwrap_or_default();
                        let mut mp =
                            MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
                        let mut rs = RuntimeSession::new(s.id.clone(), s.mode.clone())
                            .with_system_prompt(&ctx.system_prompt);
                        for se in &events {
                            replay_event_into_runtime(&mut rs, &mut mp, se.event.clone());
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
            match ctx
                .session_db
                .list_sessions(&SessionFilter {
                    limit: Some(20),
                    ..Default::default()
                })
                .await
            {
                Ok(sessions) => {
                    println!("Recent sessions:");
                    for s in &sessions {
                        let marker = if s.id == ctx.session_id { "→" } else { " " };
                        let status = if s.ended_at.is_some() {
                            "ended"
                        } else {
                            "active"
                        };
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
            let title = if args.is_empty() {
                None
            } else {
                Some(args.to_string())
            };
            let fork_point = ctx.runtime_session.message_count() as u64;
            match ctx
                .session_db
                .fork_session(
                    &ctx.session_id,
                    fork_point,
                    title.as_deref().unwrap_or("branch"),
                )
                .await
            {
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
            match compact_chat_context(ctx).await {
                Ok(Some(result)) => {
                    println!(
                        "Context compressed: {} -> {} messages.",
                        result.before_count, result.after_count
                    );
                }
                Ok(None) => println!("Context is already compact enough."),
                Err(error) => eprintln!("Error: {}", error),
            }
            SlashResult::Handled
        }

        "retry" => {
            // Drop trailing assistant/tool messages and re-queue last user input
            let last_user = ctx
                .runtime_session
                .messages
                .iter()
                .rposition(|m| m.role == Role::User);
            if let Some(pos) = last_user {
                let retry_input = ctx.runtime_session.messages[pos]
                    .content
                    .clone()
                    .unwrap_or_default();
                ctx.runtime_session.messages.truncate(pos);
                if retry_input.trim().is_empty() {
                    println!("Nothing to retry.");
                    return SlashResult::Handled;
                }
                println!("Retrying last turn...");
                return SlashResult::NotHandled(retry_input);
            }
            println!("Nothing to retry.");
            SlashResult::Handled
        }

        "undo" => {
            let last_user = ctx
                .runtime_session
                .messages
                .iter()
                .rposition(|m| m.role == Role::User);
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
            let json =
                serde_json::to_string_pretty(&ctx.runtime_session.messages).unwrap_or_default();
            if let Err(e) = std::fs::write(&filename, &json) {
                eprintln!("Save failed: {}", e);
            } else {
                println!("Saved to {}", filename);
            }
            SlashResult::Handled
        }

        "snapshot" | "checkpoint" => {
            let events = match ctx.session_db.get_events(&ctx.session_id).await {
                Ok(events) => events,
                Err(error) => {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
            };

            if args == "list" {
                let snapshots = snapshot_events(&events);
                print_snapshots(&snapshots);
                return SlashResult::Handled;
            }

            let summary = if args.is_empty() {
                format!("Checkpoint after {} event(s)", events.len())
            } else {
                args.to_string()
            };
            let event = Event::ContextSnapshotTaken {
                summary: summary.clone(),
                preserved_keys: vec![
                    format!("session_id:{}", ctx.session_id),
                    format!("message_count:{}", ctx.runtime_session.message_count()),
                ],
                active_contexts: ctx.runtime_session.context.active_contexts.clone(),
            };

            match ctx.session_db.append_event(&ctx.session_id, &event).await {
                Ok(index) => {
                    ctx.mind_palace.ingest(event);
                    println!("Snapshot saved at event_index={index}: {summary}");
                }
                Err(error) => eprintln!("Error: {}", error),
            }
            SlashResult::Handled
        }

        "rollback" | "rewind" => {
            let events = match ctx.session_db.get_events(&ctx.session_id).await {
                Ok(events) => events,
                Err(error) => {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
            };

            if args == "list" {
                let snapshots = snapshot_events(&events);
                print_snapshots(&snapshots);
                return SlashResult::Handled;
            }

            let Some(target_index) = select_snapshot_index(&events, args) else {
                println!("No matching snapshot. Use /snapshot list to inspect checkpoints.");
                return SlashResult::Handled;
            };

            match ctx
                .session_db
                .truncate_events_after(&ctx.session_id, target_index)
                .await
            {
                Ok(()) => match rebuild_runtime_from_events(ctx).await {
                    Ok(()) => println!("Rolled back to event_index={target_index}."),
                    Err(error) => eprintln!("Rollback rebuild failed: {}", error),
                },
                Err(error) => eprintln!("Rollback failed: {}", error),
            }
            SlashResult::Handled
        }

        "report" => {
            let events = match ctx.session_db.get_events(&ctx.session_id).await {
                Ok(events) => events,
                Err(error) => {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
            };
            let report = render_case_report(
                &ctx.session_id,
                &ctx.runtime_session.mode,
                ctx.runtime_state.active_goal.as_deref(),
                &events,
            );
            let reports_dir = ctx.data_dir.join("reports");
            if let Err(error) = std::fs::create_dir_all(&reports_dir) {
                eprintln!("Report failed: {}", error);
                return SlashResult::Handled;
            }
            let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
            let path = reports_dir.join(format!(
                "holmes_{}_{}.md",
                &ctx.session_id[..8.min(ctx.session_id.len())],
                ts
            ));
            if let Err(error) = std::fs::write(&path, report) {
                eprintln!("Report failed: {}", error);
                return SlashResult::Handled;
            }

            let event = Event::ReportGenerated {
                report_type: match ctx.runtime_session.mode {
                    SessionMode::CodeAudit => ReportType::CodeAuditReport,
                    SessionMode::Reverse => ReportType::ReverseEngineeringReport,
                    _ => ReportType::Writeup,
                },
                file_path: path.display().to_string(),
                sections: vec![
                    "User Requests".into(),
                    "Tool Calls".into(),
                    "Tool Results".into(),
                    "Evidence".into(),
                    "Reflection".into(),
                    "Narrative / Conclusions".into(),
                ],
                generated_by: ReportGenerator::Agent,
            };
            if let Err(error) = ctx.session_db.append_event(&ctx.session_id, &event).await {
                eprintln!(
                    "Warning: report written but event recording failed: {}",
                    error
                );
            } else {
                ctx.mind_palace.ingest(event);
            }
            println!("Report written to {}", path.display());
            SlashResult::Handled
        }

        "queue" => {
            if args.is_empty() {
                if ctx.queued_turns.is_empty() {
                    println!("Queue is empty.");
                } else {
                    println!("Queued turns:");
                    for (idx, turn) in ctx.queued_turns.iter().enumerate() {
                        println!("  {}. {}", idx + 1, turn);
                    }
                }
            } else {
                ctx.queued_turns.push_back(args.to_string());
                println!("Queued turn {}.", ctx.queued_turns.len());
            }
            SlashResult::Handled
        }

        "steer" => {
            if args.is_empty() {
                if ctx.steering_notes.is_empty() {
                    println!("No pending steering notes.");
                } else {
                    println!("Pending steering:");
                    for note in &ctx.steering_notes {
                        println!("  {}", note);
                    }
                }
            } else {
                ctx.steering_notes.push(args.to_string());
                println!("Steering note queued for the next Holmes turn.");
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
                if let Err(error) = ctx
                    .session_db
                    .set_goal_condition(&ctx.session_id, None)
                    .await
                {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
                let event = Event::GoalCleared {
                    reason: "cleared by Watson".into(),
                };
                if let Err(error) = ctx.session_db.append_event(&ctx.session_id, &event).await {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
                ctx.mind_palace.ingest(event);
                ctx.runtime_state.active_goal = None;
                println!("Goal cleared.");
            } else {
                if let Err(error) = ctx
                    .session_db
                    .set_goal_condition(&ctx.session_id, Some(args))
                    .await
                {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
                let event = Event::GoalSet {
                    condition: args.to_string(),
                    plan: None,
                    subtasks: Vec::new(),
                };
                if let Err(error) = ctx.session_db.append_event(&ctx.session_id, &event).await {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
                ctx.mind_palace.ingest(event);
                ctx.runtime_state.active_goal = Some(args.to_string());
                println!("◎ Goal set: {}", args);
            }
            SlashResult::Handled
        }

        // === Config & Model ===
        "model" => {
            if args.is_empty() || args == "list" {
                println!("Configured providers:");
                for p in &ctx.config.llm.providers {
                    println!(
                        "  {}: {} ({})",
                        p.name,
                        p.model,
                        api_format_label(&p.api_format)
                    );
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
                let event = Event::SessionModeSet {
                    mode: new_mode.clone(),
                };
                if let Err(error) = ctx.session_db.append_event(&ctx.session_id, &event).await {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
                ctx.mind_palace.ingest(event);
                ctx.runtime_session.mode = new_mode.clone();
                ctx.runtime_state.session_mode = new_mode.clone();
                println!("Mode switched to: {:?}", new_mode);
            }
            SlashResult::Handled
        }

        "config" => {
            if args.starts_with("set ") {
                println!(
                    "Config editing not yet supported in REPL. Edit {} directly.",
                    ctx.data_dir.join("config.yaml").display(),
                );
            } else {
                println!("Config: {}", ctx.data_dir.join("config.yaml").display());
                println!("  Providers: {}", ctx.config.llm.providers.len());
                println!("  Output dir: {}", ctx.config.output_dir);
                println!(
                    "  Browser: {}",
                    if ctx.config.browser.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
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
                let registry = Arc::new(build_tool_registry(
                    &ctx.config,
                    Some(ctx.session_db.clone()),
                    Some(ctx.memory_store.clone()),
                    Some(ctx.llm.clone()),
                    Some(ctx.session_id.clone()),
                ).await);
                let mut selector = Selector::new();
                for wf in workflows::create_builtin_workflows(
                    ctx.llm.clone(),
                    registry.clone(),
                    ctx.guards.clone(),
                ) {
                    selector.register(wf);
                }
                ctx.registry = registry;
                ctx.selector = selector;
                println!(
                    "MCP reloaded. Available tools: {}",
                    ctx.registry.definitions().len()
                );
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
        _ => SlashResult::NotHandled(input.to_string()),
    }
}

pub async fn list_sessions() -> anyhow::Result<()> {
    let data_dir = holmes_data_dir();
    std::fs::create_dir_all(&data_dir)?;
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
        let status = if s.ended_at.is_some() {
            "ended"
        } else {
            "active"
        };
        let title = s.title.as_deref().unwrap_or("(untitled)");
        println!("  {}  {:<8}  {}", &s.id[..8.min(s.id.len())], status, title);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_tool_output_summarizes_command_json() {
        let content = serde_json::json!({
            "exit_code": 0,
            "stderr": "",
            "stdout": "hello\nworld\n"
        })
        .to_string();

        assert_eq!(
            folded_tool_output_summary(&content),
            "output folded (exit 0, stdout 12 chars, stderr 0 chars)"
        );
    }

    #[test]
    fn folded_tool_output_summarizes_plain_text_without_echoing_content() {
        let content = "secret-ish verbose output\nsecond line";

        let summary = folded_tool_output_summary(content);

        assert_eq!(summary, "output folded (37 chars, 2 lines)");
        assert!(!summary.contains("secret-ish"));
    }

    #[test]
    fn failed_tool_preview_prefers_stderr_and_truncates() {
        let content = serde_json::json!({
            "exit_code": 1,
            "stderr": "error ".repeat(80),
            "stdout": "stdout should not be previewed"
        })
        .to_string();

        let preview = folded_tool_output_preview(&content).expect("preview");

        assert!(preview.starts_with("error error"));
        assert!(preview.ends_with("..."));
        assert!(!preview.contains("stdout should not"));
    }

    #[test]
    fn long_call_ids_are_shortened() {
        assert_eq!(
            short_call_id("call_00_bEdugtIsXGTPxbMpUlD08092"),
            "call_00_bEdu...D08092"
        );
    }
}
