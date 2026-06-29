use holmes_core::event::{Event, StoredEvent};
use holmes_core::session::{RuntimeSession, SessionLineage};
use holmes_core::{Message, SessionMode, TokenDelta};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionReplayMarker {
    pub event_index: u64,
    pub summary: String,
    pub archive_path: Option<String>,
    pub archived_event_range: Option<ArchivedEventRange>,
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
    let mut messages = Vec::new();
    let mut compactions = Vec::new();
    let mut branch_summaries = Vec::new();
    let mut warnings = Vec::new();

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
                messages.push(Message::system(format!("[Branch summary]\n{summary}")));
            }
            Event::CompressionApplied {
                summary,
                archive_path,
                archived_event_range,
                ..
            } => {
                compactions.push(CompactionReplayMarker {
                    event_index: stored.event_index,
                    summary: summary.clone(),
                    archive_path: archive_path.clone(),
                    archived_event_range: archived_event_range
                        .map(|(start, end)| ArchivedEventRange { start, end }),
                });
                messages.push(Message::assistant(format!(
                    "[Compaction summary]\n{summary}"
                )));
            }
            Event::UserMessage { content, .. } => messages.push(Message::user(content.clone())),
            Event::Thinking { content, .. } => messages.push(Message::assistant(content.clone())),
            Event::ToolResult { name, content, .. } => messages.push(Message::tool_result(
                format!("replay-tool-call-{}", stored.event_index),
                name.clone(),
                content.clone(),
            )),
            Event::ToolCall { .. } => {}
            _ => {}
        }
    }

    if let Some(prompt) = &system_prompt {
        if !matches!(messages.first(), Some(message) if message.role == holmes_core::Role::System && message.content.as_deref() == Some(prompt.as_str()))
        {
            messages.insert(0, Message::system(prompt.clone()));
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
