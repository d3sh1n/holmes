use anyhow::Context;
use chrono::Utc;
use holmes_core::background::BackgroundTasks;
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
use holmes_runtime::permissions::ApprovalHandler;
use holmes_runtime::runtime::{AgentRuntime, TurnOutcome};
use holmes_runtime::{
    RuntimeContext, RuntimeSink, RuntimeState, RuntimeYield, SteeringQueue, StreamEvent,
};
use holmes_session::memory_store::MemoryStore;
use holmes_session::selector::Selector;
use holmes_session::{SessionDB, SessionStore};
use holmes_tools::ToolRegistry;
use reedline::{
    default_emacs_keybindings, Completer, Emacs, FileBackedHistory, IdeMenu, KeyCode, KeyModifiers,
    MenuBuilder, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, Suggestion,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::commands::CommandRegistry;
use crate::project_knowledge::build_system_prompt;
use crate::workflows;

const SYSTEM_PROMPT: &str = r#"你是 Holmes，一个渗透测试、安全研究和逆向工程的 AI Agent。

## 核心原则
- 你与用户（Watson）协作进行安全研究。**Watson 主导；Watson 的显式命令优先级最高——照做，不要自行拒绝、打折扣或质疑一条指令。** 你在 Watson 的授权下行动。
- 诚实透明：不确定的事情明确说。不伪造结果。
- 授权由 Watson 负责。操作者可配置 scope 白名单作为目标范围的启发式提示（scope guard 会拦截能从参数中识别出宿主的越界请求），但它**不是不可绕过的系统硬边界**——重定向跳转、动态拼接的命令、未声明目标的工具都可能绕过检查。因此你要自行把行动严格限制在 Watson 授权的范围内；拿不准是否越界时，先向 Watson 确认，不要自行试探。
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
    let raw: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;
    let mut cfg: HolmesConfig = serde_yaml::from_value(raw.clone())
        .with_context(|| format!("failed to parse config at {}", path.display()))?;
    // Startup diagnostics (P2-01): unknown / removed keys and invalid value
    // combinations are surfaced as warnings; loading itself never fails on them.
    for diagnostic in holmes_core::config::diagnose_config(&raw, &cfg) {
        eprintln!(
            "config warning [{}]: {}",
            diagnostic.path, diagnostic.message
        );
    }
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

/// Resolve the long-term memory database path from `memory.db_path`:
/// absolute paths are used as-is, relative paths resolve against the Holmes
/// data directory.
pub(crate) fn resolve_memory_path(data_dir: &Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    if path.is_absolute() {
        path
    } else {
        data_dir.join(path)
    }
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

// Registry assembly takes every piece of session-scoped plumbing explicitly;
// the `SessionAssembler` (session_assembly.rs, P1-04) is the bundling struct,
// so exempt this one entry point.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_tool_registry(
    config: &Config,
    session_db: Option<Arc<dyn SessionStore>>,
    memory_store: Option<Arc<MemoryStore>>,
    llm: Option<Arc<LlmClient>>,
    session_id: Option<String>,
    browser: Option<Arc<holmes_browser::BrowserManager>>,
    background_tasks: &BackgroundTasks,
    subagent_slots: &Arc<tokio::sync::Semaphore>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    // Durable background tasks (AGT-007): bind the store's task sink to this
    // session so spawned subagent tasks survive a process restart.
    let durable_binding = session_db
        .as_ref()
        .and_then(|db| db.durable_task_sink())
        .map(|sink| holmes_core::background::DurableTaskBinding {
            sink,
            parent_session_id: session_id.clone(),
        });

    let runner = if let (Some(db), Some(ms), Some(l), Some(sid)) =
        (session_db, memory_store, llm, session_id)
    {
        Some(std::sync::Arc::new(crate::subagent::CliSubagentRunner {
            session_db: db,
            memory_store: ms,
            llm: l,
            config: config.clone(),
            parent_session_id: sid,
            slots: subagent_slots.clone(),
        }) as Arc<dyn holmes_core::subagent::SubagentRunner>)
    } else {
        None
    };

    holmes_tools::builtin::register_all(
        &mut registry,
        config,
        runner,
        browser,
        Some(background_tasks.clone()),
        durable_binding,
        Some(subagent_slots.clone()),
    );
    holmes_tools::mcp::register_mcp_tools(
        &mut registry,
        &config.mcp.servers,
        std::time::Duration::from_millis(config.execution.mcp_request_timeout_ms),
    )
    .await;
    registry
}

fn replay_events_into_runtime(
    session: &mut RuntimeSession,
    mind_palace: &mut MindPalace,
    events: &[StoredEvent],
) {
    use holmes_core::tool_types::{FunctionCall, Role, ToolCall};

    // Call ↔ outcome correlation (P1-03): native call ids when the events carry
    // them, plus a legacy name-matched FIFO for pre-call-id events.
    let mut pending_by_id = std::collections::HashMap::<String, String>::new(); // call_id -> tool name
    let mut legacy_pending = VecDeque::<(String, String)>::new(); // (tool name, call_id)

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
                name,
                arguments,
                call_id,
                ..
            } => {
                let id = call_id
                    .clone()
                    .unwrap_or_else(|| format!("replayed-{}", uuid::Uuid::new_v4()));
                let tool_call = ToolCall {
                    id: id.clone(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: name.clone(),
                        arguments: arguments.to_string(),
                    },
                };
                pending_by_id.insert(id.clone(), name.clone());
                if call_id.is_none() {
                    legacy_pending.push_back((name.clone(), id));
                }

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
            Event::ToolResult {
                name,
                content,
                call_id,
                ..
            } => {
                let call_id = resolve_pending_call(
                    &mut pending_by_id,
                    &mut legacy_pending,
                    call_id.as_deref(),
                    &name,
                )
                .unwrap_or_else(|| format!("replayed-orphan-{}", session.messages.len()));
                session
                    .messages
                    .push(Message::tool_result(call_id, name, content));
            }
            Event::ToolBlocked {
                tool_name,
                guard_name,
                reason,
                call_id,
            } => {
                // A blocked call never executed, but the assistant message carries
                // its tool_use — synthesize the failure tool-result so the
                // replayed history stays legal (P1-03).
                let call_id = resolve_pending_call(
                    &mut pending_by_id,
                    &mut legacy_pending,
                    call_id.as_deref(),
                    &tool_name,
                )
                .unwrap_or_else(|| format!("replayed-orphan-{}", session.messages.len()));
                session.messages.push(Message::tool_result(
                    call_id,
                    tool_name,
                    format!("[Tool blocked by {guard_name}] {reason}"),
                ));
            }
            Event::SessionModeSet { mode, .. } => {
                session.mode = mode;
            }
            _ => {}
        }
    }

    // Close dangling tool calls (crash between ToolCall and its outcome event, or
    // an outcome archived away): every tool_use needs a tool_result.
    let answered: std::collections::HashSet<&str> = session
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect();
    let mut inserts: Vec<(usize, Vec<Message>)> = Vec::new();
    for (index, message) in session.messages.iter().enumerate() {
        if message.role != Role::Assistant {
            continue;
        }
        let Some(tool_calls) = message.tool_calls.as_deref() else {
            continue;
        };
        let missing: Vec<Message> = tool_calls
            .iter()
            .filter(|call| !answered.contains(call.id.as_str()))
            .map(|call| {
                Message::tool_result(
                    call.id.clone(),
                    call.function.name.clone(),
                    "[Tool result missing from the session event log — the session was interrupted before a result was recorded.]",
                )
            })
            .collect();
        if !missing.is_empty() {
            inserts.push((index + 1, missing));
        }
    }
    for (position, synthesized) in inserts.into_iter().rev() {
        let position = position.min(session.messages.len());
        session.messages.splice(position..position, synthesized);
    }
}

