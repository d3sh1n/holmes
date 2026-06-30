use anyhow::Result;
use holmes_core::{
    session::RuntimeSession, truncate_str, CompressionMethod, HolmesConfig, Message, Role,
};
use std::collections::HashSet;

#[derive(Debug, Clone, Default)]
pub struct CaseCompactor {
    last_summary: Option<String>,
}

impl CaseCompactor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_summary(&self) -> Option<&str> {
        self.last_summary.as_deref()
    }

    pub fn plan(
        &self,
        session: &RuntimeSession,
        config: &HolmesConfig,
        force: bool,
    ) -> CompressionPlan {
        let before_count = session.messages.len();
        let estimated_tokens = estimate_message_tokens(&session.messages);
        let threshold_tokens =
            (config.compressor.context_limit as f64 * config.compressor.threshold) as u64;
        let protected_head = config.compressor.protected_head.min(before_count);
        let protect_last_n = config.compressor.protect_last_n.min(before_count);
        let protected_tail_start = before_count.saturating_sub(protect_last_n);
        let has_middle = protected_head < protected_tail_start;
        let should_compress = config.compressor.enabled
            && has_middle
            && (force || estimated_tokens >= threshold_tokens.max(1));

        CompressionPlan {
            should_compress,
            force,
            before_count,
            estimated_tokens,
            threshold_tokens,
            protected_head,
            protected_tail_start,
            archived_message_range: has_middle.then_some((protected_head, protected_tail_start)),
        }
    }

    pub fn compress_session(
        &mut self,
        session: &mut RuntimeSession,
        config: &HolmesConfig,
        plan: CompressionPlan,
        trigger: holmes_core::CompactionTrigger,
    ) -> Result<Option<CompressionResult>> {
        if !plan.should_compress {
            return Ok(None);
        }

        let before_count = session.messages.len();
        let head_end = plan.protected_head.min(before_count);
        let tail_start = plan.protected_tail_start.min(before_count);
        if head_end >= tail_start {
            return Ok(None);
        }

        let middle = &session.messages[head_end..tail_start];
        let summary = build_static_summary(session, middle, config);
        let summary_message = Message::assistant(summary.clone());

        let mut compacted = Vec::new();
        compacted.extend_from_slice(&session.messages[..head_end]);
        compacted.push(summary_message);
        compacted.extend_from_slice(&session.messages[tail_start..]);
        sanitize_orphan_tool_messages(&mut compacted);

        session.messages = compacted;
        self.last_summary = Some(summary.clone());

        Ok(Some(CompressionResult {
            before_count,
            after_count: session.messages.len(),
            summary,
            preserved_keys: vec![
                "system_prompt".into(),
                "protected_head".into(),
                "protected_tail".into(),
            ],
            method: CompressionMethod::StaticFallback,
            archived_message_range: plan.archived_message_range,
            trigger,
            archive_path: None,
            archived_event_range: None,
        }))
    }
}

pub fn estimate_message_tokens(messages: &[Message]) -> u64 {
    let estimated_chars: usize = messages
        .iter()
        .map(|message| {
            let content_chars = message
                .content
                .as_deref()
                .unwrap_or_default()
                .chars()
                .count();
            let tool_call_chars = message
                .tool_calls
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|tool_call| tool_call.function.arguments.chars().count())
                .sum::<usize>();

            content_chars + tool_call_chars
        })
        .sum();

    let estimated_tokens = estimated_chars.div_ceil(4) as u64;

    estimated_tokens.max(messages.len() as u64)
}

fn build_static_summary(
    session: &RuntimeSession,
    middle: &[Message],
    config: &HolmesConfig,
) -> String {
    let active_goal = session
        .context
        .summary
        .trim()
        .is_empty()
        .then_some("none")
        .unwrap_or(session.context.summary.as_str());
    let middle_count = middle.len();
    let sample = middle
        .iter()
        .filter_map(|message| message.content.as_deref())
        .take(5)
        .collect::<Vec<_>>()
        .join("\n- ");

    let mut summary = format!(
        "## Case Compaction Summary\n\
         - Compacted middle messages: {middle_count}\n\
         - Active context summary: {active_goal}\n\
         - Compression target ratio: {}\n\
         \n\
         ## Preserved Evidence Notes\n\
         - {sample}",
        config.compressor.target_ratio
    );
    let max_summary_bytes = config.compressor.max_summary_tokens as usize * 4;
    if summary.len() > max_summary_bytes {
        summary = truncate_str(&summary, max_summary_bytes).to_string();
    }
    summary
}

