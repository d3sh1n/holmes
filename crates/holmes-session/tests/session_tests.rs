use holmes_core::event::Event;
use holmes_core::types::*;
use holmes_session::db::*;
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
