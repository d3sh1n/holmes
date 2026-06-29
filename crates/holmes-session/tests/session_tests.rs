use holmes_core::event::{Event, StoredEvent};
use holmes_core::types::*;
use holmes_core::{CompactionTrigger, CompressionMethod, Role, SummaryMethod};
use holmes_session::db::*;
use holmes_session::replay_events;
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
    let expected_path = temp_dir
        .path()
        .join("sessions")
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
    assert!(contents.iter().any(|content| *content == "C"));
    assert!(!contents.iter().any(|content| *content == "A"));
    assert!(!contents.iter().any(|content| *content == "B"));
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
                },
            ),
            stored_event(
                1,
                Event::ToolResult {
                    name: "http_request".into(),
                    success: true,
                    content: "ok".into(),
                    error: None,
                    artifacts: vec![],
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
                content: "orphan output".into(),
                error: Some("failed".into()),
                artifacts: vec![],
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
