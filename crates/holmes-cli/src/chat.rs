use anyhow::Context;
use chrono::Utc;
use holmes_core::config::{
    resolve_attack_model_provider, ApiFormat, Config, GuardConfig, HolmesConfig, PermissionMode,
    ResolvedModel,
};
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
use holmes_session::memory_store::MemoryStore;
use holmes_session::selector::Selector;
use holmes_session::{CreateSessionParams, SessionDB, SessionStore};
use holmes_tools::ToolRegistry;
use reedline::{
    default_emacs_keybindings, Completer, Emacs, FileBackedHistory, IdeMenu, KeyCode, KeyModifiers,
    MenuBuilder, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, Suggestion,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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

pub(crate) fn parse_mode(s: &str) -> SessionMode {
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
    session_db: Option<Arc<dyn SessionStore>>,
    memory_store: Option<Arc<MemoryStore>>,
    llm: Option<Arc<LlmClient>>,
    session_id: Option<String>,
    browser: Option<Arc<holmes_browser::BrowserManager>>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    let runner = if let (Some(db), Some(ms), Some(l), Some(sid)) =
        (session_db, memory_store, llm, session_id)
    {
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

    holmes_tools::builtin::register_all(&mut registry, config, runner, browser);
    holmes_tools::mcp::register_mcp_tools(&mut registry, &config.mcp.servers).await;
    registry
}

fn replay_events_into_runtime(
    session: &mut RuntimeSession,
    mind_palace: &mut MindPalace,
    events: &[StoredEvent],
) {
    use holmes_core::tool_types::{FunctionCall, Role, ToolCall};

    let mut pending_tool_calls = Vec::<(String, String)>::new(); // (tool_name, call_id)

    for se in events {
        let event = se.event.clone();
        mind_palace.ingest(event.clone());
        match event {
            Event::UserMessage { content, .. } => {
                session.messages.push(Message::user(content));
            }
            Event::Thinking { content, .. } => {
                session.messages.push(Message::assistant(content));
            }
            Event::ToolCall {
                name, arguments, ..
            } => {
                let call_id = format!("replayed-{}", uuid::Uuid::new_v4());
                let tool_call = ToolCall {
                    id: call_id.clone(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: name.clone(),
                        arguments: arguments.to_string(),
                    },
                };
                pending_tool_calls.push((name.clone(), call_id));

                if let Some(last_msg) = session.messages.last_mut() {
                    if last_msg.role == Role::Assistant {
                        if let Some(ref mut tc) = last_msg.tool_calls {
                            tc.push(tool_call);
                        } else {
                            last_msg.tool_calls = Some(vec![tool_call]);
                        }
                    } else {
                        session
                            .messages
                            .push(Message::assistant_with_tool_calls(vec![tool_call]));
                    }
                } else {
                    session
                        .messages
                        .push(Message::assistant_with_tool_calls(vec![tool_call]));
                }
            }
            Event::ToolResult { name, content, .. } => {
                let matched_idx = pending_tool_calls
                    .iter()
                    .position(|(tname, _)| tname == &name);
                let call_id = if let Some(idx) = matched_idx {
                    pending_tool_calls.remove(idx).1
                } else {
                    format!("replayed-orphan-{}", session.messages.len())
                };
                session
                    .messages
                    .push(Message::tool_result(call_id, name, content));
            }
            Event::SessionModeSet { mode, .. } => {
                session.mode = mode;
            }
            _ => {}
        }
    }
}

/// Rebuild a runtime context from the session's semantic event stream, falling
/// back to legacy message replay when the session predates semantic startup
/// metadata. The returned bool is `true` when semantic replay succeeded.
async fn load_session_runtime_from_store(
    session_db: Arc<dyn SessionStore>,
    memory_store: Arc<MemoryStore>,
    session_id: &str,
    fallback_mode: SessionMode,
    fallback_system_prompt: &str,
) -> anyhow::Result<(RuntimeSession, MindPalace, bool)> {
    let replayed = session_db.replay_session_context(session_id).await?;
    let events = session_db.get_events(session_id).await?;

    if replayed.semantic_complete {
        let mut mind_palace = MindPalace::new(session_db, memory_store);
        for stored in &events {
            mind_palace.ingest(stored.event.clone());
        }
        Ok((replayed.session, mind_palace, true))
    } else {
        let mut mind_palace = MindPalace::new(session_db, memory_store);
        let mut legacy = RuntimeSession::new(session_id.to_string(), fallback_mode)
            .with_system_prompt(fallback_system_prompt);
        replay_events_into_runtime(&mut legacy, &mut mind_palace, &events);
        Ok((legacy, mind_palace, false))
    }
}

/// Mutable runtime context for the chat REPL — shared with all slash command handlers.
pub struct ChatContext {
    pub session_id: String,
    pub session_db: Arc<dyn SessionStore>,
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
    pub browser: Option<Arc<holmes_browser::BrowserManager>>,
}

pub(crate) fn save_config(ctx: &ChatContext) -> anyhow::Result<()> {
    let path = ctx.data_dir.join("config.yaml");
    let yaml = serde_yaml::to_string(&ctx.config)?;
    std::fs::write(&path, yaml)
        .with_context(|| format!("failed to write config at {}", path.display()))?;
    Ok(())
}

fn rebuild_selector(ctx: &mut ChatContext) {
    let mut selector = Selector::new();
    for wf in workflows::create_builtin_workflows(
        ctx.llm.clone(),
        ctx.registry.clone(),
        ctx.guards.clone(),
    ) {
        selector.register(wf);
    }
    ctx.selector = selector;
}

pub(crate) fn refresh_guard_chain(ctx: &mut ChatContext) {
    ctx.runtime_guards = GuardChain::from_config(&ctx.config.guards);
    ctx.guards = Arc::new(Mutex::new(GuardChain::from_config(&ctx.config.guards)));
    rebuild_selector(ctx);
}

pub(crate) async fn load_session_runtime(
    ctx: &ChatContext,
    session_id: &str,
    mode: SessionMode,
) -> anyhow::Result<(RuntimeSession, MindPalace)> {
    let events = ctx.session_db.get_events(session_id).await?;
    let mut mind_palace = MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
    let mut runtime_session =
        RuntimeSession::new(session_id.to_string(), mode).with_system_prompt(&ctx.system_prompt);
    replay_events_into_runtime(&mut runtime_session, &mut mind_palace, &events);
    Ok((runtime_session, mind_palace))
}

fn print_session_tree(sessions: &[SessionSummary], current_id: &str) {
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }

    let mut children: BTreeMap<Option<String>, Vec<usize>> = BTreeMap::new();
    for (idx, session) in sessions.iter().enumerate() {
        children
            .entry(session.parent_session_id.clone())
            .or_default()
            .push(idx);
    }
    for indexes in children.values_mut() {
        indexes.sort_by_key(|idx| {
            std::cmp::Reverse(
                sessions[*idx]
                    .last_active
                    .unwrap_or(sessions[*idx].started_at),
            )
        });
    }

    println!("Session tree:");
    let mut visited = BTreeSet::new();
    if let Some(roots) = children.get(&None) {
        for (pos, idx) in roots.iter().enumerate() {
            print_session_tree_node(
                sessions,
                &children,
                *idx,
                "",
                pos + 1 == roots.len(),
                current_id,
                &mut visited,
            );
        }
    }

    for (idx, session) in sessions.iter().enumerate() {
        if !visited.contains(&session.id) {
            let last = idx + 1 == sessions.len();
            print_session_tree_node(sessions, &children, idx, "", last, current_id, &mut visited);
        }
    }

    println!(
        "\nUse /resume <id> to switch, /tree events to inspect this session, or /tree fork <event_index> [title]."
    );
}

fn print_session_tree_node(
    sessions: &[SessionSummary],
    children: &BTreeMap<Option<String>, Vec<usize>>,
    idx: usize,
    prefix: &str,
    is_last: bool,
    current_id: &str,
    visited: &mut BTreeSet<String>,
) {
    let session = &sessions[idx];
    if !visited.insert(session.id.clone()) {
        return;
    }

    let connector = if prefix.is_empty() {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };
    let current = if session.id == current_id { "→" } else { " " };
    let title = session.title.as_deref().unwrap_or("(untitled)");
    let preview = session.preview.as_deref().unwrap_or("");
    let active_at = session.last_active.unwrap_or(session.started_at);
    println!(
        "{}{}{} {}  {:?}  {} msg  {}  {}",
        prefix,
        connector,
        current,
        short_id(&session.id),
        session.mode,
        session.message_count,
        format_relative_time(active_at),
        truncate_chars(title, 36),
    );
    if !preview.trim().is_empty() {
        println!(
            "{}{}    {}",
            prefix,
            if prefix.is_empty() { "" } else { "  " },
            truncate_chars(preview.trim(), 72),
        );
    }

    let child_prefix = if prefix.is_empty() {
        String::new()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };
    if let Some(child_indexes) = children.get(&Some(session.id.clone())) {
        for (pos, child_idx) in child_indexes.iter().enumerate() {
            print_session_tree_node(
                sessions,
                children,
                *child_idx,
                &child_prefix,
                pos + 1 == child_indexes.len(),
                current_id,
                visited,
            );
        }
    }
}

fn print_event_timeline(events: &[StoredEvent], limit: usize) {
    if events.is_empty() {
        println!("No events recorded in this session.");
        return;
    }

    let start = events.len().saturating_sub(limit);
    println!("Current session events:");
    for event in &events[start..] {
        println!(
            "  {:>4}  {:<24} {}",
            event.event_index,
            event_type_label(&event.event),
            truncate_chars(&event_summary(&event.event), 92),
        );
    }
    if start > 0 {
        println!("  ... {} earlier event(s) hidden", start);
    }
}

pub(crate) fn event_type_label(event: &Event) -> &'static str {
    match event {
        Event::UserMessage { .. } => "user",
        Event::Thinking { .. } => "assistant",
        Event::ToolCall { .. } => "tool_call",
        Event::ToolResult { .. } => "tool_result",
        Event::ToolBlocked { .. } => "tool_blocked",
        Event::TurnComplete { .. } => "turn_complete",
        Event::GoalSet { .. } => "goal_set",
        Event::GoalEvaluated { .. } => "goal_evaluated",
        Event::GoalCleared { .. } => "goal_cleared",
        Event::EvidenceObserved { .. } => "evidence",
        Event::FactRecorded { .. } => "fact",
        Event::HypothesisProposed { .. } => "hypothesis",
        Event::HypothesisSupported { .. } => "hypothesis_supported",
        Event::HypothesisContradicted { .. } => "hypothesis_contradicted",
        Event::HypothesisRejected { .. } => "hypothesis_rejected",
        Event::HypothesisConfirmed { .. } => "hypothesis_confirmed",
        Event::ConclusionDrawn { .. } => "conclusion",
        Event::ReflectionRecorded { .. } => "reflection",
        Event::MemoryStored { .. } => "memory_stored",
        Event::MemoryRecalled { .. } => "memory_recalled",
        Event::CompressionApplied { .. } => "compression",
        Event::ContextSnapshotTaken { .. } => "snapshot",
        Event::ReportGenerated { .. } => "report",
        Event::SubAgentSpawned { .. } => "subagent_spawned",
        Event::SubAgentCompleted { .. } => "subagent_done",
        Event::SubAgentProgress { .. } => "subagent_progress",
        _ => "event",
    }
}

