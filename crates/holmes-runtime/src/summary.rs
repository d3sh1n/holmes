use holmes_core::event::{Event, StoredEvent};
use holmes_core::{truncate_str, Message, SummaryMethod};

/// A generated summary plus the method used to produce it.
#[derive(Debug, Clone)]
pub struct GeneratedSummary {
    pub summary: String,
    pub method: SummaryMethod,
}

/// Build a deterministic branch summary from an event window.
///
/// This is the static fallback used when an LLM summary is unavailable or
/// fails. It surfaces the abandoned/parent path's user objectives, tool usage,
/// findings, errors, and the last assistant note so the new branch retains
/// context about why the previous path mattered.
pub fn static_branch_summary(events: &[StoredEvent], reason: &str) -> String {
    let mut user_messages = Vec::new();
    let mut tool_calls = Vec::new();
    let mut findings = Vec::new();
    let mut errors = Vec::new();
    let mut last_assistant = None;

    for stored in events {
        match &stored.event {
            Event::UserMessage { content, .. } => user_messages.push(content.clone()),
            Event::Thinking { content, .. } => last_assistant = Some(content.clone()),
            Event::ToolCall { name, .. } => tool_calls.push(name.clone()),
            Event::ToolResult {
                name,
                success,
                error,
                ..
            } => {
                if !success {
                    errors.push(format!("{}: {}", name, error.as_deref().unwrap_or("failed")));
                }
            }
            Event::VulnerabilityFound {
                title, evidence, ..
            } => {
                findings.push(format!("{} — {}", title, evidence));
            }
            _ => {}
        }
    }

    let summary = format!(
        "Branch summary ({reason})\n- User objectives: {}\n- Tools used: {}\n- Findings: {}\n- Errors: {}\n- Last assistant note: {}",
        join_or_none(&user_messages),
        join_or_none(&tool_calls),
        join_or_none(&findings),
        join_or_none(&errors),
        last_assistant.as_deref().unwrap_or("none"),
    );
    truncate_str(&summary, 1200).to_string()
}

/// Build a deterministic compaction summary from the messages being compacted.
///
/// This is the static fallback used when an LLM summary is unavailable.
pub fn static_compaction_summary(messages: &[Message]) -> String {
    let sample = messages
        .iter()
        .filter_map(|message| message.content.as_deref())
        .take(8)
        .collect::<Vec<_>>()
        .join("\n- ");
    truncate_str(
        &format!(
            "Compaction summary\n- Messages compacted: {}\n- Preserved notes:\n- {}",
            messages.len(),
            sample
        ),
        1600,
    )
    .to_string()
}

/// Wrap a static summary string into a `GeneratedSummary` tagged as a fallback.
pub fn fallback_summary(summary: String) -> GeneratedSummary {
    GeneratedSummary {
        summary,
        method: SummaryMethod::StaticFallback,
    }
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::event::Event;

    fn stored(event_index: u64, event: Event) -> StoredEvent {
        StoredEvent {
            id: event_index,
            session_id: "s".into(),
            event_index,
            turn_index: None,
            timestamp: chrono::Utc::now(),
            event,
        }
    }

    #[test]
    fn static_branch_summary_mentions_user_tools_and_findings() {
        let now = chrono::Utc::now();
        let events = vec![
            stored(
                0,
                Event::UserMessage {
                    content: "test login".into(),
                    timestamp: now,
                },
            ),
            stored(
                1,
                Event::ToolCall {
                    name: "http_request".into(),
                    arguments: serde_json::json!({"url":"/login"}),
                    purpose: Some("probe".into()),
                },
            ),
            stored(
                2,
                Event::VulnerabilityFound {
                    title: "IDOR".into(),
                    cwe: None,
                    cvss: None,
                    severity: holmes_core::Severity::High,
                    location: "/api/users/2".into(),
                    evidence: "user2 data returned".into(),
                    poc: None,
                    status: holmes_core::FindingStatus::Suspicious,
                },
            ),
        ];
        let summary = static_branch_summary(&events, "fork");
        assert!(summary.contains("test login"));
        assert!(summary.contains("http_request"));
        assert!(summary.contains("IDOR"));
        assert!(summary.contains("fork"));
    }

    #[test]
    fn static_branch_summary_handles_empty_window() {
        let summary = static_branch_summary(&[], "tree_fork");
        assert!(summary.contains("tree_fork"));
        assert!(summary.contains("none"));
    }

    #[test]
    fn static_compaction_summary_reports_count_and_sample() {
        let messages = vec![
            Message::user("first objective"),
            Message::assistant("did some recon"),
        ];
        let summary = static_compaction_summary(&messages);
        assert!(summary.contains("Messages compacted: 2"));
        assert!(summary.contains("first objective"));
    }

    #[test]
    fn fallback_summary_is_tagged_static() {
        let generated = fallback_summary("x".into());
        assert_eq!(generated.method, SummaryMethod::StaticFallback);
        assert_eq!(generated.summary, "x");
    }
}
