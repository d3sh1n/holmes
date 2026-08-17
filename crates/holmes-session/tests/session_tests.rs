use holmes_core::event::{Event, StoredEvent};
use holmes_core::types::*;
use holmes_core::{CompactionTrigger, CompressionMethod, Role, SummaryMethod};
use holmes_session::db::*;
use holmes_session::replay_events;
use holmes_session::SessionStore;
use std::path::{Component, Path};

#[tokio::test]
async fn test_full_session_lifecycle() {
    let db = SessionDB::open(":memory:").await.unwrap();

    let session = db
        .create_session(CreateSessionParams {
            id: None,
            title: Some("integration test".into()),
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();

    let event = Event::UserMessage {
        content: "test message".into(),
        timestamp: chrono::Utc::now(),
    };
    db.append_event(&session.id, &event).await.unwrap();

    let events = db.get_events(&session.id).await.unwrap();
    assert_eq!(events.len(), 1);

    db.end_session(&session.id, EndReason::UserQuit)
        .await
        .unwrap();
}

#[tokio::test]
async fn compaction_archive_round_trips_through_session_store() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let db_path = temp_dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();

    let session = db
        .create_session(CreateSessionParams {
            id: Some("archive_session".into()),
            title: Some("archive test".into()),
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();

    let event = Event::UserMessage {
        content: "old context".into(),
        timestamp: chrono::Utc::now(),
    };
    db.append_event(&session.id, &event).await.unwrap();
    let stored = db.get_events(&session.id).await.unwrap();

    let archive = holmes_session::CompactionArchive {
        schema_version: holmes_session::COMPACTION_ARCHIVE_SCHEMA_VERSION,
        session_id: session.id.clone(),
        compaction_event_index: 7,
        trigger: holmes_core::CompactionTrigger::Manual,
        archived_event_range: Some(holmes_session::ArchivedEventRange::new(0, 0)),
        messages: vec![holmes_core::Message::user("old context")],
        events: stored
            .iter()
            .map(holmes_session::ArchivedEvent::from_stored)
            .collect(),
        created_at: chrono::Utc::now(),
    };

    let path = db
        .write_compaction_archive(&session.id, 7, &archive)
        .await
        .unwrap();
    let expected_path = sessions_dir_for(&db_path)
        .join("archive_session")
        .join("compactions")
        .join("compaction_7.json");
    assert_eq!(Path::new(&path), expected_path.as_path());
    assert!(Path::new(&path).components().any(|component| {
        matches!(component, Component::Normal(name) if name == "archive_session")
    }));

    let loaded = db.read_compaction_archive(&path).await.unwrap();
    assert_eq!(
        loaded.schema_version,
        holmes_session::COMPACTION_ARCHIVE_SCHEMA_VERSION
    );
    assert_eq!(loaded.session_id, session.id);
    assert_eq!(loaded.compaction_event_index, 7);
    let archived_range = loaded.archived_event_range.unwrap();
    assert_eq!(archived_range.start, 0);
    assert_eq!(archived_range.end, 0);
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.events.len(), 1);
}

#[tokio::test]
async fn rejects_compaction_archive_paths_outside_sessions_dir() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let db_path = temp_dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();

    let outside_dir = tempfile::TempDir::new().unwrap();
    let outside_archive = outside_dir.path().join("compaction_7.json");
    std::fs::write(&outside_archive, "{}").unwrap();

    let error = db
        .read_compaction_archive(outside_archive.to_str().unwrap())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outside sessions directory"));

    let error = db.read_compaction_archive("/etc/passwd").await.unwrap_err();
    assert!(error.to_string().contains("outside sessions directory"));
}

#[tokio::test]
async fn rejects_invalid_session_workspace_ids() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let db_path = temp_dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();

    let error = db.session_workspace("../escape").await.unwrap_err();
    assert!(error.to_string().contains("invalid session id"));

    let error = db.session_workspace("").await.unwrap_err();
    assert!(error.to_string().contains("invalid session id"));
}
#[tokio::test]
async fn replay_complete_semantic_session_restores_metadata_and_messages() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let session = db
        .create_session(CreateSessionParams {
            id: Some("semantic_session".into()),
            title: Some("semantic".into()),
            mode: Some(SessionMode::Pentest),
            model: Some("claude-sonnet-4-6".into()),
            system_prompt: Some("old table prompt".into()),
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();
    let now = chrono::Utc::now();

    db.append_event(
        &session.id,
        &Event::SessionCreated {
            id: session.id.clone(),
            title: session.title.clone(),
            mode: SessionMode::Pentest,
            model: Some("claude-sonnet-4-6".into()),
            system_prompt: Some("semantic prompt".into()),
            parent_id: None,
            fork_point: None,
            created_at: now,
            tags: vec![],
        },
    )
    .await
    .unwrap();
    db.append_event(
        &session.id,
        &Event::SessionSystemPromptSet {
            prompt_hash: "hash-semantic".into(),
            content: "semantic prompt".into(),
            source: "startup".into(),
            timestamp: now,
        },
    )
    .await
    .unwrap();
    db.append_event(
        &session.id,
        &Event::SessionModeSet {
            mode: SessionMode::SecurityResearch,
            source: Some("startup".into()),
            timestamp: Some(now),
        },
    )
    .await
    .unwrap();
    db.append_event(
        &session.id,
        &Event::SessionModelSet {
            model: "claude-opus-4-8".into(),
            provider: Some("default".into()),
            source: "startup".into(),
            timestamp: now,
        },
    )
    .await
    .unwrap();
    db.append_event(
        &session.id,
        &Event::ActiveToolsSet {
            tool_names: vec!["http_request".into(), "report_finding".into()],
            source: "startup".into(),
            timestamp: now,
        },
    )
    .await
    .unwrap();
    db.append_event(
        &session.id,
        &Event::UserMessage {
            content: "hello".into(),
            timestamp: now,
        },
    )
    .await
    .unwrap();

    let replayed = db.replay_session_context(&session.id).await.unwrap();
    assert!(replayed.semantic_complete);
    assert_eq!(replayed.system_prompt.as_deref(), Some("semantic prompt"));
    assert_eq!(replayed.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(
        replayed.active_tools,
        vec!["http_request", "report_finding"]
    );
    assert_eq!(replayed.session.mode, SessionMode::SecurityResearch);
    assert_eq!(
        replayed.session.messages[0].content.as_deref(),
        Some("semantic prompt")
    );
    assert_eq!(
        replayed.session.messages.last().unwrap().content.as_deref(),
        Some("hello")
    );
}

#[tokio::test]
async fn replay_legacy_session_reports_incomplete() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let session = db
        .create_session(CreateSessionParams {
            id: Some("legacy_session".into()),
            title: Some("legacy".into()),
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();
    db.append_event(
        &session.id,
        &Event::UserMessage {
            content: "legacy hello".into(),
            timestamp: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let replayed = db.replay_session_context(&session.id).await.unwrap();
    assert!(!replayed.semantic_complete);
    assert_eq!(replayed.session.messages.len(), 1);
}

fn stored_event(event_index: u64, event: Event) -> StoredEvent {
    StoredEvent {
        id: event_index,
        session_id: "replay_test".into(),
        event_index,
        turn_index: None,
        timestamp: chrono::Utc::now(),
        event,
    }
}

fn session_created_with_prompt(prompt: &str) -> Event {
    Event::SessionCreated {
        id: "replay_test".into(),
        title: Some("replay".into()),
        mode: SessionMode::Pentest,
        model: Some("claude-opus-4-8".into()),
        system_prompt: Some(prompt.into()),
        parent_id: None,
        fork_point: None,
        created_at: chrono::Utc::now(),
        tags: vec![],
    }
}

#[test]
fn replay_branch_summary_keeps_primary_prompt_as_only_system_message() {
    let now = chrono::Utc::now();
    let replayed = replay_events(
        "replay_test",
        &[
            stored_event(0, session_created_with_prompt("real Holmes prompt")),
            stored_event(
                1,
                Event::SessionSystemPromptSet {
                    prompt_hash: "hash".into(),
                    content: "real Holmes prompt".into(),
                    source: "test".into(),
                    timestamp: now,
                },
            ),
            stored_event(
                2,
                Event::BranchSummary {
                    from_event_index: 0,
                    to_event_index: 1,
                    summary: "branch-only context".into(),
                    reason: "fork".into(),
                    method: SummaryMethod::StaticFallback,
                    timestamp: now,
                },
            ),
        ],
    );

    let messages = &replayed.session.messages;
    assert_eq!(messages.first().map(|m| &m.role), Some(&Role::System));
    assert_eq!(
        messages.first().and_then(|m| m.content.as_deref()),
        Some("real Holmes prompt")
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == Role::System)
            .count(),
        1
    );
    let branch_summary = messages
        .iter()
        .find(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("branch-only context"))
        })
        .expect("branch summary should be replayed");
    assert_ne!(branch_summary.role, Role::System);
}

#[tokio::test]
async fn fork_session_preserves_event_indices_for_compaction_archived_ranges() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let db_path = temp_dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();

    let parent = db
        .create_session(CreateSessionParams {
            id: Some("parent_compacted".into()),
            title: Some("parent".into()),
            mode: Some(SessionMode::Pentest),
            model: Some("startup-model".into()),
            system_prompt: Some("startup prompt".into()),
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();
    let now = chrono::Utc::now();

    for event in [
        Event::SessionCreated {
            id: parent.id.clone(),
            title: parent.title.clone(),
            mode: SessionMode::Pentest,
            model: Some("startup-model".into()),
            system_prompt: Some("startup prompt".into()),
            parent_id: None,
            fork_point: None,
            created_at: now,
            tags: vec![],
        },
        Event::SessionSystemPromptSet {
            prompt_hash: "startup-prompt-hash".into(),
            content: "startup prompt".into(),
            source: "startup".into(),
            timestamp: now,
        },
        Event::SessionModeSet {
            mode: SessionMode::Pentest,
            source: Some("startup".into()),
            timestamp: Some(now),
        },
        Event::SessionModelSet {
            model: "startup-model".into(),
            provider: Some("startup-provider".into()),
            source: "startup".into(),
            timestamp: now,
        },
        Event::ActiveToolsSet {
            tool_names: vec!["tool_a".into()],
            source: "startup".into(),
            timestamp: now,
        },
    ] {
        db.append_event(&parent.id, &event).await.unwrap();
    }

    let archived_user_index = db
        .append_event(
            &parent.id,
            &Event::UserMessage {
                content: "archived user text".into(),
                timestamp: now,
            },
        )
        .await
        .unwrap();
    let archived_assistant_index = db
        .append_event(
            &parent.id,
            &Event::Thinking {
                content: "archived assistant text".into(),
                reasoning_type: None,
            },
        )
        .await
        .unwrap();
    let compaction_index = db
        .append_event(
            &parent.id,
            &Event::CompressionApplied {
                before_count: 2,
                after_count: 1,
                summary: "summary replaces archived pair".into(),
                preserved_keys: vec![],
                method: CompressionMethod::StaticFallback,
                preserved_head: None,
                preserved_tail_tokens: None,
                archive_path: Some("sessions/parent_compacted/compactions/compaction.json".into()),
                archived_event_range: Some((archived_user_index, archived_assistant_index)),
                trigger: Some(CompactionTrigger::Manual),
                timestamp: Some(now),
            },
        )
        .await
        .unwrap();
    let tail_index = db
        .append_event(
            &parent.id,
            &Event::UserMessage {
                content: "tail user text".into(),
                timestamp: now,
            },
        )
        .await
        .unwrap();

    let child = db
        .fork_session(&parent.id, tail_index, "child")
        .await
        .unwrap();

    let child_events = db.get_events(&child.id).await.unwrap();
    let child_indices = child_events
        .iter()
        .map(|stored| stored.event_index)
        .collect::<Vec<_>>();
    assert_eq!(
        child_indices,
        vec![
            archived_user_index,
            archived_assistant_index,
            compaction_index,
            tail_index,
        ],
        "forked events must keep parent indices so compaction ranges still resolve"
    );
    assert!(child_events.iter().any(|stored| {
        stored.event_index == archived_user_index
            && matches!(&stored.event, Event::UserMessage { content, .. } if content == "archived user text")
    }));
    assert!(child_events.iter().any(|stored| {
        stored.event_index == archived_assistant_index
            && matches!(&stored.event, Event::Thinking { content, .. } if content == "archived assistant text")
    }));
    let child_archived_range = child_events
        .iter()
        .find_map(|stored| match &stored.event {
            Event::CompressionApplied {
                archived_event_range,
                ..
            } => *archived_event_range,
            _ => None,
        })
        .expect("child should copy compaction marker");
    assert_eq!(
        child_archived_range,
        (archived_user_index, archived_assistant_index)
    );

    let replayed = db.replay_session_context(&child.id).await.unwrap();
    let contents = replayed
        .session
        .messages
        .iter()
        .filter_map(|message| message.content.as_deref())
        .collect::<Vec<_>>();
    assert!(contents
        .iter()
        .any(|content| content.contains("[Compaction summary]\nsummary replaces archived pair")));
    assert!(contents.contains(&"tail user text"));
    assert!(!contents.contains(&"archived user text"));
    assert!(!contents.contains(&"archived assistant text"));
}

#[test]
fn replay_compaction_summary_replaces_archived_context_range() {
    let now = chrono::Utc::now();
    let replayed = replay_events(
        "replay_test",
        &[
            stored_event(0, session_created_with_prompt("system prompt")),
            stored_event(
                1,
                Event::SessionSystemPromptSet {
                    prompt_hash: "hash".into(),
                    content: "system prompt".into(),
                    source: "test".into(),
                    timestamp: now,
                },
            ),
            stored_event(
                2,
                Event::UserMessage {
                    content: "A".into(),
                    timestamp: now,
                },
            ),
            stored_event(
                3,
                Event::Thinking {
                    content: "B".into(),
                    reasoning_type: None,
                },
            ),
            stored_event(
                4,
                Event::CompressionApplied {
                    before_count: 2,
                    after_count: 1,
                    summary: "compacted A and B".into(),
                    preserved_keys: vec![],
                    method: CompressionMethod::StaticFallback,
                    preserved_head: None,
                    preserved_tail_tokens: None,
                    archive_path: Some("sessions/replay_test/compactions/compaction_4.json".into()),
                    archived_event_range: Some((2, 3)),
                    trigger: Some(CompactionTrigger::Manual),
                    timestamp: Some(now),
                },
            ),
            stored_event(
                5,
                Event::UserMessage {
                    content: "C".into(),
                    timestamp: now,
                },
            ),
        ],
    );

    let messages = &replayed.session.messages;
    assert_eq!(messages.first().map(|m| &m.role), Some(&Role::System));
    assert_eq!(
        messages.first().and_then(|m| m.content.as_deref()),
        Some("system prompt")
    );
    let contents = messages
        .iter()
        .filter_map(|message| message.content.as_deref())
        .collect::<Vec<_>>();
    assert!(contents
        .iter()
        .any(|content| content.contains("[Compaction summary]\ncompacted A and B")));
    assert!(contents.contains(&"C"));
    assert!(!contents.contains(&"A"));
    assert!(!contents.contains(&"B"));
}

#[test]
fn replay_injects_compaction_summary_and_marker() {
    let now = chrono::Utc::now();
    let replayed = replay_events(
        "marker_test",
        &[
            stored_event(0, session_created_with_prompt("system prompt")),
            stored_event(
                1,
                Event::SessionSystemPromptSet {
                    prompt_hash: "hash".into(),
                    content: "system prompt".into(),
                    source: "test".into(),
                    timestamp: now,
                },
            ),
            stored_event(
                2,
                Event::UserMessage {
                    content: "old work".into(),
                    timestamp: now,
                },
            ),
            stored_event(
                3,
                Event::CompressionApplied {
                    before_count: 1,
                    after_count: 1,
                    summary: "old work summarized".into(),
                    preserved_keys: vec!["system_prompt".into()],
                    method: CompressionMethod::StaticFallback,
                    preserved_head: Some(1),
                    preserved_tail_tokens: Some(4000),
                    // Intentionally a missing/unreadable archive path: replay must
                    // not block on it and must still record the marker + summary.
                    archive_path: Some("sessions/marker_test/compactions/missing.json".into()),
                    archived_event_range: Some((2, 2)),
                    trigger: Some(CompactionTrigger::Manual),
                    timestamp: Some(now),
                },
            ),
        ],
    );

    // The compaction marker is recorded even though the archive file is absent.
    assert_eq!(replayed.compactions.len(), 1);
    let marker = &replayed.compactions[0];
    assert_eq!(marker.event_index, 3);
    assert_eq!(marker.summary, "old work summarized");
    assert_eq!(
        marker.archive_path.as_deref(),
        Some("sessions/marker_test/compactions/missing.json")
    );
    assert!(marker.archived_event_range.is_some());

    // The summary is injected into the replayed context.
    assert!(replayed
        .session
        .messages
        .iter()
        .filter_map(|message| message.content.as_deref())
        .any(|content| content.contains("old work summarized")));
}

#[test]
fn replay_tool_call_followed_by_result_uses_matching_tool_result_id() {
    let replayed = replay_events(
        "replay_test",
        &[
            stored_event(
                0,
                Event::ToolCall {
                    name: "http_request".into(),
                    arguments: serde_json::json!({"url": "https://example.com"}),
                    purpose: Some("fetch".into()),
                    call_id: None,
                },
            ),
            stored_event(
                1,
                Event::ToolResult {
                    name: "http_request".into(),
                    success: true,
                    outcome: Some(holmes_core::ToolOutcomeStatus::Succeeded),
                    content: "ok".into(),
                    error: None,
                    artifacts: vec![],
                    call_id: None,
                },
            ),
        ],
    );

    assert_eq!(replayed.session.messages.len(), 2);
    let assistant = &replayed.session.messages[0];
    assert_eq!(assistant.role, Role::Assistant);
    let tool_call = assistant
        .tool_calls
        .as_ref()
        .and_then(|calls| calls.first())
        .expect("tool call should replay as assistant tool call");
    assert_eq!(tool_call.function.name, "http_request");
    assert_eq!(
        tool_call.function.arguments,
        r#"{"url":"https://example.com"}"#
    );

    let result = &replayed.session.messages[1];
    assert_eq!(result.role, Role::Tool);
    assert_eq!(result.tool_call_id.as_deref(), Some(tool_call.id.as_str()));
    assert_eq!(result.name.as_deref(), Some("http_request"));
    assert_eq!(result.content.as_deref(), Some("ok"));
}

#[test]
fn replay_orphan_tool_result_becomes_non_tool_historical_context() {
    let replayed = replay_events(
        "replay_test",
        &[stored_event(
            0,
            Event::ToolResult {
                name: "http_request".into(),
                success: false,
                outcome: Some(holmes_core::ToolOutcomeStatus::Failed),
                content: "orphan output".into(),
                error: Some("failed".into()),
                artifacts: vec![],
                call_id: None,
            },
        )],
    );

    assert!(replayed
        .session
        .messages
        .iter()
        .all(|message| message.role != Role::Tool));
    let context = replayed
        .session
        .messages
        .iter()
        .find(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("orphan output"))
        })
        .expect("orphan tool result should be preserved as text context");
    assert_ne!(context.role, Role::System);
}

/// P1-03 legality invariant: every assistant tool_use in the replayed history is
/// answered by exactly one tool_result message carrying the same call id.
fn assert_tool_history_is_legal(messages: &[holmes_core::Message]) {
    let answered: std::collections::HashSet<&str> = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect();
    for message in messages {
        if message.role != Role::Assistant {
            continue;
        }
        for call in message.tool_calls.as_deref().unwrap_or(&[]) {
            assert!(
                answered.contains(call.id.as_str()),
                "tool_use '{}' ({}) has no matching tool_result",
                call.function.name,
                call.id
            );
        }
    }
}

#[test]
fn replay_blocked_tool_synthesizes_failure_tool_result() {
    // Legacy events (no call_id): a blocked call must still yield a tool_result
    // so the replayed history stays legal (P1-03).
    let replayed = replay_events(
        "replay_test",
        &[
            stored_event(
                0,
                Event::ToolCall {
                    name: "http_request".into(),
                    arguments: serde_json::json!({"url": "https://example.com"}),
                    purpose: None,
                    call_id: None,
                },
            ),
            stored_event(
                1,
                Event::ToolBlocked {
                    tool_name: "http_request".into(),
                    guard_name: "scope".into(),
                    reason: "target outside authorized scope".into(),
                    call_id: None,
                },
            ),
        ],
    );

    assert_tool_history_is_legal(&replayed.session.messages);
    assert_eq!(replayed.session.messages.len(), 2);
    let assistant = &replayed.session.messages[0];
    let call_id = assistant.tool_calls.as_ref().expect("tool calls")[0]
        .id
        .clone();
    let result = &replayed.session.messages[1];
    assert_eq!(result.role, Role::Tool);
    assert_eq!(result.tool_call_id.as_deref(), Some(call_id.as_str()));
    let content = result.content.as_deref().expect("blocked content");
    assert!(content.contains("scope"), "got: {content}");
    assert!(
        content.contains("target outside authorized scope"),
        "got: {content}"
    );
}

#[test]
fn replay_parallel_same_name_calls_bind_results_by_call_id() {
    // Two parallel calls to the same tool, results persisted out of order: each
    // result must bind to its own call id (and therefore its own arguments).
    let replayed = replay_events(
        "replay_test",
        &[
            stored_event(
                0,
                Event::ToolCall {
                    name: "http_request".into(),
                    arguments: serde_json::json!({"url": "https://a.example"}),
                    purpose: None,
                    call_id: Some("call-a".into()),
                },
            ),
            stored_event(
                1,
                Event::ToolCall {
                    name: "http_request".into(),
                    arguments: serde_json::json!({"url": "https://b.example"}),
                    purpose: None,
                    call_id: Some("call-b".into()),
                },
            ),
            stored_event(
                2,
                Event::ToolResult {
                    name: "http_request".into(),
                    success: true,
                    outcome: Some(holmes_core::ToolOutcomeStatus::Succeeded),
                    content: "response-for-b".into(),
                    error: None,
                    artifacts: vec![],
                    call_id: Some("call-b".into()),
                },
            ),
            stored_event(
                3,
                Event::ToolResult {
                    name: "http_request".into(),
                    success: true,
                    outcome: Some(holmes_core::ToolOutcomeStatus::Succeeded),
                    content: "response-for-a".into(),
                    error: None,
                    artifacts: vec![],
                    call_id: Some("call-a".into()),
                },
            ),
        ],
    );

    assert_tool_history_is_legal(&replayed.session.messages);
    // One merged assistant message (the run is one model response's tool batch),
    // followed by one result per call.
    let assistant = &replayed.session.messages[0];
    assert_eq!(assistant.role, Role::Assistant);
    let calls = assistant.tool_calls.as_ref().expect("tool calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].function.arguments,
        r#"{"url":"https://a.example"}"#
    );
    assert_eq!(
        calls[1].function.arguments,
        r#"{"url":"https://b.example"}"#
    );

    let result_for = |id: &str| {
        replayed
            .session
            .messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some(id))
            .and_then(|message| message.content.as_deref())
            .expect("result content")
    };
    assert_eq!(result_for("call-a"), "response-for-a");
    assert_eq!(result_for("call-b"), "response-for-b");
}