pub(crate) fn event_summary(event: &Event) -> String {
    match event {
        Event::UserMessage { content, .. } => content.clone(),
        Event::Thinking { content, .. } => content.clone(),
        Event::ToolCall {
            name, arguments, ..
        } => format!("{name} {arguments}"),
        Event::ToolResult {
            name,
            success,
            content,
            ..
        } => format!(
            "{name} [{}] {content}",
            if *success { "ok" } else { "failed" }
        ),
        Event::ToolBlocked {
            tool_name, reason, ..
        } => format!("{tool_name}: {reason}"),
        Event::GoalSet { condition, .. } => condition.clone(),
        Event::GoalEvaluated {
            satisfied, reason, ..
        } => format!("satisfied={satisfied}: {reason}"),
        Event::GoalCleared { reason } => reason.clone(),
        Event::EvidenceObserved {
            evidence_id,
            summary,
            ..
        } => {
            format!("{evidence_id}: {summary}")
        }
        Event::FactRecorded {
            fact_id, statement, ..
        } => format!("{fact_id}: {statement}"),
        Event::HypothesisProposed {
            hypothesis_id,
            statement,
            ..
        } => format!("{hypothesis_id}: {statement}"),
        Event::HypothesisSupported {
            hypothesis_id,
            evidence_id,
            ..
        } => format!("{hypothesis_id} supported by {evidence_id}"),
        Event::HypothesisContradicted {
            hypothesis_id,
            evidence_id,
            ..
        } => format!("{hypothesis_id} contradicted by {evidence_id}"),
        Event::HypothesisRejected {
            hypothesis_id,
            reason,
        } => format!("{hypothesis_id}: {reason}"),
        Event::HypothesisConfirmed {
            hypothesis_id,
            conclusion,
            ..
        } => format!("{hypothesis_id}: {conclusion}"),
        Event::ConclusionDrawn { conclusion, .. } => conclusion.clone(),
        Event::ReflectionRecorded {
            diagnosis,
            lessons_learned,
            ..
        } => format!("{diagnosis}; next: {lessons_learned}"),
        Event::MemoryStored { content, .. } => content.clone(),
        Event::MemoryRecalled { memory_ids, .. } => format!("{} memory item(s)", memory_ids.len()),
        Event::CompressionApplied {
            before_count,
            after_count,
            summary,
            ..
        } => format!("{before_count} -> {after_count}: {summary}"),
        Event::ContextSnapshotTaken { summary, .. } => summary.clone(),
        Event::ReportGenerated { file_path, .. } => file_path.clone(),
        Event::TurnComplete { event_range, .. } => {
            format!("events {}..{}", event_range.0, event_range.1)
        }
        _ => event.content_text(),
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn parse_bool_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" | "enable" | "enabled" => Some(true),
        "off" | "false" | "no" | "0" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

fn print_permissions(ctx: &ChatContext) {
    let permissions = &ctx.config.permissions;
    println!("Permissions:");
    println!("  Mode: {}", permissions.mode);
    println!(
        "  Auto-approve read-only tools: {}",
        if permissions.auto_approve_read_only {
            "on"
        } else {
            "off"
        }
    );
    println!(
        "  Allowlist: {}",
        if permissions.allowed_tools.is_empty() {
            "(empty: all tools are eligible)".into()
        } else {
            permissions.allowed_tools.join(", ")
        }
    );
    println!(
        "  Denylist:  {}",
        if permissions.disallowed_tools.is_empty() {
            "(empty)".into()
        } else {
            permissions.disallowed_tools.join(", ")
        }
    );
    println!("\nModes:");
    println!("  default      read-only tools auto-run; other tools remain policy-controlled");
    println!("  plan         block all tools; Holmes must plan or ask Watson");
    println!("  read-only    allow only read-only tools");
    println!("  accept-edits allow mutating tools while guards still run");
    println!("  dont-ask     non-interactive autonomy; policy lists still apply");
    println!("  bypass       maximum autonomy; GuardChain still applies");
}

fn print_guards(config: &GuardConfig) {
    println!("GuardChain:");
    println!(
        "  immutable-field    {}  blocks overwriting protected state",
        on_off(config.immutable_field)
    );
    println!(
        "  dangerous-command  {}  blocks obviously destructive shell actions",
        on_off(config.dangerous_command)
    );
    println!(
        "  repetition         {}  blocks repeated low-value tool loops",
        on_off(config.repetition)
    );
    println!(
        "  attack-surface     {}  extracts ports, services, endpoints, credentials",
        on_off(config.attack_surface)
    );
    println!(
        "  evidence-extractor {}  extracts findings and evidence bundles",
        on_off(config.evidence_extractor)
    );
    println!(
        "  skeptic-gate       {}  keeps weak findings from becoming conclusions",
        on_off(config.skeptic_gate)
    );
    println!(
        "  failure-tracker    {}  records failed actions for reflection",
        on_off(config.failure_tracker)
    );
    println!(
        "  soft404            {}  detects false-positive HTTP probes",
        on_off(config.soft404)
    );
    println!(
        "  read-state-seeding {}  lets read tools seed guard state safely",
        on_off(config.read_state_seeding)
    );
    println!("  repetition-window  {}", config.repetition_window);
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on "
    } else {
        "off"
    }
}

fn set_guard_flag(config: &mut GuardConfig, name: &str, enabled: bool) -> Option<&'static str> {
    match normalize_guard_name(name).as_str() {
        "immutable_field" => {
            config.immutable_field = enabled;
            Some("immutable-field")
        }
        "dangerous_command" => {
            config.dangerous_command = enabled;
            Some("dangerous-command")
        }
        "repetition" => {
            config.repetition = enabled;
            Some("repetition")
        }
        "attack_surface" => {
            config.attack_surface = enabled;
            Some("attack-surface")
        }
        "evidence_extractor" => {
            config.evidence_extractor = enabled;
            Some("evidence-extractor")
        }
        "skeptic_gate" => {
            config.skeptic_gate = enabled;
            Some("skeptic-gate")
        }
        "failure_tracker" => {
            config.failure_tracker = enabled;
            Some("failure-tracker")
        }
        "soft404" => {
            config.soft404 = enabled;
            Some("soft404")
        }
        "read_state_seeding" => {
            config.read_state_seeding = enabled;
            Some("read-state-seeding")
        }
        _ => None,
    }
}

