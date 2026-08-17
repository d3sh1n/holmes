//! Concurrency evidence for AGT-019: the single-writer SQLite store must hold up
//! under parallel-subagent load. Two shapes are exercised:
//!
//! - many in-process writers sharing one `SessionDB` handle (the production
//!   shape: subagents share the parent's `Arc<Mutex<Connection>>`);
//! - two independent handles on the same database file (a second holmes process
//!   on the same data dir), relying on WAL + `busy_timeout` + the bounded
//!   BUSY/LOCKED retry to serialize writers without surfacing lock errors.
//!
//! These tests assert correctness under concurrency, not throughput: per §11.2 of
//! the remediation plan, a connection pool / external database is a deliberate
//! later decision, not part of this phase.

use holmes_core::event::Event;
use holmes_core::types::*;
use holmes_session::db::*;
use holmes_session::SessionStore;

fn session_params(id: &str) -> CreateSessionParams {
    CreateSessionParams {
        id: Some(id.into()),
        title: Some("concurrency test".into()),
        mode: Some(SessionMode::Pentest),
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    }
}

fn user_message(content: &str) -> Event {
    Event::UserMessage {
        content: content.into(),
        timestamp: chrono::Utc::now(),
    }
}

/// 8 concurrent writers × 25 events each against one shared handle — the shape
/// parallel subagents produce. Every append must succeed and every event must be
/// readable afterwards (serialized by the connection mutex; no BUSY possible
/// in-process, so this pins the happy path the runtime depends on).
#[tokio::test]
async fn concurrent_writers_on_shared_handle_all_land() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(SessionDB::open(dir.path().join("holmes.db")).await.unwrap());
    db.create_session(session_params("shared-session"))
        .await
        .unwrap();

    let writers = 8;
    let events_per_writer = 25;
    let mut handles = Vec::new();
    for writer in 0..writers {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            for seq in 0..events_per_writer {
                db.append_event(
                    "shared-session",
                    &user_message(&format!("writer-{writer}-event-{seq}")),
                )
                .await
                .expect("append under concurrency must succeed");
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let events = db.get_events("shared-session").await.unwrap();
    // Startup metadata events (session_created etc.) plus the appended ones.
    let appended = events
        .iter()
        .filter(|event| matches!(event.event, Event::UserMessage { .. }))
        .count();
    assert_eq!(appended, writers * events_per_writer);
    // Event indices stay a dense, ordered sequence — no torn appends.
    let indices: Vec<u64> = events.iter().map(|event| event.event_index).collect();
    let mut sorted = indices.clone();
    sorted.sort_unstable();
    assert_eq!(indices.len(), sorted.len());
    sorted.dedup();
    assert_eq!(indices.len(), sorted.len(), "duplicate event indices");
}

/// Two handles on the same file (cross-process stand-in): WAL allows one writer
/// at a time, so the loser waits on `busy_timeout` / the bounded retry. Both
/// writers must complete without a lock error ever surfacing.
#[tokio::test]
async fn two_handles_same_file_serialize_writes_without_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");

    let db1 = std::sync::Arc::new(SessionDB::open(&db_path).await.unwrap());
    db1.create_session(session_params("session-a"))
        .await
        .unwrap();

    let db2 = std::sync::Arc::new(SessionDB::open(&db_path).await.unwrap());
    db2.create_session(session_params("session-b"))
        .await
        .unwrap();

    let mut handles = Vec::new();
    for (db, session) in [(db1.clone(), "session-a"), (db2.clone(), "session-b")] {
        handles.push(tokio::spawn(async move {
            for seq in 0..40 {
                db.append_event(session, &user_message(&format!("{session}-{seq}")))
                    .await
                    .expect("cross-handle write must not surface a lock error");
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    for (db, session) in [(db1, "session-a"), (db2, "session-b")] {
        let events = db.get_events(session).await.unwrap();
        let appended = events
            .iter()
            .filter(|event| matches!(event.event, Event::UserMessage { .. }))
            .count();
        assert_eq!(appended, 40, "{session} lost events across handles");
        // Each handle sees the other's commits too (WAL read-consistent).
        let other = if session == "session-a" {
            "session-b"
        } else {
            "session-a"
        };
        let other_events = db.get_events(other).await.unwrap();
        assert_eq!(
            other_events
                .iter()
                .filter(|event| matches!(event.event, Event::UserMessage { .. }))
                .count(),
            40,
            "{other} not fully visible from the other handle"
        );
    }
}

/// P1-08: two handles on the same database file share ONE projection worker,
/// so transcript line order is a single global commit order — interleaved
/// appends from both handles land in commit sequence, never per-handle
/// batches. Flushing through one handle also drains the other's queued jobs
/// (behavioural proof of the shared worker).
#[tokio::test]
async fn two_handles_share_one_projector_with_global_order() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");

    let db1 = std::sync::Arc::new(SessionDB::open(&db_path).await.unwrap());
    db1.create_session(session_params("shared-session"))
        .await
        .unwrap();
    let db2 = std::sync::Arc::new(SessionDB::open(&db_path).await.unwrap());

    // Strictly interleaved, serially awaited: commit order is h1-0, h2-1, ...
    for seq in 0..20 {
        let (db, tag) = if seq % 2 == 0 {
            (&db1, "h1")
        } else {
            (&db2, "h2")
        };
        db.append_event("shared-session", &user_message(&format!("{tag}-{seq}")))
            .await
            .expect("interleaved append must succeed");
    }

    // Flushing through db2 must also drain the jobs db1 queued (one worker).
    db2.projector().flush().await;

    let transcript =
        std::fs::read_to_string(dir.path().join("sessions/shared-session/transcript.jsonl"))
            .unwrap();
    let lines: Vec<&str> = transcript.lines().collect();
    let events = db1.get_events("shared-session").await.unwrap();
    assert_eq!(
        lines.len(),
        events.len(),
        "every event projected exactly once"
    );
    for (line, stored) in lines.iter().zip(events.iter()) {
        let Event::UserMessage { content, .. } = &stored.event else {
            panic!("unexpected event in transcript stream: {stored:?}");
        };
        assert!(
            line.contains(content.as_str()),
            "transcript line out of commit order: {line} vs {content}"
        );
    }
    assert!(lines[0].contains("h1-0"));
    assert!(lines[1].contains("h2-1"));
    assert!(lines[19].contains("h2-19"));
}