#[test]
fn replay_interrupted_tool_call_gets_synthesized_result() {
    // Crash between ToolCall and its outcome event: replay closes the dangling
    // tool_use with a synthesized failure result instead of leaving illegal
    // history (P1-03).
    let replayed = replay_events(
        "replay_test",
        &[stored_event(
            0,
            Event::ToolCall {
                name: "execute_command".into(),
                arguments: serde_json::json!({"command": "nmap -sV target"}),
                purpose: None,
                call_id: Some("call-crash".into()),
            },
        )],
    );

    assert_tool_history_is_legal(&replayed.session.messages);
    let result = replayed
        .session
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("call-crash"))
        .expect("synthesized result");
    assert!(
        result
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("interrupted"),
        "got: {:?}",
        result.content
    );
    assert!(replayed
        .warnings
        .iter()
        .any(|warning| warning.contains("no result or blocked event")));
}

#[tokio::test]
async fn pre_call_id_database_migrates_and_legacy_events_read() {
    // P1-03: a database written before call-id persistence (schema v4, event
    // payloads without `call_id`) must still open — the v5 migration applies —
    // and its legacy events must read and replay as a legal history.
    let temp_dir = std::env::temp_dir().join(format!("holmes_legacy_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("legacy.db");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(holmes_session::schema::schema_version_table())
            .unwrap();
        for (index, migration) in holmes_session::schema::MIGRATIONS
            .iter()
            .take(4)
            .enumerate()
        {
            conn.execute_batch(migration).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                rusqlite::params![(index + 1) as u32],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO sessions (id, started_at) VALUES ('legacy-session', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
             VALUES ('legacy-session', 0, 'tool_call',
                     '{\"type\":\"tool_call\",\"name\":\"nmap\",\"arguments\":{\"target\":\"10.0.0.9\"},\"purpose\":null}',
                     datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
             VALUES ('legacy-session', 1, 'tool_blocked',
                     '{\"type\":\"tool_blocked\",\"tool_name\":\"nmap\",\"guard_name\":\"scope\",\"reason\":\"target outside authorized scope\"}',
                     datetime('now'))",
            [],
        )
        .unwrap();
    }

    let db = SessionDB::open(&db_path).await.unwrap();
    // The v5 migration must have been applied on open.
    let version: u32 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, holmes_session::schema::SCHEMA_VERSION);

    let events = db.get_events("legacy-session").await.unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[0].event,
        Event::ToolCall { name, call_id: None, .. } if name == "nmap"
    ));
    assert!(matches!(
        &events[1].event,
        Event::ToolBlocked { tool_name, guard_name, call_id: None, .. }
            if tool_name == "nmap" && guard_name == "scope"
    ));

    // The legacy blocked call replays as a legal tool history (P1-03 fix item 4).
    let replayed = replay_events("legacy-session", &events);
    assert_tool_history_is_legal(&replayed.session.messages);
    let blocked_result = replayed
        .session
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("synthesized blocked result");
    assert!(blocked_result
        .content
        .as_deref()
        .unwrap_or_default()
        .contains("target outside authorized scope"));
}