fn set_all_guard_flags(config: &mut GuardConfig, enabled: bool) {
    config.immutable_field = enabled;
    config.dangerous_command = enabled;
    config.repetition = enabled;
    config.attack_surface = enabled;
    config.evidence_extractor = enabled;
    config.skeptic_gate = enabled;
    config.failure_tracker = enabled;
    config.soft404 = enabled;
    config.read_state_seeding = enabled;
}

fn normalize_guard_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .replace('-', "_")
        .replace(' ', "_")
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

pub(crate) fn folded_tool_output_summary(content: &str) -> String {
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

pub(crate) fn truncate_chars(content: &str, max_chars: usize) -> String {
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

pub(crate) async fn run_runtime_input_with_sink<S: RuntimeSink>(
    ctx: &mut ChatContext,
    input: String,
    oneshot: bool,
    sink: &mut S,
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
    if ctx.browser.is_some() {
        runtime
            .context_mut()
            .middlewares
            .push(Arc::new(holmes_runtime::middleware::BrowserReadOnlyMiddleware));
    }
    let result = if oneshot {
        runtime.run_oneshot(input, sink).await
    } else {
        runtime.run_turn(input, sink).await
    };
    let runtime_context = runtime.into_context();

    ctx.session_id = runtime_context.session_id.clone();
    ctx.runtime_session = runtime_context.session;
    ctx.mind_palace = runtime_context.mind_palace;
    ctx.runtime_guards = runtime_context.guards;
    ctx.runtime_state = runtime_context.state;

    result.map_err(Into::into)
}

pub(crate) async fn run_runtime_input(
    ctx: &mut ChatContext,
    input: String,
    oneshot: bool,
) -> anyhow::Result<TurnOutcome> {
    let mut sink = CliRuntimeSink;
    run_runtime_input_with_sink(ctx, input, oneshot, &mut sink).await
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

pub(crate) async fn drain_queued_turns(ctx: &mut ChatContext) {
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
    let fallback_mode = session_record
        .as_ref()
        .map(|session| session.mode.clone())
        .unwrap_or_else(|| ctx.runtime_session.mode.clone());
    let (runtime_session, mind_palace, semantic_complete) = load_session_runtime_from_store(
        ctx.session_db.clone(),
        ctx.memory_store.clone(),
        &ctx.session_id,
        fallback_mode,
        &ctx.system_prompt,
    )
    .await?;
    if !semantic_complete {
        eprintln!(
            "⚠ Session {} is missing semantic startup metadata; used legacy replay fallback",
            &ctx.session_id[..8.min(ctx.session_id.len())]
        );
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

fn active_tool_names(registry: &ToolRegistry) -> Vec<String> {
    let mut names = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.function.name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

/// Append the semantic startup metadata events (SessionCreated, SystemPromptSet,
/// ModeSet, ModelSet) for a freshly created session. Returns the shared timestamp
/// so the caller can emit the matching ActiveToolsSet once the registry is built.
async fn append_startup_metadata_events(
    session_db: &dyn SessionStore,
    id: &str,
    title: Option<String>,
    mode: SessionMode,
    resolved_model: Option<ResolvedModel>,
    system_prompt: String,
    parent_id: Option<String>,
    fork_point: Option<u64>,
    tags: Vec<String>,
) -> anyhow::Result<chrono::DateTime<Utc>> {
    let now = Utc::now();
    session_db
        .append_event(
            id,
            &Event::SessionCreated {
                id: id.to_string(),
                title,
                mode: mode.clone(),
                model: resolved_model
                    .as_ref()
                    .map(|resolved| resolved.model.clone()),
                system_prompt: Some(system_prompt.clone()),
                parent_id,
                fork_point,
                created_at: now,
                tags,
            },
        )
        .await?;
    session_db
        .append_event(
            id,
            &Event::SessionSystemPromptSet {
                prompt_hash: holmes_core::stable_prompt_hash(&system_prompt),
                content: system_prompt,
                source: "startup".into(),
                timestamp: now,
            },
        )
        .await?;
    session_db
        .append_event(
            id,
            &Event::SessionModeSet {
                mode,
                source: Some("startup".into()),
                timestamp: Some(now),
            },
        )
        .await?;
    session_db
        .append_event(
            id,
            &Event::SessionModelSet {
                model: resolved_model
                    .as_ref()
                    .map(|resolved| resolved.model.clone())
                    .unwrap_or_else(|| "unknown".into()),
                provider: resolved_model.and_then(|resolved| resolved.provider),
                source: "startup".into(),
                timestamp: now,
            },
        )
        .await?;
    Ok(now)
}

async fn append_active_tools_startup_metadata_event(
    session_db: &dyn SessionStore,
    id: &str,
    tool_names: Vec<String>,
    timestamp: chrono::DateTime<Utc>,
) -> anyhow::Result<()> {
    session_db
        .append_event(
            id,
            &Event::ActiveToolsSet {
                tool_names,
                source: "startup".into(),
                timestamp,
            },
        )
        .await?;
    Ok(())
}

/// Persist a branch summary on a freshly forked child session.
///
/// Reads the parent event window `[from_event_index, to_event_index]`, builds a
/// deterministic static fallback summary, and appends a `BranchSummary` event to
/// the child so replay surfaces the parent path's context as a non-system message.
pub(crate) async fn append_branch_summary(
    ctx: &ChatContext,
    new_session_id: &str,
    from_event_index: u64,
    to_event_index: u64,
    reason: &str,
) -> anyhow::Result<()> {
    let events = ctx.session_db.get_events(&ctx.session_id).await?;
    let window: Vec<_> = events
        .into_iter()
        .filter(|e| e.event_index >= from_event_index && e.event_index <= to_event_index)
        .collect();
    let summary = holmes_runtime::summary::static_branch_summary(&window, reason);
    ctx.session_db
        .append_event(
            new_session_id,
            &Event::BranchSummary {
                from_event_index,
                to_event_index,
                summary,
                reason: reason.to_string(),
                method: holmes_core::SummaryMethod::StaticFallback,
                timestamp: Utc::now(),
            },
        )
        .await?;
    Ok(())
}

async fn append_active_tools_event_for_registry(
    session_db: &dyn SessionStore,
    session_id: &str,
    registry: &ToolRegistry,
    source: &str,
) -> anyhow::Result<()> {
    session_db
        .append_event(
            session_id,
            &Event::ActiveToolsSet {
                tool_names: active_tool_names(registry),
                source: source.into(),
                timestamp: Utc::now(),
            },
        )
        .await?;
    Ok(())
}

async fn create_fresh_runtime_session(
    session_db: Arc<dyn SessionStore>,
    memory_store: Arc<MemoryStore>,
    llm: Arc<LlmClient>,
    config: &HolmesConfig,
    data_dir: &Path,
    browser_out: &mut Option<Arc<holmes_browser::BrowserManager>>,
    mode: SessionMode,
    resolved_model: Option<ResolvedModel>,
    system_prompt: String,
) -> anyhow::Result<(String, RuntimeSession, MindPalace, Arc<ToolRegistry>)> {
    let session = session_db
        .create_session(CreateSessionParams {
            id: None,
            title: None,
            mode: Some(mode.clone()),
            model: resolved_model
                .as_ref()
                .map(|resolved| resolved.model.clone()),
            system_prompt: Some(system_prompt.clone()),
            parent_session_id: None,
            fork_point: None,
            source: Some("cli".into()),
            tags: vec![],
        })
        .await?;
    let session_id = session.id.clone();
    let startup_timestamp = match append_startup_metadata_events(
        session_db.as_ref(),
        &session_id,
        session.title,
        mode.clone(),
        resolved_model,
        system_prompt.clone(),
        None,
        None,
        session.tags,
    )
    .await
    {
        Ok(timestamp) => timestamp,
        Err(error) => {
            session_db
                .end_session(&session_id, EndReason::Error)
                .await
                .ok();
            return Err(error);
        }
    };
    let browser: Option<Arc<holmes_browser::BrowserManager>> = if config.browser.enabled {
        let sessions_dir = data_dir.join("sessions");
        match holmes_browser::BrowserManager::new(
            &session_id,
            &sessions_dir,
            config.browser.clone(),
        ) {
            Ok(mgr) => Some(Arc::new(mgr)),
            Err(e) => {
                eprintln!("Warning: browser disabled: {e}");
                None
            }
        }
    } else {
        None
    };
    let registry = Arc::new(
        build_tool_registry(
            config,
            Some(session_db.clone()),
            Some(memory_store.clone()),
            Some(llm),
            Some(session_id.clone()),
            browser.clone(),
        )
        .await,
    );
    if let Err(error) = append_active_tools_startup_metadata_event(
        session_db.as_ref(),
        &session_id,
        active_tool_names(&registry),
        startup_timestamp,
    )
    .await
    {
        session_db
            .end_session(&session_id, EndReason::Error)
            .await
            .ok();
        return Err(error);
    }
    let mind_palace = MindPalace::new(session_db, memory_store);
    *browser_out = browser.clone();
    Ok((
        session_id.clone(),
        RuntimeSession::new(session_id, mode).with_system_prompt(&system_prompt),
        mind_palace,
        registry,
    ))
}

pub(crate) struct ChatStartup {
    pub ctx: ChatContext,
    pub is_resume: bool,
}

pub(crate) async fn create_chat_context(
    resume_id: Option<String>,
    continue_last: bool,
    model: Option<String>,
    mode_str: String,
    announce: bool,
) -> anyhow::Result<Option<ChatStartup>> {
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
        return Ok(None);
    };
    let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mode = parse_mode(&mode_str);
    let system_prompt = build_system_prompt(SYSTEM_PROMPT, &config, &project_dir, mode.clone());

    let db_path = data_dir.join("holmes.db");
    let session_db: Arc<dyn SessionStore> = Arc::new(SessionDB::open(&db_path).await?);

    let memory_path = data_dir.join("memory.db");
    let memory_store = Arc::new(MemoryStore::open(&memory_path).await?);

    let guards = Arc::new(Mutex::new(GuardChain::from_config(&config.guards)));
    let runtime_guards = GuardChain::from_config(&config.guards);
    let llm = Arc::new(LlmClient::new(&config));
    let startup_model = resolve_attack_model_provider(&config, model);

    // Browser manager (lazy-launches on first action). Only fresh sessions
    // populate this; resume/continue currently run without a browser.
    let mut browser: Option<Arc<holmes_browser::BrowserManager>> = None;

    // Create RuntimeSession
    let (session_id, runtime_session, mind_palace, registry, is_resume) = if let Some(id) =
        resume_id
    {
        let (session, mp, semantic_complete) = load_session_runtime_from_store(
            session_db.clone(),
            memory_store.clone(),
            &id,
            mode.clone(),
            &system_prompt,
        )
        .await?;
        if announce {
            if !semantic_complete {
                eprintln!(
                    "⚠ Session {} is missing semantic startup metadata; used legacy replay fallback",
                    &id[..8.min(id.len())]
                );
            }
            eprintln!("↻ Resumed session {}", &id[..8.min(id.len())]);
        }
        let registry = Arc::new(
            build_tool_registry(
                &config,
                Some(session_db.clone()),
                Some(memory_store.clone()),
                Some(llm.clone()),
                Some(id.clone()),
            None,
            )
            .await,
        );
        (id, session, mp, registry, true)
    } else if continue_last {
        let filter = SessionFilter {
            limit: Some(1),
            ..Default::default()
        };
        let sessions = session_db.list_sessions(&filter).await?;
        if let Some(s) = sessions.first() {
            let (session, mp, semantic_complete) = load_session_runtime_from_store(
                session_db.clone(),
                memory_store.clone(),
                &s.id,
                mode.clone(),
                &system_prompt,
            )
            .await?;
            if announce {
                if !semantic_complete {
                    eprintln!(
                        "⚠ Session {} is missing semantic startup metadata; used legacy replay fallback",
                        &s.id[..8.min(s.id.len())]
                    );
                }
                eprintln!("↻ Continued session {}", &s.id[..8.min(s.id.len())]);
            }
            let registry = Arc::new(
                build_tool_registry(
                    &config,
                    Some(session_db.clone()),
                    Some(memory_store.clone()),
                    Some(llm.clone()),
                    Some(s.id.clone()),
                None,
                )
                .await,
            );
            (s.id.clone(), session, mp, registry, true)
        } else {
            let (session_id, runtime_session, mind_palace, registry) =
                create_fresh_runtime_session(
                    session_db.clone(),
                    memory_store.clone(),
                    llm.clone(),
                    &config,
                    &data_dir,
                    &mut browser,
                    mode.clone(),
                    startup_model.clone(),
                    system_prompt.clone(),
                )
                .await?;
            (session_id, runtime_session, mind_palace, registry, false)
        }
    } else {
        let (session_id, runtime_session, mind_palace, registry) = create_fresh_runtime_session(
            session_db.clone(),
            memory_store.clone(),
            llm.clone(),
            &config,
            &data_dir,
            &mut browser,
            mode.clone(),
            startup_model.clone(),
            system_prompt.clone(),
        )
        .await?;
        (session_id, runtime_session, mind_palace, registry, false)
    };

    let mut selector = Selector::new();
    for wf in workflows::create_builtin_workflows(llm.clone(), registry.clone(), guards.clone()) {
        selector.register(wf);
    }

    let mut runtime_state = RuntimeState::new(runtime_session.mode.clone());
    if let Some(session_record) = session_db.get_session(&session_id).await? {
        runtime_state.active_goal = session_record.goal_condition;
    }
    let ctx = ChatContext {
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
        browser,
    };

    Ok(Some(ChatStartup { ctx, is_resume }))
}

pub async fn run_chat(
    resume_id: Option<String>,
    continue_last: bool,
    query: Option<String>,
    model: Option<String>,
    mode_str: String,
) -> anyhow::Result<()> {
    let Some(ChatStartup { mut ctx, is_resume }) =
        create_chat_context(resume_id, continue_last, model, mode_str, true).await?
    else {
        return Ok(());
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
    let initial_sessions = ctx
        .session_db
        .list_sessions(&SessionFilter {
            limit: Some(100),
            ..Default::default()
        })
        .await
        .unwrap_or_default();

    #[derive(Clone)]
    struct CommandCompleter {
        commands: Vec<(String, String)>,
        sessions: Vec<SessionSummary>,
    }

    impl Completer for CommandCompleter {
        fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
            let mut suggestions = Vec::new();
            if line.starts_with("/resume ")
                || (line.starts_with("/resume")
                    && line.len() > 7
                    && line.chars().nth(7) == Some(' '))
            {
                let prefix = if line[..pos].len() > 8 {
                    &line[8..pos]
                } else {
                    ""
                };
                for (i, s) in self.sessions.iter().enumerate() {
                    let num = (i + 1).to_string();
                    let title = s.title.as_deref().unwrap_or("-");
                    let preview = s.preview.as_deref().unwrap_or("");
                    if num.starts_with(prefix)
                        || s.id.starts_with(prefix)
                        || title.to_lowercase().contains(&prefix.to_lowercase())
                    {
                        suggestions.push(Suggestion {
                            value: num.clone(),
                            description: Some(format!("{} - {}", title, preview)),
                            extra: None,
                            span: Span::new(8, pos),
                            append_whitespace: false,
                            match_indices: None,
                            display_override: Some(format!(
                                "{}  {:<20}  {}",
                                num,
                                title,
                                &s.id[..8.min(s.id.len())]
                            )),
                            style: None,
                        });
                    }
                }
            } else if line.starts_with('/') {
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
        sessions: initial_sessions,
    });

    let completion_menu = Box::new(IdeMenu::default().with_name("completion_menu"));

    let history_path = ctx.data_dir.join("history.txt");
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
        fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed(&self.left)
        }
        fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed("")
        }
        fn render_prompt_indicator(
            &self,
            _: reedline::PromptEditMode,
        ) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed("")
        }
        fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed("::: ")
        }
        fn render_prompt_history_search_indicator(
            &self,
            _: reedline::PromptHistorySearch,
        ) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed("? ")
        }
    }

    loop {
        let prompt_str = if ctx.runtime_session.message_count() <= 1 {
            "> "
        } else {
            "» "
        };
        let prompt = SimplePrompt {
            left: prompt_str.to_string(),
        };

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
                SlashResult::NewSession(rs, mp, new_id, registry) => {
                    ctx.runtime_session = rs;
                    ctx.mind_palace = mp;
                    ctx.session_id = new_id;
                    ctx.registry = registry;
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
    session_db: &dyn SessionStore,
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

pub(crate) enum SlashResult {
    Quit,
    Handled,
    NewSession(RuntimeSession, MindPalace, String, Arc<ToolRegistry>),
    NotHandled(String),
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn handle_slash_command(input: &str, ctx: &mut ChatContext) -> SlashResult {
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
            let model = resolve_attack_model_provider(&ctx.config, None);
            let mut new_browser: Option<Arc<holmes_browser::BrowserManager>> = None;
            match create_fresh_runtime_session(
                ctx.session_db.clone(),
                ctx.memory_store.clone(),
                ctx.llm.clone(),
                &ctx.config,
                &ctx.data_dir,
                &mut new_browser,
                ctx.runtime_session.mode.clone(),
                model,
                ctx.system_prompt.clone(),
            )
            .await
            {
                Ok((new_id, rs, mp, registry)) => {
                    println!("Started new session: {}", &new_id[..8.min(new_id.len())]);
                    ctx.browser = new_browser;
                    return SlashResult::NewSession(rs, mp, new_id, registry);
                }
                Err(error) => eprintln!("Error: {}", error),
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
                let filter = SessionFilter {
                    limit: Some(20),
                    ..Default::default()
                };
                match ctx.session_db.list_sessions(&filter).await {
                    Ok(sessions) => {
                        if sessions.is_empty() {
                            println!("No sessions found.");
                        } else {
                            println!("Recent sessions:\n");
                            println!(
                                "{:<4} {:<27} {:<51} {:<12} {}",
                                "#", "Title", "Preview", "Last Active", "ID"
                            );
                            println!("{}", "-".repeat(101));
                            for (i, s) in sessions.iter().enumerate() {
                                let num = i + 1;
                                let title = s.title.as_deref().unwrap_or("-");
                                let preview = s.preview.as_deref().unwrap_or("");

                                let truncated_title = if title.chars().count() > 25 {
                                    format!("{}...", title.chars().take(22).collect::<String>())
                                } else {
                                    title.to_string()
                                };
                                let truncated_preview = if preview.chars().count() > 48 {
                                    format!("{}...", preview.chars().take(45).collect::<String>())
                                } else {
                                    preview.to_string()
                                };

                                let relative_time = if let Some(dt) = s.last_active {
                                    format_relative_time(dt)
                                } else {
                                    format_relative_time(s.started_at)
                                };

                                println!(
                                    "{:<4} {:<27} {:<51} {:<12} {}",
                                    num, truncated_title, truncated_preview, relative_time, s.id
                                );
                            }
                            println!("\nUse /resume <number>, /resume <session id>, or /resume <session title> to continue.");
                            println!("Example: /resume 2");
                        }
                    }
                    Err(e) => {
                        println!("Failed to list sessions: {}", e);
                    }
                }
                return SlashResult::Handled;
            }
            let filter = SessionFilter {
                limit: Some(100),
                ..Default::default()
            };
            match ctx.session_db.list_sessions(&filter).await {
                Ok(sessions) => {
                    let target = if let Ok(num) = args.parse::<usize>() {
                        if num >= 1 && num <= sessions.len() {
                            Some(&sessions[num - 1])
                        } else {
                            None
                        }
                    } else {
                        sessions
                            .iter()
                            .find(|s| s.id.starts_with(args) || s.title.as_deref() == Some(args))
                    };

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
                        println!(
                            "↻ Resuming session {} ({}) and replaying history...",
                            &s.id[..8.min(s.id.len())],
                            s.title.as_deref().unwrap_or("untitled"),
                        );
                        replay_events_into_runtime(&mut rs, &mut mp, &events);
                        for se in &events {
                            match &se.event {
                                Event::UserMessage { content, .. } => {
                                    println!("\n> {}", content);
                                }
                                Event::Thinking { content, .. } => {
                                    print_holmes(content);
                                }
                                Event::ToolCall { name, .. } => {
                                    print_tool_started(name, None);
                                }
                                Event::ToolResult {
                                    name,
                                    success,
                                    content,
                                    ..
                                } => {
                                    print_tool_finished(name, *success, content);
                                }
                                Event::ToolBlocked {
                                    tool_name, reason, ..
                                } => {
                                    print_permission_decision(tool_name, false, reason);
                                }
                                Event::GoalSet { condition, .. } => {
                                    println!("  goal set: {}", condition);
                                }
                                Event::GoalEvaluated {
                                    satisfied, reason, ..
                                } => {
                                    println!(
                                        "  goal evaluated: satisfied={}, {}",
                                        satisfied, reason
                                    );
                                }
                                _ => {}
                            }
                        }
                        println!();
                        let registry = Arc::new(
                            build_tool_registry(
                                &ctx.config,
                                Some(ctx.session_db.clone()),
                                Some(ctx.memory_store.clone()),
                                Some(ctx.llm.clone()),
                                Some(s.id.clone()),
                            None,
                            )
                            .await,
                        );
                        return SlashResult::NewSession(rs, mp, s.id.clone(), registry);
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

        "tree" => {
            if args.is_empty() {
                match ctx
                    .session_db
                    .list_sessions(&SessionFilter {
                        include_children: true,
                        limit: Some(200),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(sessions) => print_session_tree(&sessions, &ctx.session_id),
                    Err(error) => eprintln!("Error: {}", error),
                }
                return SlashResult::Handled;
            }

            let mut parts = args.split_whitespace();
            match parts.next().unwrap_or_default() {
                "events" | "timeline" => {
                    let limit = parts
                        .next()
                        .and_then(|raw| raw.parse::<usize>().ok())
                        .unwrap_or(80);
                    match ctx.session_db.get_events(&ctx.session_id).await {
                        Ok(events) => print_event_timeline(&events, limit),
                        Err(error) => eprintln!("Error: {}", error),
                    }
                }
                "fork" | "branch" => {
                    let Some(index_raw) = parts.next() else {
                        println!("Usage: /tree fork <event_index> [title]");
                        return SlashResult::Handled;
                    };
                    let Ok(fork_point) = index_raw.parse::<u64>() else {
                        println!("Invalid event_index: {index_raw}");
                        return SlashResult::Handled;
                    };
                    let title = parts.collect::<Vec<_>>().join(" ");
                    let title = if title.trim().is_empty() {
                        format!("branch at event {fork_point}")
                    } else {
                        title
                    };
                    match ctx
                        .session_db
                        .fork_session(&ctx.session_id, fork_point, &title)
                        .await
                    {
                        Ok(new_session) => {
                            match load_session_runtime(
                                ctx,
                                &new_session.id,
                                new_session.mode.clone(),
                            )
                            .await
                            {
                                Ok((runtime_session, mind_palace)) => {
                                    println!(
                                        "Branched to {} at event_index={fork_point}.",
                                        short_id(&new_session.id)
                                    );
                                    let registry = Arc::new(
                                        build_tool_registry(
                                            &ctx.config,
                                            Some(ctx.session_db.clone()),
                                            Some(ctx.memory_store.clone()),
                                            Some(ctx.llm.clone()),
                                            Some(new_session.id.clone()),
                                        None,
                                        )
                                        .await,
                                    );
                                    return SlashResult::NewSession(
                                        runtime_session,
                                        mind_palace,
                                        new_session.id,
                                        registry,
                                    );
                                }
                                Err(error) => {
                                    eprintln!("Branch created but reload failed: {}", error)
                                }
                            }
                        }
                        Err(error) => eprintln!("Error: {}", error),
                    }
                }
                "help" => {
                    println!("Usage:");
                    println!("  /tree                         Show session tree");
                    println!("  /tree events [limit]          Show current session event timeline");
                    println!("  /tree fork <event_index> [title]");
                }
                other => {
                    println!("Unknown /tree action: {other}");
                    println!("Use /tree help for options.");
                }
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
            let fork_point = match ctx.session_db.get_events(&ctx.session_id).await {
                Ok(events) => events
                    .last()
                    .map(|event| event.event_index)
                    .unwrap_or_else(|| ctx.runtime_session.message_count() as u64),
                Err(error) => {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
            };
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
                    let branch_metadata = append_startup_metadata_events(
                        ctx.session_db.as_ref(),
                        &new_session.id,
                        new_session.title.clone(),
                        new_session.mode.clone(),
                        new_session.model.clone().map(|model| ResolvedModel {
                            model,
                            provider: resolve_attack_model_provider(&ctx.config, None)
                                .and_then(|resolved| resolved.provider),
                        }),
                        new_session
                            .system_prompt
                            .clone()
                            .unwrap_or_else(|| ctx.system_prompt.clone()),
                        Some(ctx.session_id.clone()),
                        Some(fork_point),
                        new_session.tags.clone(),
                    )
                    .await;

                    match branch_metadata {
                        Ok(startup_timestamp) => {
                            if let Err(error) = append_active_tools_startup_metadata_event(
                                ctx.session_db.as_ref(),
                                &new_session.id,
                                active_tool_names(&ctx.registry),
                                startup_timestamp,
                            )
                            .await
                            {
                                ctx.session_db
                                    .end_session(&new_session.id, EndReason::Error)
                                    .await
                                    .ok();
                                eprintln!("Error: {}", error);
                                return SlashResult::Handled;
                            }
                        }
                        Err(error) => {
                            ctx.session_db
                                .end_session(&new_session.id, EndReason::Error)
                                .await
                                .ok();
                            eprintln!("Error: {}", error);
                            return SlashResult::Handled;
                        }
                    }

                    if let Err(error) =
                        append_branch_summary(ctx, &new_session.id, 0, fork_point, "branch").await
                    {
                        eprintln!("Warning: failed to record branch summary: {}", error);
                    }

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

        "browser" => {
            match args.trim() {
                "close" => {
                    if let Some(mgr) = ctx.browser.as_ref() {
                        mgr.close().await;
                        println!("Browser closed; the next browser action will relaunch it.");
                    } else {
                        println!("Browser is not enabled in config.");
                    }
                }
                other => {
                    println!("Usage: /browser close");
                    if !other.is_empty() {
                        println!("Unknown subcommand: {other}");
                    }
                }
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
                let selected = ctx
                    .config
                    .llm
                    .providers
                    .iter()
                    .find(|provider| provider.name == args || provider.model == args)
                    .map(|provider| ResolvedModel {
                        model: provider.model.clone(),
                        provider: Some(provider.name.clone()),
                    })
                    .unwrap_or_else(|| ResolvedModel {
                        model: args.to_string(),
                        provider: None,
                    });

                if let Err(error) = ctx.session_db.set_model(&ctx.session_id, &selected.model).await
                {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
                let event = Event::SessionModelSet {
                    model: selected.model.clone(),
                    provider: selected.provider.clone(),
                    source: "slash_command".into(),
                    timestamp: Utc::now(),
                };
                if let Err(error) = ctx.session_db.append_event(&ctx.session_id, &event).await {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
                ctx.mind_palace.ingest(event);

                if let Some(provider) = selected.provider.clone() {
                    ctx.config.llm.roles.attack_agent = provider.clone();
                    println!("Model switched to: {} ({})", selected.model, provider);
                } else {
                    let role_provider = ctx.config.llm.roles.attack_agent.clone();
                    if let Some(provider) = ctx
                        .config
                        .llm
                        .providers
                        .iter_mut()
                        .find(|provider| provider.name == role_provider)
                    {
                        provider.model = selected.model.clone();
                    }
                    println!("Model switched to: {}", selected.model);
                }
                ctx.llm = Arc::new(LlmClient::new(&ctx.config));
                rebuild_selector(ctx);
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
                if let Err(error) = ctx.session_db.set_mode(&ctx.session_id, new_mode.clone()).await
                {
                    eprintln!("Error: {}", error);
                    return SlashResult::Handled;
                }
                let event = Event::SessionModeSet {
                    mode: new_mode.clone(),
                    source: Some("slash_command".into()),
                    timestamp: Some(Utc::now()),
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
                    "Use /permissions or /guards for runtime safety settings. For other keys, edit {} directly.",
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

        "permissions" | "permission" | "perm" => {
            if args.is_empty() || matches!(args, "status" | "show") {
                print_permissions(ctx);
                return SlashResult::Handled;
            }

            let mut parts = args.split_whitespace();
            match parts.next().unwrap_or_default() {
                "mode" => {
                    let Some(mode_raw) = parts.next() else {
                        println!("Usage: /permissions mode <default|plan|read-only|accept-edits|dont-ask|bypass>");
                        return SlashResult::Handled;
                    };
                    match mode_raw.parse::<PermissionMode>() {
                        Ok(mode) => {
                            ctx.config.permissions.mode = mode;
                            match save_config(ctx) {
                                Ok(()) => println!(
                                    "Permission mode set to {}.",
                                    ctx.config.permissions.mode
                                ),
                                Err(error) => eprintln!("Config save failed: {}", error),
                            }
                        }
                        Err(error) => println!("{}", error),
                    }
                }
                "allow" => {
                    let Some(pattern) = parts.next() else {
                        println!("Usage: /permissions allow <tool|pattern>");
                        return SlashResult::Handled;
                    };
                    if !ctx
                        .config
                        .permissions
                        .allowed_tools
                        .iter()
                        .any(|p| p == pattern)
                    {
                        ctx.config
                            .permissions
                            .allowed_tools
                            .push(pattern.to_string());
                    }
                    match save_config(ctx) {
                        Ok(()) => println!("Allowed tool pattern: {pattern}"),
                        Err(error) => eprintln!("Config save failed: {}", error),
                    }
                }
                "deny" | "disallow" => {
                    let Some(pattern) = parts.next() else {
                        println!("Usage: /permissions deny <tool|pattern>");
                        return SlashResult::Handled;
                    };
                    if !ctx
                        .config
                        .permissions
                        .disallowed_tools
                        .iter()
                        .any(|p| p == pattern)
                    {
                        ctx.config
                            .permissions
                            .disallowed_tools
                            .push(pattern.to_string());
                    }
                    match save_config(ctx) {
                        Ok(()) => println!("Denied tool pattern: {pattern}"),
                        Err(error) => eprintln!("Config save failed: {}", error),
                    }
                }
                "remove" | "rm" => {
                    let list = parts.next().unwrap_or_default();
                    let Some(pattern) = parts.next() else {
                        println!("Usage: /permissions remove <allow|deny> <tool|pattern>");
                        return SlashResult::Handled;
                    };
                    match list {
                        "allow" | "allowed" => {
                            ctx.config
                                .permissions
                                .allowed_tools
                                .retain(|p| p != pattern);
                        }
                        "deny" | "denied" | "disallow" => {
                            ctx.config
                                .permissions
                                .disallowed_tools
                                .retain(|p| p != pattern);
                        }
                        _ => {
                            println!("Expected allow or deny, got: {list}");
                            return SlashResult::Handled;
                        }
                    }
                    match save_config(ctx) {
                        Ok(()) => println!("Removed {pattern} from {list}."),
                        Err(error) => eprintln!("Config save failed: {}", error),
                    }
                }
                "auto-read-only" | "readonly-auto" => {
                    let Some(value_raw) = parts.next() else {
                        println!("Usage: /permissions auto-read-only <on|off>");
                        return SlashResult::Handled;
                    };
                    let Some(value) = parse_bool_flag(value_raw) else {
                        println!("Expected on/off, got: {value_raw}");
                        return SlashResult::Handled;
                    };
                    ctx.config.permissions.auto_approve_read_only = value;
                    match save_config(ctx) {
                        Ok(()) => println!(
                            "Auto-approve read-only tools: {}.",
                            if value { "on" } else { "off" }
                        ),
                        Err(error) => eprintln!("Config save failed: {}", error),
                    }
                }
                "reset" => {
                    ctx.config.permissions.mode = PermissionMode::Default;
                    ctx.config.permissions.allowed_tools.clear();
                    ctx.config.permissions.disallowed_tools.clear();
                    ctx.config.permissions.auto_approve_read_only = true;
                    match save_config(ctx) {
                        Ok(()) => println!("Permissions reset to default."),
                        Err(error) => eprintln!("Config save failed: {}", error),
                    }
                }
                "help" => {
                    println!("Usage:");
                    println!("  /permissions");
                    println!(
                        "  /permissions mode <default|plan|read-only|accept-edits|dont-ask|bypass>"
                    );
                    println!("  /permissions allow <tool|prefix*|*suffix>");
                    println!("  /permissions deny <tool|prefix*|*suffix>");
                    println!("  /permissions remove <allow|deny> <pattern>");
                    println!("  /permissions auto-read-only <on|off>");
                    println!("  /permissions reset");
                }
                other => {
                    println!("Unknown /permissions action: {other}");
                    println!("Use /permissions help for options.");
                }
            }
            SlashResult::Handled
        }

        "guards" | "guard" => {
            if args.is_empty() || matches!(args, "status" | "show") {
                print_guards(&ctx.config.guards);
                return SlashResult::Handled;
            }

            let mut parts = args.split_whitespace();
            match parts.next().unwrap_or_default() {
                "enable" | "on" => {
                    let Some(name) = parts.next() else {
                        println!("Usage: /guards enable <guard-name>");
                        return SlashResult::Handled;
                    };
                    match set_guard_flag(&mut ctx.config.guards, name, true) {
                        Some(label) => {
                            refresh_guard_chain(ctx);
                            match save_config(ctx) {
                                Ok(()) => println!("Guard enabled: {label}"),
                                Err(error) => eprintln!("Config save failed: {}", error),
                            }
                        }
                        None => println!("Unknown guard: {name}"),
                    }
                }
                "disable" | "off" => {
                    let Some(name) = parts.next() else {
                        println!("Usage: /guards disable <guard-name>");
                        return SlashResult::Handled;
                    };
                    match set_guard_flag(&mut ctx.config.guards, name, false) {
                        Some(label) => {
                            refresh_guard_chain(ctx);
                            match save_config(ctx) {
                                Ok(()) => println!("Guard disabled: {label}"),
                                Err(error) => eprintln!("Config save failed: {}", error),
                            }
                        }
                        None => println!("Unknown guard: {name}"),
                    }
                }
                "all" => {
                    let Some(value_raw) = parts.next() else {
                        println!("Usage: /guards all <on|off>");
                        return SlashResult::Handled;
                    };
                    let Some(value) = parse_bool_flag(value_raw) else {
                        println!("Expected on/off, got: {value_raw}");
                        return SlashResult::Handled;
                    };
                    set_all_guard_flags(&mut ctx.config.guards, value);
                    refresh_guard_chain(ctx);
                    match save_config(ctx) {
                        Ok(()) => {
                            println!("All guards set to {}.", if value { "on" } else { "off" })
                        }
                        Err(error) => eprintln!("Config save failed: {}", error),
                    }
                }
                "window" | "repetition-window" => {
                    let Some(value_raw) = parts.next() else {
                        println!("Usage: /guards window <count>");
                        return SlashResult::Handled;
                    };
                    let Ok(value) = value_raw.parse::<usize>() else {
                        println!("Invalid window size: {value_raw}");
                        return SlashResult::Handled;
                    };
                    ctx.config.guards.repetition_window = value.max(1);
                    refresh_guard_chain(ctx);
                    match save_config(ctx) {
                        Ok(()) => println!(
                            "Repetition guard window set to {}.",
                            ctx.config.guards.repetition_window
                        ),
                        Err(error) => eprintln!("Config save failed: {}", error),
                    }
                }
                "help" => {
                    println!("Usage:");
                    println!("  /guards");
                    println!("  /guards enable <immutable-field|dangerous-command|repetition|attack-surface|evidence-extractor|skeptic-gate|failure-tracker|soft404|read-state-seeding>");
                    println!("  /guards disable <guard-name>");
                    println!("  /guards all <on|off>");
                    println!("  /guards window <count>");
                }
                other => {
                    println!("Unknown /guards action: {other}");
                    println!("Use /guards help for options.");
                }
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
                let registry = Arc::new(
                    build_tool_registry(
                        &ctx.config,
                        Some(ctx.session_db.clone()),
                        Some(ctx.memory_store.clone()),
                        Some(ctx.llm.clone()),
                        Some(ctx.session_id.clone()),
                    None,
                    )
                    .await,
                );
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
                if let Err(error) = append_active_tools_event_for_registry(
                    ctx.session_db.as_ref(),
                    &ctx.session_id,
                    &ctx.registry,
                    "mcp_reload",
                )
                .await
                {
                    eprintln!("Warning: failed to record active tools: {}", error);
                }
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

pub(crate) fn format_relative_time(dt: chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(dt);
    if duration.num_days() == 0 {
        let today = now.date_naive();
        let dt_day = dt.date_naive();
        if today == dt_day {
            "today".to_string()
        } else if today.pred_opt() == Some(dt_day) {
            "yesterday".to_string()
        } else {
            dt.format("%Y-%m-%d").to_string()
        }
    } else if duration.num_days() == 1 {
        "yesterday".to_string()
    } else {
        dt.format("%Y-%m-%d").to_string()
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
    if sessions.is_empty() {
        println!("No sessions found.");
    } else {
        println!("Recent sessions:\n");
        println!(
            "{:<4} {:<27} {:<51} {:<12} {}",
            "#", "Title", "Preview", "Last Active", "ID"
        );
        println!("{}", "-".repeat(101));
        for (i, s) in sessions.iter().enumerate() {
            let num = i + 1;
            let title = s.title.as_deref().unwrap_or("-");
            let preview = s.preview.as_deref().unwrap_or("");

            let truncated_title = if title.chars().count() > 25 {
                format!("{}...", title.chars().take(22).collect::<String>())
            } else {
                title.to_string()
            };
            let truncated_preview = if preview.chars().count() > 48 {
                format!("{}...", preview.chars().take(45).collect::<String>())
            } else {
                preview.to_string()
            };

            let relative_time = if let Some(dt) = s.last_active {
                format_relative_time(dt)
            } else {
                format_relative_time(s.started_at)
            };

            println!(
                "{:<4} {:<27} {:<51} {:<12} {}",
                num, truncated_title, truncated_preview, relative_time, s.id
            );
        }
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