fn sanitize_orphan_tool_messages(messages: &mut Vec<Message>) {
    let original = std::mem::take(messages);
    let mut sanitized = Vec::with_capacity(original.len());
    let mut index = 0;

    while index < original.len() {
        let message = original[index].clone();
        index += 1;

        if message.role == Role::Tool {
            continue;
        }

        let required_tool_results = if message.role == Role::Assistant {
            message
                .tool_calls
                .as_deref()
                .into_iter()
                .flat_map(|tool_calls| tool_calls.iter())
                .map(|tool_call| (tool_call.id.clone(), tool_call.function.name.clone()))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        if required_tool_results.is_empty() {
            sanitized.push(message);
            continue;
        }

        let required_tool_call_ids = required_tool_results
            .iter()
            .map(|(tool_call_id, _)| tool_call_id.as_str())
            .collect::<HashSet<_>>();
        let mut consumed_tool_call_ids = HashSet::new();
        let mut immediate_tool_results = Vec::new();

        while index < original.len() && original[index].role == Role::Tool {
            let tool_result = original[index].clone();
            index += 1;

            if let Some(tool_call_id) = tool_result.tool_call_id.as_deref() {
                if required_tool_call_ids.contains(tool_call_id)
                    && consumed_tool_call_ids.insert(tool_call_id.to_owned())
                {
                    immediate_tool_results.push(tool_result);
                }
            }
        }

        sanitized.push(message);
        sanitized.extend(immediate_tool_results);
        for (tool_call_id, tool_name) in required_tool_results {
            if !consumed_tool_call_ids.contains(&tool_call_id) {
                sanitized.push(Message::tool_result(
                    tool_call_id,
                    tool_name,
                    "[Old tool output cleared during case compaction]",
                ));
            }
        }
    }

    *messages = sanitized;
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompressionPlan {
    pub should_compress: bool,
    pub force: bool,
    pub before_count: usize,
    pub estimated_tokens: u64,
    pub threshold_tokens: u64,
    pub protected_head: usize,
    pub protected_tail_start: usize,
    pub archived_message_range: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct CompressionResult {
    pub before_count: usize,
    pub after_count: usize,
    pub summary: String,
    pub preserved_keys: Vec<String>,
    pub method: CompressionMethod,
    pub archived_message_range: Option<(usize, usize)>,
    pub trigger: holmes_core::CompactionTrigger,
    pub archive_path: Option<String>,
    pub archived_event_range: Option<(u64, u64)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::{session::RuntimeSession, FunctionCall, SessionMode, ToolCall};

    #[test]
    fn case_compactor_starts_without_summary() {
        let compactor = CaseCompactor::new();

        assert_eq!(compactor.last_summary(), None);
    }

    #[test]
    fn estimates_tokens_from_message_content() {
        let messages = vec![
            Message::system("system"),
            Message::user("12345678"),
            Message::assistant("abcdefghijkl"),
        ];

        assert_eq!(estimate_message_tokens(&messages), 7);
    }

    #[test]
    fn plans_compression_when_threshold_is_crossed() {
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 1;
        config.compressor.threshold = 1.0;

        let mut session = RuntimeSession::new("session-1".into(), SessionMode::Pentest)
            .with_system_prompt("system");
        for index in 0..24 {
            session.messages.push(if index % 2 == 0 {
                Message::user("middle content")
            } else {
                Message::assistant("middle content")
            });
        }

        let plan = CaseCompactor::new().plan(&session, &config, false);

        assert!(plan.should_compress);
        assert!(!plan.force);
        assert_eq!(plan.before_count, session.messages.len());
        assert_eq!(
            plan.estimated_tokens,
            estimate_message_tokens(&session.messages)
        );
        assert_eq!(plan.threshold_tokens, 1);
        assert_eq!(plan.protected_head, config.compressor.protected_head);
        assert_eq!(plan.protected_tail_start, 5);
    }

    #[test]
    fn skips_compression_when_disabled() {
        let mut config = HolmesConfig::default();
        config.compressor.enabled = false;
        config.compressor.context_limit = 1;
        config.compressor.threshold = 1.0;

        let mut session = RuntimeSession::new("session-1".into(), SessionMode::Pentest)
            .with_system_prompt("system");
        for index in 0..24 {
            session.messages.push(if index % 2 == 0 {
                Message::user("middle content")
            } else {
                Message::assistant("middle content")
            });
        }

        let plan = CaseCompactor::new().plan(&session, &config, false);

        assert!(!plan.should_compress);
        assert!(!plan.force);
        assert_eq!(plan.before_count, session.messages.len());
        assert_eq!(
            plan.estimated_tokens,
            estimate_message_tokens(&session.messages)
        );
        assert_eq!(plan.threshold_tokens, 1);
        assert_eq!(plan.protected_head, config.compressor.protected_head);
        assert_eq!(plan.protected_tail_start, 5);
    }

    #[test]
    fn static_compaction_preserves_head_summary_and_tail() {
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 20;
        config.compressor.threshold = 0.5;
        config.compressor.protected_head = 1;
        config.compressor.protect_last_n = 2;

        let mut session = RuntimeSession::new("s".into(), SessionMode::Pentest)
            .with_system_prompt("system prompt");
        session
            .messages
            .push(Message::user("initial authorized objective"));
        session.messages.push(Message::assistant("old reasoning"));
        session.messages.push(Message::user("latest question"));
        session.messages.push(Message::assistant("latest answer"));

        let mut compactor = CaseCompactor::default();
        let plan = compactor.plan(&session, &config, true);
        assert_eq!(plan.archived_message_range, Some((plan.protected_head, plan.protected_tail_start)));
        let result = compactor
            .compress_session(
                &mut session,
                &config,
                plan,
                holmes_core::CompactionTrigger::Manual,
            )
            .expect("compression result")
            .expect("compressed");
        assert_eq!(result.trigger, holmes_core::CompactionTrigger::Manual);
        assert!(result.archived_message_range.is_some());

        assert_eq!(result.before_count, 5);
        assert!(result.after_count < result.before_count);
        assert!(matches!(result.method, CompressionMethod::StaticFallback));
        assert_eq!(compactor.last_summary(), Some(result.summary.as_str()));
        assert!(result
            .preserved_keys
            .iter()
            .any(|key| key == "system_prompt"));
        assert!(result
            .preserved_keys
            .iter()
            .any(|key| key == "protected_head"));
        assert!(result
            .preserved_keys
            .iter()
            .any(|key| key == "protected_tail"));
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[0].role, Role::System);
        assert_eq!(
            session.messages[0].content.as_deref(),
            Some("system prompt")
        );
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(
            session.messages[1].content.as_deref(),
            Some(result.summary.as_str())
        );
        assert!(session.messages[1]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("Case Compaction Summary"));
        assert_eq!(session.messages[2].role, Role::User);
        assert_eq!(
            session.messages[2].content.as_deref(),
            Some("latest question")
        );
        assert_eq!(session.messages[3].role, Role::Assistant);
        assert_eq!(
            session.messages[3].content.as_deref(),
            Some("latest answer")
        );
    }

    #[test]
    fn sanitizes_compacted_tool_messages() {
        let preserved_call = ToolCall {
            id: "preserved-call".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "lookup".into(),
                arguments: "{}".into(),
            },
        };
        let mut messages = vec![
            Message::system("system prompt"),
            Message::tool_result("orphan-call", "orphan", "orphan result"),
            Message::assistant_with_tool_calls(vec![preserved_call]),
            Message::user("latest question"),
        ];

        sanitize_orphan_tool_messages(&mut messages);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("preserved-call"));
        assert_eq!(messages[2].name.as_deref(), Some("lookup"));
        assert_eq!(
            messages[2].content.as_deref(),
            Some("[Old tool output cleared during case compaction]")
        );
        assert_eq!(messages[3].role, Role::User);
        assert!(messages
            .iter()
            .all(|message| message.tool_call_id.as_deref() != Some("orphan-call")));
    }

    #[test]
    fn compaction_stubs_cross_boundary_tool_result_after_summary() {
        let preserved_call = ToolCall {
            id: "preserved-call".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "lookup".into(),
                arguments: "{}".into(),
            },
        };
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 20;
        config.compressor.threshold = 0.5;
        config.compressor.protected_head = 2;
        config.compressor.protect_last_n = 1;

        let mut session =
            RuntimeSession::new("s".into(), SessionMode::Pentest).with_system_prompt("system");
        session
            .messages
            .push(Message::assistant_with_tool_calls(vec![preserved_call]));
        session
            .messages
            .push(Message::user("middle message to summarize"));
        session.messages.push(Message::tool_result(
            "preserved-call",
            "lookup",
            "real separated result",
        ));

        let mut compactor = CaseCompactor::default();
        let plan = compactor.plan(&session, &config, true);
        assert_eq!(plan.archived_message_range, Some((plan.protected_head, plan.protected_tail_start)));
        let result = compactor
            .compress_session(
                &mut session,
                &config,
                plan,
                holmes_core::CompactionTrigger::Manual,
            )
            .expect("compression result")
            .expect("compressed");
        assert_eq!(result.trigger, holmes_core::CompactionTrigger::Manual);
        assert!(result.archived_message_range.is_some());

        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[0].role, Role::System);
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[2].role, Role::Tool);
        assert_eq!(
            session.messages[2].tool_call_id.as_deref(),
            Some("preserved-call")
        );
        assert_eq!(session.messages[2].name.as_deref(), Some("lookup"));
        assert_eq!(
            session.messages[2].content.as_deref(),
            Some("[Old tool output cleared during case compaction]")
        );
        assert_eq!(session.messages[3].role, Role::Assistant);
        assert_eq!(
            session.messages[3].content.as_deref(),
            Some(result.summary.as_str())
        );
        assert!(session
            .messages
            .iter()
            .all(|message| { message.content.as_deref() != Some("real separated result") }));
    }
}