/// Resolve which pending tool call an outcome event answers (P1-03): by native
/// call id when present, otherwise the oldest still-pending legacy call with the
/// same tool name.
fn resolve_pending_call(
    pending_by_id: &mut std::collections::HashMap<String, String>,
    legacy_pending: &mut VecDeque<(String, String)>,
    call_id: Option<&str>,
    name: &str,
) -> Option<String> {
    if let Some(call_id) = call_id {
        return pending_by_id.remove(call_id).map(|_| call_id.to_string());
    }
    let position = legacy_pending.iter().position(|(tool, _)| tool == name)?;
    let (_, id) = legacy_pending.remove(position)?;
    pending_by_id.remove(&id);
    Some(id)
}

/// Rebuild a runtime context from the session's semantic event stream, falling
/// back to legacy message replay when the session predates semantic startup
/// metadata. The returned bool is `true` when semantic replay succeeded.
pub(crate) async fn load_session_runtime_from_store(
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
    /// Shared cooperative-cancellation flag for the in-flight turn. The TUI sets it
    /// (Esc/Ctrl+C in the busy-loop key dispatch); the runtime observes it at each
    /// iteration boundary.
    pub cancel: Arc<AtomicBool>,
    /// Steering queue for the in-flight turn (grok-build interjection): complete lines
    /// typed while Holmes is busy are pushed here and drained into the conversation at
    /// the next iteration boundary. Lines still queued when the turn ends were never
    /// seen by the agent and are transferred back into `queued_turns`
    /// (`run_runtime_input_with_sink`).
    pub steering: SteeringQueue,
    /// Background subagent task registry shared with the tool registry (spawn /
    /// get_task_output tools hold clones) and wired into each turn's runtime context,
    /// so background completions are drained into the conversation at iteration
    /// boundaries across turns. Shares the `cancel` flag so blocking task waits
    /// break on Esc.
    pub background_tasks: BackgroundTasks,
    /// Process-wide subagent concurrency pool (AGT-014): one semaphore shared by
    /// every tool registry built for this context and by every nested subagent
    /// registry, so `subagent.max_concurrent` holds across nesting levels and
    /// registry rebuilds.
    pub subagent_slots: Arc<tokio::sync::Semaphore>,
    /// Resident durable task scheduler (P1-02): shutdown token + scan-loop
    /// handle. Dropping the context cancels the loop and every in-flight
    /// scheduler worker.
    pub scheduler_shutdown: Option<tokio_util::sync::CancellationToken>,
    pub scheduler_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ChatContext {
    fn drop(&mut self) {
        if let Some(token) = &self.scheduler_shutdown {
            token.cancel();
        }
        if let Some(handle) = &self.scheduler_handle {
            handle.abort();
        }
    }
}

pub(crate) fn save_config(ctx: &ChatContext) -> anyhow::Result<()> {
    let path = ctx.data_dir.join("config.yaml");
    let yaml = serde_yaml::to_string(&ctx.config)?;
    std::fs::write(&path, yaml)
        .with_context(|| format!("failed to write config at {}", path.display()))?;
    Ok(())
}

pub(crate) fn rebuild_selector(ctx: &mut ChatContext) {
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
    name.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

struct CliRuntimeSink;

impl RuntimeSink for CliRuntimeSink {
    fn emit(&mut self, event: StreamEvent) {
        match event.data {
            // Streaming deltas are surfaced by the inline UI; the line REPL / one-shot sink
            // prints the whole block below, so ignore the incremental fragments here.
            RuntimeYield::TextDelta { .. } => {}
            RuntimeYield::MessageToUser { content } | RuntimeYield::PlanUpdate { content } => {
                print_holmes(&content);
            }
            RuntimeYield::ToolStarted { name, call_id, .. } => {
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
            RuntimeYield::SteeringInjected { content } => {
                println!("  steering: {}", content);
            }
            RuntimeYield::BackgroundTaskFinished {
                description,
                success,
                ..
            } => {
                println!(
                    "  ⚑ background task \"{}\" {}",
                    description,
                    if success { "completed" } else { "failed" }
                );
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
    approver: Option<Arc<dyn ApprovalHandler>>,
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
    // Start this turn uncancelled and share the flag with the runtime so an external
    // interrupt (e.g. the TUI's Esc/Ctrl+C dispatch) can stop the loop at the next boundary.
    ctx.cancel.store(false, Ordering::Relaxed);
    let mut runtime = AgentRuntime::new(runtime_context);
    // Interactive approval gate (Ask mode): the inline UI installs its approver here;
    // REPL / one-shot callers pass None, so `Ask` mutating calls are denied
    // (fail-closed) there.
    if let Some(approver) = approver {
        runtime.set_approver(approver);
    }
    runtime.context_mut().set_cancel_flag(ctx.cancel.clone());
    // Share the steering queue so lines typed mid-turn are injected at the next
    // iteration boundary instead of waiting for a follow-up turn.
    runtime
        .context_mut()
        .set_steering_queue(ctx.steering.clone());
    // Share the background task registry so subagents spawned in background mode by
    // this session's tools have their completions drained into this turn (and any
    // task that finished between turns is injected at this turn's first boundary).
    runtime
        .context_mut()
        .set_background_tasks(ctx.background_tasks.clone());
    if ctx.browser.is_some() {
        runtime.context_mut().middlewares.push(Arc::new(
            holmes_runtime::middleware::BrowserReadOnlyMiddleware,
        ));
    }
    // Always-on safety middleware: secret redaction on tool output + a static
    // dangerous-command backstop. (These existed but were never installed in production.)
    runtime.context_mut().middlewares.push(Arc::new(
        holmes_runtime::middleware::SensitiveDataRedactMiddleware::new(),
    ));
    runtime.context_mut().middlewares.push(Arc::new(
        holmes_runtime::middleware::UntrustedContentMiddleware,
    ));
    runtime.context_mut().middlewares.push(Arc::new(
        holmes_runtime::bounty::BountyWorkflowMiddleware,
    ));
    runtime
        .context_mut()
        .middlewares
        .push(Arc::new(holmes_runtime::middleware::GuardMiddleware));
    // Outbound attack-rate limit (opt-in via config.safety.egress_rpm).
    if let Some(rpm) = ctx.config.safety.egress_rpm {
        if rpm > 0 {
            runtime.context_mut().middlewares.push(Arc::new(
                holmes_runtime::middleware::RateLimitMiddleware::new(rpm),
            ));
        }
    }
    // User-configurable tool hooks (opt-in via config.hooks) — deterministic policy / audit.
    if holmes_runtime::middleware::UserHookMiddleware::is_active(&ctx.config.hooks) {
        runtime.context_mut().middlewares.push(Arc::new(
            holmes_runtime::middleware::UserHookMiddleware::new(ctx.config.hooks.clone()),
        ));
    }
    let result = if oneshot {
        runtime.run_oneshot(input, sink).await
    } else {
        runtime.run_turn(input, sink).await
    };
    let runtime_context = runtime.into_context();

    // Steering leftovers: lines the agent never drained (pushed after its final
    // iteration-boundary drain, e.g. while the last LLM call was in flight) keep the
    // old type-ahead behavior — they become follow-up turns.
    transfer_steering_leftovers(&ctx.steering, &mut ctx.queued_turns);

    ctx.session_id = runtime_context.session_id.clone();
    ctx.runtime_session = runtime_context.session;
    ctx.mind_palace = runtime_context.mind_palace;
    ctx.runtime_guards = runtime_context.guards;
    ctx.runtime_state = runtime_context.state;

    // Auto-generate a structured findings report when the engagement finishes (one-shot)
    // and config.agent.generate_reports is on — previously a dead flag.
    if oneshot && ctx.config.agent.generate_reports {
        if let Ok(events) = ctx.session_db.get_events(&ctx.session_id).await {
            let report = render_case_report(
                &ctx.session_id,
                &ctx.runtime_session.mode,
                ctx.runtime_state.active_goal.as_deref(),
                &events,
            );
            let dir = std::path::Path::new(&ctx.config.output_dir);
            if std::fs::create_dir_all(dir).is_ok() {
                let path = dir.join(format!("report-{}.md", ctx.session_id));
                if std::fs::write(&path, &report).is_ok() {
                    eprintln!("[holmes] report written to {}", path.display());
                }
            }
        }
    }

    result.map_err(Into::into)
}

pub(crate) async fn run_runtime_input(
    ctx: &mut ChatContext,
    input: String,
    oneshot: bool,
) -> anyhow::Result<TurnOutcome> {
    let mut sink = CliRuntimeSink;
    run_runtime_input_with_sink(ctx, input, oneshot, &mut sink, None).await
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

/// Move steering lines the in-flight turn never drained (pushed after its last
/// iteration-boundary drain) into the follow-up turn queue, preserving the old
/// type-ahead "run after this turn" behavior for them.
pub(crate) fn transfer_steering_leftovers(
    steering: &SteeringQueue,
    queued_turns: &mut VecDeque<String>,
) {
    let mut queue = steering.lock().unwrap_or_else(|e| e.into_inner());
    while let Some(line) = queue.pop_front() {
        queued_turns.push_back(line);
    }
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
    ctx.steering
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
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
    // Structured findings — the actual deliverable. Keyed by id so a later re-report
    // (e.g. a confirmation) supersedes the earlier record.
    let mut findings: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut ruled_out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for event in events {
        match &event.event {
            Event::FindingRecorded {
                id,
                finding_type,
                confidence,
                severity,
                evidence: ev,
                details,
                attack_type,
                location,
                evidence_source,
            } => {
                if confidence == "rejected" {
                    let ty = if finding_type.is_empty() {
                        attack_type
                    } else {
                        finding_type
                    };
                    let loc = if location.is_empty() {
                        String::new()
                    } else {
                        format!(" @ {location}")
                    };
                    ruled_out.insert(format!("{ty}{loc}"));
                    findings.remove(id);
                    continue;
                }
                let ty = if finding_type.is_empty() {
                    attack_type
                } else {
                    finding_type
                };
                let mut entry = format!(
                    "### [{:?}] {id}\n- **Type:** {ty}\n- **Confidence:** {confidence}\n",
                    severity
                );
                if !location.is_empty() {
                    entry.push_str(&format!("- **Location:** {location}\n"));
                }
                if !ev.is_empty() {
                    entry.push_str(&format!("- **Evidence:** {ev}\n"));
                }
                if let Some(src) = evidence_source {
                    entry.push_str(&format!("- **Evidence source:** {src}\n"));
                }
                if !details.is_empty() {
                    entry.push_str(&format!("- **Details:** {details}\n"));
                }
                findings.insert(id.clone(), entry);
            }
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

    // Findings first — the deliverable.
    out.push_str(&format!("## Findings ({})\n\n", findings.len()));
    if findings.is_empty() {
        out.push_str("_No findings recorded._\n\n");
    } else {
        for entry in findings.values() {
            out.push_str(entry);
            out.push('\n');
        }
    }
    if !ruled_out.is_empty() {
        out.push_str("## Ruled out (tested, negative)\n\n");
        for r in &ruled_out {
            out.push_str(&format!("- {r}\n"));
        }
        out.push('\n');
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
        out.push('\n');
    }
    out.push('\n');
}

pub(crate) fn active_tool_names(registry: &ToolRegistry) -> Vec<String> {
    let mut names = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.function.name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
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

/// Construct the browser manager for a session (lazy-launch; only when enabled).
/// Shared by fresh and resumed/continued sessions so browser availability does not
/// depend on how the session was started.
pub(crate) fn build_browser(
    config: &HolmesConfig,
    data_dir: &Path,
    session_id: &str,
) -> Option<Arc<holmes_browser::BrowserManager>> {
    if !config.browser.enabled {
        return None;
    }
    let sessions_dir = data_dir.join("sessions");
    match holmes_browser::BrowserManager::new(session_id, &sessions_dir, config.browser.clone()) {
        Ok(mgr) => Some(Arc::new(mgr)),
        Err(e) => {
            eprintln!("Warning: browser disabled: {e}");
            None
        }
    }
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
    let session_db = SessionDB::open(&db_path).await?;

    // Restart recovery (AGT-007): discharge tasks whose runner died with the
    // previous process before any new work starts. Safe-to-retry orphans are
    // requeued; side-effecting ones are suspended for manual recovery.
    let task_store = session_db.task_store();
    match holmes_runtime::recovery::recover_durable_tasks(&task_store).await {
        Ok(report) if !report.is_clean() => {
            eprintln!(
                "⚠ Recovered {} orphaned background task(s): {} requeued, {} suspended for manual recovery",
                report.requeued.len() + report.manual_required.len(),
                report.requeued.len(),
                report.manual_required.len(),
            );
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("⚠ Background task recovery failed: {error}");
        }
    }

    let session_db: Arc<dyn SessionStore> = Arc::new(session_db);

    let memory_path = resolve_memory_path(&data_dir, &config.memory.db_path);
    if let Some(parent) = memory_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let memory_store = Arc::new(MemoryStore::open(&memory_path).await?);

    let guards = Arc::new(Mutex::new(GuardChain::from_config(&config.guards)));
    let runtime_guards = GuardChain::from_config(&config.guards);
    let llm = Arc::new(LlmClient::new(&config));
    let startup_model = resolve_attack_model_provider(&config, model);

    // Cancel flag + background task registry are created as a pair: every tool
    // registry built for this ChatContext and every per-turn runtime context share
    // them, so background subagent completions drain across turns and blocking
    // `get_task_output` waits observe Esc.
    let cancel = Arc::new(AtomicBool::new(false));
    let background_tasks = BackgroundTasks::with_cancel(cancel.clone());
    // One process-wide subagent concurrency pool (AGT-014), shared by every
    // registry built below and every nested subagent run.
    let subagent_slots = Arc::new(tokio::sync::Semaphore::new(
        (config.subagent.max_concurrent as usize).max(1),
    ));

    // Resident durable task scheduler (P1-02): startup recovery above runs once;
    // this keeps scanning — dead-owner reaper, expired-lease recovery, and
    // atomic lease-and-execute for requeued tasks (subagent tasks are re-run
    // from their persisted payload after an operator requeue).
    let scheduler_shutdown = tokio_util::sync::CancellationToken::new();
    let scheduler = Arc::new(
        holmes_runtime::scheduler::DurableTaskScheduler::new(task_store)
            .with_executor(
                "subagent",
                Arc::new(crate::subagent::SubagentTaskExecutor {
                    session_db: session_db.clone(),
                    memory_store: memory_store.clone(),
                    llm: llm.clone(),
                    config: config.clone(),
                    slots: subagent_slots.clone(),
                }),
            )
            .with_heartbeat_interval(std::time::Duration::from_millis(
                config.experiments.heartbeat_ms.max(1),
            ))
            .with_max_attempts(config.experiments.max_attempts.max(1))
            .with_shutdown(scheduler_shutdown.clone()),
    );
    let scheduler_handle = scheduler.spawn(holmes_runtime::scheduler::DEFAULT_SCAN_INTERVAL);

    // Session assembly (P1-04): fresh startup, --resume and --continue all
    // build the session through the one assembler — identical startup
    // semantics, canonical replay and resource rebuild on every path.
    let assembler = crate::session_assembly::SessionAssembler::new(
        session_db.clone(),
        memory_store.clone(),
        llm.clone(),
        config.clone(),
        data_dir.clone(),
        system_prompt.clone(),
        background_tasks.clone(),
        subagent_slots.clone(),
    );

    let (assembled, is_resume) = if let Some(id) = resume_id {
        let assembled = assembler.assemble_resume(&id, mode.clone()).await?;
        if announce {
            if !assembled.semantic_complete {
                eprintln!(
                    "⚠ Session {} is missing semantic startup metadata; used legacy replay fallback",
                    &id[..8.min(id.len())]
                );
            }
            eprintln!("↻ Resumed session {}", &id[..8.min(id.len())]);
            print_pending_task_results_notice(assembled.pending_task_results);
        }
        (assembled, true)
    } else if continue_last {
        let filter = SessionFilter {
            limit: Some(1),
            ..Default::default()
        };
        let sessions = session_db.list_sessions(&filter).await?;
        if let Some(s) = sessions.first() {
            let assembled = assembler.assemble_resume(&s.id, mode.clone()).await?;
            if announce {
                if !assembled.semantic_complete {
                    eprintln!(
                        "⚠ Session {} is missing semantic startup metadata; used legacy replay fallback",
                        &s.id[..8.min(s.id.len())]
                    );
                }
                eprintln!("↻ Continued session {}", &s.id[..8.min(s.id.len())]);
                print_pending_task_results_notice(assembled.pending_task_results);
            }
            (assembled, true)
        } else {
            (
                assembler
                    .assemble_fresh(
                        mode.clone(),
                        startup_model.clone(),
                        system_prompt.clone(),
                        "cli",
                    )
                    .await?,
                false,
            )
        }
    } else {
        (
            assembler
                .assemble_fresh(
                    mode.clone(),
                    startup_model.clone(),
                    system_prompt.clone(),
                    "cli",
                )
                .await?,
            false,
        )
    };

    let mut selector = Selector::new();
    for wf in
        workflows::create_builtin_workflows(llm.clone(), assembled.registry.clone(), guards.clone())
    {
        selector.register(wf);
    }

    let mut runtime_state = RuntimeState::new(assembled.runtime_session.mode.clone());
    runtime_state.active_goal = assembled.active_goal.clone();
    let ctx = ChatContext {
        session_id: assembled.session_id,
        session_db: session_db.clone(),
        memory_store: memory_store.clone(),
        llm: llm.clone(),
        registry: assembled.registry,
        guards: guards.clone(),
        runtime_guards,
        selector,
        runtime_session: assembled.runtime_session,
        mind_palace: assembled.mind_palace,
        runtime_state,
        queued_turns: VecDeque::new(),
        steering_notes: Vec::new(),
        system_prompt,
        config,
        data_dir: data_dir.clone(),
        command_registry: CommandRegistry::default(),
        browser: assembled.browser,
        cancel,
        steering: holmes_runtime::new_steering_queue(),
        background_tasks,
        subagent_slots,
        scheduler_shutdown: Some(scheduler_shutdown),
        scheduler_handle: Some(scheduler_handle),
    };

    Ok(Some(ChatStartup { ctx, is_resume }))
}

/// Durable background-task results written before a crash are re-delivered by
/// the runtime at the next turn boundary (P1-02); surface the pending count
/// when a session starts or resumes with some waiting.
pub(crate) fn print_pending_task_results_notice(pending_task_results: usize) {
    if pending_task_results > 0 {
        eprintln!(
            "⧉ {pending_task_results} background task result(s) pending; delivered at the next turn boundary"
        );
    }
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
                SlashResult::NewSession(assembled) => {
                    crate::session_assembly::switch_to(&mut ctx, *assembled);
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

// NewSession carries a fully assembled session stack (P1-04); boxing keeps the
// enum small despite the larger payload.
pub(crate) enum SlashResult {
    Quit,
    Handled,
    NewSession(Box<crate::session_assembly::AssembledSession>),
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
            let assembler = crate::session_assembly::SessionAssembler::from_context(ctx);
            match assembler
                .assemble_fresh(
                    ctx.runtime_session.mode.clone(),
                    model,
                    ctx.system_prompt.clone(),
                    "cli",
                )
                .await
            {
                Ok(assembled) => {
                    println!(
                        "Started new session: {}",
                        &assembled.session_id[..8.min(assembled.session_id.len())]
                    );
                    return SlashResult::NewSession(Box::new(assembled));
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
                                "{:<4} {:<27} {:<51} {:<12} ID",
                                "#", "Title", "Preview", "Last Active"
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
                        let assembler =
                            crate::session_assembly::SessionAssembler::from_context(ctx);
                        let assembled = match assembler.assemble_resume(&s.id, s.mode.clone()).await
                        {
                            Ok(assembled) => assembled,
                            Err(error) => {
                                eprintln!("Error: {}", error);
                                return SlashResult::Handled;
                            }
                        };
                        if !assembled.semantic_complete {
                            eprintln!(
                                "⚠ Session {} is missing semantic startup metadata; used legacy replay fallback",
                                &s.id[..8.min(s.id.len())]
                            );
                        }
                        println!(
                            "↻ Resuming session {} ({}) and replaying history...",
                            &s.id[..8.min(s.id.len())],
                            s.title.as_deref().unwrap_or("untitled"),
                        );
                        print_pending_task_results_notice(assembled.pending_task_results);
                        let events = ctx
                            .session_db
                            .get_events(&s.id)
                            .await
                            .ok()
                            .unwrap_or_default();
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
                        return SlashResult::NewSession(Box::new(assembled));
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

        "ledger" => {
            match ctx.session_db.case_id_for_session(&ctx.session_id).await {
                Ok(case_id) => {
                    if args.trim() == "compact" {
                        match ctx.session_db.compact_snapshot(&case_id, 1).await {
                            Ok(result) if result.written => println!(
                                "Ledger snapshot rebuilt at event {}.",
                                result.projected_seq
                            ),
                            Ok(result) => println!(
                                "Ledger snapshot is already current at event {}.",
                                result.projected_seq
                            ),
                            Err(error) => {
                                eprintln!("Ledger snapshot rebuild failed: {error}");
                                return SlashResult::Handled;
                            }
                        }
                    }
                    match ctx.session_db.load(&case_id).await {
                        Ok(snapshot) if args.trim() == "json" => {
                            match serde_json::to_string_pretty(&snapshot) {
                                Ok(json) => println!("{json}"),
                                Err(error) => eprintln!("Ledger serialization failed: {error}"),
                            }
                        }
                        Ok(snapshot) => {
                            println!("Case Ledger: {} @ v{}", snapshot.case_id, snapshot.version);
                            println!(
                                "  Hypotheses: {}  Predictions: {}  Experiments: {}",
                                snapshot.hypotheses.len(),
                                snapshot.predictions.len(),
                                snapshot.experiments.len()
                            );
                            println!(
                                "  Evidence: {}  Links: {}  Resolutions: {}  Contradictions: {}",
                                snapshot.evidence.len(),
                                snapshot.evidence_links.len(),
                                snapshot.resolutions.len(),
                                snapshot.contradictions.len()
                            );
                            for hypothesis in snapshot.hypotheses.values() {
                                println!(
                                    "  [{} {:?}/{:?} r{}] {}",
                                    hypothesis.id,
                                    hypothesis.status,
                                    hypothesis.priority,
                                    hypothesis.revision,
                                    truncate_chars(&hypothesis.claim, 120)
                                );
                            }
                            for experiment in snapshot.experiments.values() {
                                println!(
                                    "  [{} {:?} attempt={} task={}] {}",
                                    experiment.id,
                                    experiment.status,
                                    experiment.attempt,
                                    experiment.task_id.as_deref().unwrap_or("-"),
                                    truncate_chars(&experiment.action, 100)
                                );
                            }
                        }
                        Err(error) => eprintln!("Ledger load failed: {error}"),
                    }
                }
                Err(error) => eprintln!("Case lookup failed: {error}"),
            }
            SlashResult::Handled
        }

        "bounty" => {
            let bounty = &ctx.runtime_state.compatibility_state.bounty;
            match &bounty.program {
                Some(program) => {
                    println!("Authorized program: {}", program.name);
                    if !program.policy_notes.trim().is_empty() {
                        println!("  Policy: {}", program.policy_notes.trim());
                    }
                    println!("  In scope:");
                    for entry in &program.in_scope {
                        println!("    - {:?} {}", entry.kind, entry.value);
                    }
                    if program.out_of_scope.is_empty() {
                        println!("  Out of scope: (none)");
                    } else {
                        println!("  Out of scope:");
                        for entry in &program.out_of_scope {
                            println!("    - {:?} {}", entry.kind, entry.value);
                        }
                    }
                    println!("  Assets: {}", bounty.assets.len());
                    for asset in &bounty.assets {
                        println!(
                            "    - {} ({:?}, {})",
                            asset.identifier,
                            asset.kind,
                            asset.how_found.label()
                        );
                    }
                }
                None => println!(
                    "No authorized bounty/VDP program is attached. Ask the agent to call set_program_scope."
                ),
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
                    let assembler = crate::session_assembly::SessionAssembler::from_context(ctx);
                    match assembler
                        .assemble_fork(&ctx.session_id, fork_point, &title, "branch")
                        .await
                    {
                        Ok(assembled) => {
                            println!(
                                "Branched to {} at event_index={fork_point}.",
                                short_id(&assembled.session_id)
                            );
                            return SlashResult::NewSession(Box::new(assembled));
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
            let assembler = crate::session_assembly::SessionAssembler::from_context(ctx);
            match assembler
                .assemble_fork(
                    &ctx.session_id,
                    fork_point,
                    title.as_deref().unwrap_or("branch"),
                    "branch",
                )
                .await
            {
                Ok(assembled) => {
                    println!(
                        "Branched to: {} ({})",
                        &assembled.session_id[..8.min(assembled.session_id.len())],
                        title.as_deref().unwrap_or("branch"),
                    );
                    // Same as /tree fork and the TUI: a branch switches the
                    // current context into the child session.
                    return SlashResult::NewSession(Box::new(assembled));
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

                if let Err(error) = ctx
                    .session_db
                    .set_model(&ctx.session_id, &selected.model)
                    .await
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
                if let Err(error) = ctx
                    .session_db
                    .set_mode(&ctx.session_id, new_mode.clone())
                    .await
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
                // P2-04: rebuild through the SessionAssembler's unified
                // registry builder with the session's CURRENT browser handle —
                // passing `None` here silently dropped the `browser` tool.
                let registry = crate::session_assembly::SessionAssembler::from_context(ctx)
                    .rebuild_registry(&ctx.session_id, ctx.browser.clone())
                    .await;
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
            println!("Session:   {}", holmes_core::truncate_str(&s.id, 8));
            println!("Mode:      {:?}", s.mode);
            println!("Messages:  {}", s.message_count());
            println!("Tokens:    {} in / {} out", s.tokens.input, s.tokens.output);
            let parent_short = s
                .lineage
                .parent_id
                .as_ref()
                .map(|id| holmes_core::truncate_str(id, 8).to_string());
            println!(
                "Lineage:   parent={:?}, fork_point={:?}",
                parent_short, s.lineage.fork_point,
            );
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
            "{:<4} {:<27} {:<51} {:<12} ID",
            "#", "Title", "Preview", "Last Active"
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
    fn memory_db_path_resolves_against_data_dir_unless_absolute() {
        let data_dir = Path::new("/tmp/holmes-data");
        // The default ("memory.db") keeps the historical hardcoded location.
        assert_eq!(
            resolve_memory_path(data_dir, "memory.db"),
            PathBuf::from("/tmp/holmes-data/memory.db")
        );
        assert_eq!(
            resolve_memory_path(data_dir, "custom/mem.db"),
            PathBuf::from("/tmp/holmes-data/custom/mem.db")
        );
        assert_eq!(
            resolve_memory_path(data_dir, "/var/lib/holmes/mem.db"),
            PathBuf::from("/var/lib/holmes/mem.db")
        );
    }

    #[test]
    fn steering_leftovers_transfer_into_queued_turns() {
        // Lines the in-flight turn never drained (pushed after its final
        // iteration-boundary drain) must become follow-up turns, in FIFO order.
        let steering = holmes_runtime::new_steering_queue();
        {
            let mut queue = steering.lock().expect("steering lock");
            queue.push_back("first".to_string());
            queue.push_back("second".to_string());
        }
        let mut queued_turns = VecDeque::from(vec!["already queued".to_string()]);

        transfer_steering_leftovers(&steering, &mut queued_turns);

        assert!(steering.lock().expect("steering lock").is_empty());
        assert_eq!(
            queued_turns.into_iter().collect::<Vec<_>>(),
            vec![
                "already queued".to_string(),
                "first".to_string(),
                "second".to_string()
            ]
        );
    }

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
