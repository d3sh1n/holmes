use holmes_core::event::{Event, StoredEvent};
use holmes_core::session::{RuntimeSession, SessionLineage};
use holmes_core::{FunctionCall, Message, Role, SessionMode, TokenDelta, ToolCall};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::ArchivedEventRange;

#[derive(Debug, Clone)]
pub struct ReplayedSessionContext {
    pub session: RuntimeSession,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub active_tools: Vec<String>,
    pub compactions: Vec<CompactionReplayMarker>,
    pub branch_summaries: Vec<String>,
    pub semantic_complete: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct ReplayMessage {
    message: Message,
    origin_event_index: Option<u64>,
}

impl ReplayMessage {
    fn new(message: Message, origin_event_index: impl Into<Option<u64>>) -> Self {
        Self {
            message,
            origin_event_index: origin_event_index.into(),
        }
    }
}

/// One call inside an open tool run: the message-level tool call plus the event
/// that produced it.
#[derive(Debug, Clone)]
struct PendingCall {
    tool_call: ToolCall,
    origin_event_index: u64,
}

/// A maximal run of consecutive tool-phase events (`ToolCall` / `ToolResult` /
/// `ToolBlocked`). One run corresponds to one model response's tool batch, so
/// the flush emits a single assistant message carrying every call, followed by
/// one result message per call — correlated by call id, never by adjacency or
/// tool name (P1-03).
#[derive(Debug, Default)]
struct ToolRun {
    calls: Vec<PendingCall>,
    /// Answered results, keyed by the assigned tool-call id.
    results: HashMap<String, Message>,
}

impl ToolRun {
    /// Match a result/blocked event to a pending call: by native call id when
    /// the event carries one, otherwise (legacy events) the first still
    /// unanswered call with the same tool name.
    fn match_call(&self, call_id: Option<&str>, name: &str) -> Option<String> {
        if let Some(call_id) = call_id {
            return self
                .calls
                .iter()
                .find(|pending| pending.tool_call.id == call_id)
                .map(|pending| pending.tool_call.id.clone());
        }
        self.calls
            .iter()
            .find(|pending| {
                pending.tool_call.function.name == name
                    && !self.results.contains_key(&pending.tool_call.id)
            })
            .map(|pending| pending.tool_call.id.clone())
    }
}

/// Flush an open tool run into replayed messages: one assistant message with
/// every call, then one result message per call in call order. Calls left
/// unanswered at run end (crash between `ToolCall` and its outcome event, or a
/// legacy `ToolBlocked`-only log) get a synthesized failure result so the
/// replayed history always pairs every tool_use with a tool_result.
fn flush_tool_run(
    run: &mut ToolRun,
    messages: &mut Vec<ReplayMessage>,
    warnings: &mut Vec<String>,
) {
    if run.calls.is_empty() {
        return;
    }
    let ToolRun { calls, results } = std::mem::take(run);

    let assistant_index = calls
        .last()
        .map(|pending| pending.origin_event_index)
        .unwrap_or(0);
    let tool_calls: Vec<ToolCall> = calls
        .iter()
        .map(|pending| pending.tool_call.clone())
        .collect();
    messages.push(ReplayMessage::new(
        Message::assistant_with_tool_calls(tool_calls),
        assistant_index,
    ));

    for pending in &calls {
        let id = &pending.tool_call.id;
        let name = &pending.tool_call.function.name;
        let result_message = results.get(id).cloned().unwrap_or_else(|| {
            warnings.push(format!(
                "tool_call '{}' ({}) has no result or blocked event; synthesizing failure result",
                name, id
            ));
            Message::tool_result(
                id.clone(),
                name.clone(),
                "[Tool result missing from the session event log — the session was interrupted before a result was recorded.]",
            )
        });
        messages.push(ReplayMessage::new(
            result_message,
            pending.origin_event_index,
        ));
    }
}

/// Final safety net: after compaction replacement, an assistant message can
/// survive while its result messages were archived away. Insert a synthesized
/// failure result right after any assistant message whose tool calls have no
/// matching result anywhere in the replayed history.
fn legalize_tool_history(messages: &mut Vec<ReplayMessage>, warnings: &mut Vec<String>) {
    let answered: HashSet<&str> = messages
        .iter()
        .filter(|entry| entry.message.role == Role::Tool)
        .filter_map(|entry| entry.message.tool_call_id.as_deref())
        .collect();

    // (insert position, synthesized results); applied back-to-front so earlier
    // positions stay valid.
    let mut inserts: Vec<(usize, Vec<ReplayMessage>)> = Vec::new();
    for (index, entry) in messages.iter().enumerate() {
        if entry.message.role != Role::Assistant {
            continue;
        }
        let Some(tool_calls) = entry.message.tool_calls.as_deref() else {
            continue;
        };
        let missing: Vec<ReplayMessage> = tool_calls
            .iter()
            .filter(|call| !answered.contains(call.id.as_str()))
            .map(|call| {
                warnings.push(format!(
                    "assistant tool_use '{}' ({}) lost its tool_result (e.g. archived by compaction); synthesizing failure result",
                    call.function.name, call.id
                ));
                ReplayMessage::new(
                    Message::tool_result(
                        call.id.clone(),
                        call.function.name.clone(),
                        "[Tool result unavailable in the replayed session history.]",
                    ),
                    entry.origin_event_index,
                )
            })
            .collect();
        if !missing.is_empty() {
            inserts.push((index + 1, missing));
        }
    }

    for (position, synthesized) in inserts.into_iter().rev() {
        let position = position.min(messages.len());
        messages.splice(position..position, synthesized);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionReplayMarker {
    pub event_index: u64,
    pub summary: String,
    pub archive_path: Option<String>,
    pub archived_event_range: Option<ArchivedEventRange>,
}
fn replace_archived_messages_with_summary(
    messages: &mut Vec<ReplayMessage>,
    range: ArchivedEventRange,
    summary_message: ReplayMessage,
) {
    let first_archived_position = messages.iter().position(|message| {
        message
            .origin_event_index
            .is_some_and(|event_index| event_index >= range.start && event_index <= range.end)
    });

    messages.retain(|message| {
        !message
            .origin_event_index
            .is_some_and(|event_index| event_index >= range.start && event_index <= range.end)
    });

    let insert_position = first_archived_position
        .unwrap_or(messages.len())
        .min(messages.len());
    messages.insert(insert_position, summary_message);
}

pub fn replay_events(session_id: &str, events: &[StoredEvent]) -> ReplayedSessionContext {
    let mut title = None;
    let mut mode = SessionMode::default();
    let mut model = None;
    let mut system_prompt = None;
    let mut parent_id = None;
    let mut fork_point = None;
    let mut created_at = events
        .first()
        .map(|event| event.timestamp)
        .unwrap_or_else(chrono::Utc::now);

    let mut active_tools = Vec::new();
    let mut messages: Vec<ReplayMessage> = Vec::new();
    let mut compactions = Vec::new();
    let mut branch_summaries = Vec::new();
    let mut warnings = Vec::new();
    let mut tool_run = ToolRun::default();

    let mut saw_session_created = false;
    let mut saw_system_prompt = false;
    let mut saw_mode = false;
    let mut saw_model = false;
    let mut saw_active_tools = false;

    for stored in events {
        match &stored.event {
            Event::SessionCreated {
                title: event_title,
                mode: event_mode,
                model: event_model,
                system_prompt: event_system_prompt,
                parent_id: event_parent_id,
                fork_point: event_fork_point,
                created_at: event_created_at,
                ..
            } => {
                saw_session_created = true;
                title = event_title.clone();
                mode = event_mode.clone();
                model = event_model.clone();
                system_prompt = event_system_prompt.clone();
                parent_id = event_parent_id.clone();
                fork_point = *event_fork_point;
                created_at = *event_created_at;
            }
            Event::SessionSystemPromptSet { content, .. } => {
                saw_system_prompt = true;
                system_prompt = Some(content.clone());
            }
            Event::SessionModeSet {
                mode: event_mode, ..
            } => {
                saw_mode = true;
                mode = event_mode.clone();
            }
            Event::SessionModelSet {
                model: event_model, ..
            } => {
                saw_model = true;
                model = Some(event_model.clone());
            }
            Event::ActiveToolsSet { tool_names, .. } => {
                saw_active_tools = true;
                active_tools = tool_names.clone();
            }
            Event::BranchSummary { summary, .. } => {
                flush_tool_run(&mut tool_run, &mut messages, &mut warnings);
                branch_summaries.push(summary.clone());
                messages.push(ReplayMessage::new(
                    Message::user(format!("[Branch summary]\n{summary}")),
                    stored.event_index,
                ));
            }
            Event::CompressionApplied {
                summary,
                archive_path,
                archived_event_range,
                ..
            } => {
                flush_tool_run(&mut tool_run, &mut messages, &mut warnings);
                let archived_event_range =
                    archived_event_range.map(|(start, end)| ArchivedEventRange { start, end });
                compactions.push(CompactionReplayMarker {
                    event_index: stored.event_index,
                    summary: summary.clone(),
                    archive_path: archive_path.clone(),
                    archived_event_range,
                });

                let summary_message = ReplayMessage::new(
                    Message::assistant(format!("[Compaction summary]\n{summary}")),
                    stored.event_index,
                );
                if let Some(range) = archived_event_range {
                    replace_archived_messages_with_summary(&mut messages, range, summary_message);
                } else {
                    warnings.push(format!(
                        "compression_applied event {} missing archived_event_range; appending compaction summary",
                        stored.event_index
                    ));
                    messages.push(summary_message);
                }
            }
            Event::UserMessage { content, .. } => {
                flush_tool_run(&mut tool_run, &mut messages, &mut warnings);
                messages.push(ReplayMessage::new(
                    Message::user(content.clone()),
                    stored.event_index,
                ));
            }
            Event::Thinking { content, .. } => {
                flush_tool_run(&mut tool_run, &mut messages, &mut warnings);
                messages.push(ReplayMessage::new(
                    Message::assistant(content.clone()),
                    stored.event_index,
                ));
            }
            Event::ToolCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                let tool_call = ToolCall {
                    id: call_id
                        .clone()
                        .unwrap_or_else(|| format!("replay-tool-call-{}", stored.event_index)),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: name.clone(),
                        arguments: arguments.to_string(),
                    },
                };
                tool_run.calls.push(PendingCall {
                    tool_call,
                    origin_event_index: stored.event_index,
                });
            }
            Event::ToolResult {
                name,
                content,
                call_id,
                ..
            } => {
                if let Some(id) = tool_run.match_call(call_id.as_deref(), name) {
                    tool_run.results.insert(
                        id.clone(),
                        Message::tool_result(id, name.clone(), content.clone()),
                    );
                } else {
                    flush_tool_run(&mut tool_run, &mut messages, &mut warnings);
                    warnings.push(format!(
                        "tool_result event {} for '{}' has no preceding tool_call; replaying as text context",
                        stored.event_index, name
                    ));
                    messages.push(ReplayMessage::new(
                        Message::user(format!("[Tool result: {name}]\n{content}")),
                        stored.event_index,
                    ));
                }
            }
            Event::ToolBlocked {
                tool_name,
                guard_name,
                reason,
                call_id,
            } => {
                // A blocked call never executed, but the model still saw a
                // tool_use for it — synthesize the failure tool-result so the
                // replayed history stays legal. The guard name and reason are
                // the audit metadata of this event.
                let content = format!("[Tool blocked by {guard_name}] {reason}");
                if let Some(id) = tool_run.match_call(call_id.as_deref(), tool_name) {
                    tool_run.results.insert(
                        id.clone(),
                        Message::tool_result(id, tool_name.clone(), content),
                    );
                } else {
                    flush_tool_run(&mut tool_run, &mut messages, &mut warnings);
                    warnings.push(format!(
                        "tool_blocked event {} for '{}' has no preceding tool_call; replaying as text context",
                        stored.event_index, tool_name
                    ));
                    messages.push(ReplayMessage::new(
                        Message::user(format!("[Tool blocked: {tool_name}]\n{content}")),
                        stored.event_index,
                    ));
                }
            }
            _ => {}
        }
    }

    flush_tool_run(&mut tool_run, &mut messages, &mut warnings);
    legalize_tool_history(&mut messages, &mut warnings);

    if let Some(prompt) = &system_prompt {
        if !matches!(messages.first(), Some(replay_message) if replay_message.message.role == holmes_core::Role::System && replay_message.message.content.as_deref() == Some(prompt.as_str()))
        {
            messages.insert(0, ReplayMessage::new(Message::system(prompt.clone()), None));
        }
    }

    if !saw_session_created {
        warnings.push("session_created event missing; replayed context may be incomplete".into());
    }
    if !saw_system_prompt {
        warnings.push(
            "session_system_prompt_set event missing; replayed context may be incomplete".into(),
        );
    }
    if !saw_mode {
        warnings.push("session_mode_set event missing; replayed context may be incomplete".into());
    }
    if !saw_model {
        warnings.push("session_model_set event missing; replayed context may be incomplete".into());
    }
    if !saw_active_tools {
        warnings.push("active_tools_set event missing; replayed context may be incomplete".into());
    }

    let messages = messages
        .into_iter()
        .map(|replay_message| replay_message.message)
        .collect();

    let session = RuntimeSession {
        id: session_id.to_string(),
        title,
        mode,
        messages,
        lineage: SessionLineage {
            parent_id,
            fork_point,
            branches: Vec::new(),
        },
        tokens: TokenDelta::default(),
        context: holmes_core::ContextSnapshot {
            summary: String::new(),
            preserved_keys: Vec::new(),
            active_contexts: Vec::new(),
            timestamp: chrono::Utc::now(),
        },
        created_at,
    };

    ReplayedSessionContext {
        session,
        system_prompt,
        model,
        active_tools,
        compactions,
        branch_summaries,
        semantic_complete: saw_session_created
            && saw_system_prompt
            && saw_mode
            && saw_model
            && saw_active_tools,
        warnings,
    }
}
