use holmes_core::event::{Event, StoredEvent};
use holmes_core::session::{RuntimeSession, SessionLineage};
use holmes_core::{FunctionCall, Message, SessionMode, TokenDelta, ToolCall};
use serde::{Deserialize, Serialize};

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
    let mut pending_tool_calls: Vec<ToolCall> = Vec::new();

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
            Event::UserMessage { content, .. } => messages.push(ReplayMessage::new(
                Message::user(content.clone()),
                stored.event_index,
            )),
            Event::Thinking { content, .. } => messages.push(ReplayMessage::new(
                Message::assistant(content.clone()),
                stored.event_index,
            )),
            Event::ToolCall {
                name, arguments, ..
            } => {
                let tool_call = ToolCall {
                    id: format!("replay-tool-call-{}", stored.event_index),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: name.clone(),
                        arguments: arguments.to_string(),
                    },
                };
                pending_tool_calls.push(tool_call.clone());
                messages.push(ReplayMessage::new(
                    Message::assistant_with_tool_calls(vec![tool_call]),
                    stored.event_index,
                ));
            }
            Event::ToolResult { name, content, .. } => {
                if let Some(position) = pending_tool_calls
                    .iter()
                    .position(|call| call.function.name == *name)
                {
                    let tool_call = pending_tool_calls.remove(position);
                    messages.push(ReplayMessage::new(
                        Message::tool_result(tool_call.id, name.clone(), content.clone()),
                        stored.event_index,
                    ));
                } else {
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
            _ => {}
        }
    }

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