#[test]
fn legacy_tool_events_without_call_id_still_deserialize() {
    // Pre-P1-03 event payloads carry no call_id; they must stay readable.
    let call: Event = serde_json::from_str(
        r#"{"type":"tool_call","name":"nmap","arguments":{"target":"t"},"purpose":null}"#,
    )
    .expect("legacy tool_call");
    assert!(matches!(call, Event::ToolCall { call_id: None, .. }));

    let result: Event = serde_json::from_str(
        r#"{"type":"tool_result","name":"nmap","success":true,"content":"ok","error":null,"artifacts":[]}"#,
    )
    .expect("legacy tool_result");
    assert!(matches!(
        result,
        Event::ToolResult {
            call_id: None,
            outcome: None,
            ..
        }
    ));

    let blocked: Event = serde_json::from_str(
        r#"{"type":"tool_blocked","tool_name":"nmap","guard_name":"scope","reason":"out of scope"}"#,
    )
    .expect("legacy tool_blocked");
    assert!(matches!(blocked, Event::ToolBlocked { call_id: None, .. }));
}

// P1-08: a large ToolResult is stored in the SQLite blob tables (same
// transaction as the event), NOT in a disk sidecar — the database alone
// restores the full payload.
#[tokio::test]
async fn test_large_tool_result_stored_in_sqlite_blobs() {
    let temp_dir = std::env::temp_dir().join(format!("holmes_test_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("holmes_test.db");
    let db = SessionDB::open(&db_path).await.unwrap();

    let session = db
        .create_session(CreateSessionParams {
            id: Some("test_session_123".into()),
            title: Some("blob offload test".into()),
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();

    let large_content = "A".repeat(15000);
    let event = Event::ToolResult {
        name: "test_tool".into(),
        success: true,
        outcome: Some(holmes_core::ToolOutcomeStatus::Succeeded),
        content: large_content.clone(),
        error: None,
        artifacts: vec![],
        call_id: None,
    };
    db.append_event(&session.id, &event).await.unwrap();

    // No sidecar file is written anymore.
    let sessions_dir = sessions_dir_for(&db_path);
    let session_workspace = sessions_dir.join("test_session_123");
    assert!(
        !session_workspace.join("tool-results").exists(),
        "blob offload must not create disk sidecars"
    );

    // The stored event row carries only the content-addressed marker.
    let stored_data: String = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT event_data FROM events WHERE session_id = 'test_session_123'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(stored_data.contains(holmes_session::blob_store::BLOB_REF_PREFIX));
    assert!(!stored_data.contains(&large_content));

    // The transcript projection carries the marker, not the payload.
    db.projector().flush().await;
    let jsonl_content =
        std::fs::read_to_string(session_workspace.join("transcript.jsonl")).unwrap();
    assert!(jsonl_content.contains(holmes_session::blob_store::BLOB_REF_PREFIX));

    // The read path restores the full payload from SQLite alone.
    let events = db.get_events(&session.id).await.unwrap();
    assert_eq!(events.len(), 1);
    if let Event::ToolResult { content, .. } = &events[0].event {
        assert_eq!(content.as_str(), large_content.as_str());
    } else {
        panic!("Event type mismatch");
    }

    std::fs::remove_dir_all(temp_dir).ok();
}

// P1-08 acceptance: with every file under the sessions directory deleted,
// the SQLite database alone still restores a large tool result in full.
#[tokio::test]
async fn large_tool_result_survives_total_sidecar_loss() {
    let temp_dir = std::env::temp_dir().join(format!("holmes_test_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("holmes_test.db");

    let large_content = "needle-payload-".repeat(2000); // 30 000 chars
    {
        let db = SessionDB::open(&db_path).await.unwrap();
        db.create_session(CreateSessionParams {
            id: Some("sidecar-loss".into()),
            title: None,
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();
        db.append_event(
            "sidecar-loss",
            &Event::ToolResult {
                name: "nmap".into(),
                success: true,
                outcome: Some(holmes_core::ToolOutcomeStatus::Succeeded),
                content: large_content.clone(),
                error: None,
                artifacts: vec![],
                call_id: Some("call-1".into()),
            },
        )
        .await
        .unwrap();
        db.projector().flush().await;
    }

    // Total sidecar loss: every derived file is gone; only holmes.db remains.
    std::fs::remove_dir_all(sessions_dir_for(&db_path)).unwrap();

    let db = SessionDB::open(&db_path).await.unwrap();
    let events = db.get_events("sidecar-loss").await.unwrap();
    assert_eq!(events.len(), 1);
    match &events[0].event {
        Event::ToolResult {
            content, call_id, ..
        } => {
            assert_eq!(content.as_str(), large_content.as_str());
            assert_eq!(call_id.as_deref(), Some("call-1"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    std::fs::remove_dir_all(temp_dir).ok();
}

// P1-08 read-path compatibility: pre-v6 events whose content is a
// `__BYPASS_FILE__:file://` sidecar pointer still read — from the file when it
// exists, degraded to the pointer (never an error, never a panic) when lost.
#[tokio::test]
async fn legacy_bypass_file_pointer_reads_degrade_gracefully() {
    let temp_dir = std::env::temp_dir().join(format!("holmes_test_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("holmes_test.db");

    let sessions_dir = db_path.parent().unwrap().join("sessions");
    let sidecar = sessions_dir.join("legacy-session/tool-results/call_legacy.txt");
    std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
    std::fs::write(&sidecar, "legacy payload from disk").unwrap();

    let db = SessionDB::open(&db_path).await.unwrap();
    db.create_session(CreateSessionParams {
        id: Some("legacy-session".into()),
        title: None,
        mode: Some(SessionMode::Pentest),
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    })
    .await
    .unwrap();

    // Insert a legacy pointer row directly (append_event never writes these).
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
             VALUES ('legacy-session', 0, 'tool_result', ?1, datetime('now'))",
            rusqlite::params![format!(
                "{{\"type\":\"tool_result\",\"name\":\"nmap\",\"success\":true,\"content\":\"__BYPASS_FILE__:file://{}\",\"error\":null,\"artifacts\":[]}}",
                sidecar.to_string_lossy()
            )],
        )
        .unwrap();
    }

    // Sidecar present: content is restored from the file.
    let events = db.get_events("legacy-session").await.unwrap();
    match &events[0].event {
        Event::ToolResult { content, .. } => {
            assert_eq!(content, "legacy payload from disk")
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // Sidecar lost: read degrades to the pointer instead of failing.
    std::fs::remove_file(&sidecar).unwrap();
    let events = db.get_events("legacy-session").await.unwrap();
    match &events[0].event {
        Event::ToolResult { content, .. } => {
            assert!(content.starts_with("__BYPASS_FILE__:file://"))
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    std::fs::remove_dir_all(temp_dir).ok();
}

// P1-04: a fork with startup semantics commits copied events, the startup
// batch and the branch summary in ONE transaction — the child replays as
// semantically complete, and a conflicting fork leaves nothing behind.
#[tokio::test]
async fn fork_session_with_events_commits_startup_semantics_atomically() {
    use holmes_session::{BranchSummarySpec, ForkStartupSpec};

    let db = SessionDB::open(":memory:").await.unwrap();
    let parent = db
        .create_session_with_events(
            CreateSessionParams {
                id: Some("parent".into()),
                title: Some("parent".into()),
                mode: Some(SessionMode::Pentest),
                model: Some("parent-model".into()),
                system_prompt: Some("parent prompt".into()),
                parent_session_id: None,
                fork_point: None,
                source: Some("test".into()),
                tags: vec![],
            },
            vec![Event::UserMessage {
                content: "hello".into(),
                timestamp: chrono::Utc::now(),
            }],
        )
        .await
        .unwrap();
    let parent_events = db.get_events(&parent.id).await.unwrap();
    let fork_point = parent_events.last().unwrap().event_index;

    let spec = ForkStartupSpec {
        new_session_id: "child-atomic".into(),
        model: Some("child-model".into()),
        provider: None,
        fallback_system_prompt: "fallback prompt".into(),
        active_tool_names: vec!["shell".into()],
        branch_summary: Some(BranchSummarySpec {
            from_event_index: 0,
            to_event_index: fork_point,
            summary: "branch summary text".into(),
            reason: "branch".into(),
        }),
    };
    let child = db
        .fork_session_with_events(&parent.id, fork_point, "child", spec)
        .await
        .unwrap();
    assert_eq!(child.id, "child-atomic");
    assert_eq!(child.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(child.fork_point, Some(fork_point));

    let replayed = db.replay_session_context(&child.id).await.unwrap();
    assert!(
        replayed.semantic_complete,
        "forked child must replay with complete startup semantics"
    );
    assert_eq!(replayed.system_prompt.as_deref(), Some("parent prompt"));
    assert_eq!(
        replayed.session.lineage.parent_id.as_deref(),
        Some("parent")
    );
    assert_eq!(replayed.active_tools, vec!["shell".to_string()]);
    assert!(replayed
        .branch_summaries
        .iter()
        .any(|s| s == "branch summary text"));
    let contents: Vec<&str> = replayed
        .session
        .messages
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect();
    assert!(contents.contains(&"hello"), "copied parent events replay");

    // The startup batch comes after every retained parent event index.
    let child_events = db.get_events(&child.id).await.unwrap();
    let event_count = child_events.len();
    let branch_index = child_events
        .iter()
        .find(|stored| matches!(&stored.event, Event::BranchSummary { .. }))
        .map(|stored| stored.event_index)
        .expect("branch summary recorded");
    assert!(branch_index > fork_point);

    // A conflicting fork (same caller-generated child id) fails as a whole:
    // no partial session row, no stray extra events.
    let conflict = db
        .fork_session_with_events(
            &parent.id,
            fork_point,
            "child-again",
            ForkStartupSpec {
                new_session_id: "child-atomic".into(),
                model: None,
                provider: None,
                fallback_system_prompt: "fallback prompt".into(),
                active_tool_names: vec![],
                branch_summary: None,
            },
        )
        .await;
    assert!(conflict.is_err());
    let child_events_after = db.get_events(&child.id).await.unwrap();
    assert_eq!(
        child_events_after.len(),
        event_count,
        "failed fork leaves the child's event log untouched"
    );
    let record = db.get_session(&child.id).await.unwrap().unwrap();
    assert_eq!(record.title.as_deref(), Some("child"));
}
