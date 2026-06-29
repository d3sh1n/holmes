use holmes_core::event::Event;
use holmes_core::types::*;
use holmes_session::db::*;

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
    let temp_dir = std::env::temp_dir().join(format!("holmes_archive_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("holmes.db");
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
        session_id: session.id.clone(),
        compaction_event_index: 7,
        trigger: holmes_core::CompactionTrigger::Manual,
        archived_event_range: Some((0, 0)),
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
    assert!(path.ends_with("sessions/archive_session/compactions/compaction_7.json"));

    let loaded = db.read_compaction_archive(&path).await.unwrap();
    assert_eq!(loaded.session_id, session.id);
    assert_eq!(loaded.compaction_event_index, 7);
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.events.len(), 1);

    std::fs::remove_dir_all(temp_dir).ok();
}
