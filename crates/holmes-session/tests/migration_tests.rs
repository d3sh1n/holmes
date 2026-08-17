//! P1-04: schema migrations run inside a single `BEGIN IMMEDIATE`
//! transaction, so concurrent openers of an unmigrated database serialize on
//! the write lock instead of racing the DDL.

use holmes_session::{schema, SessionDB, SessionStore};
use std::sync::{Arc, Barrier};

/// Several connections (same file-locking semantics as separate processes —
/// SQLite locks are per database file) open a never-migrated database at the
/// same time. Exactly one runs the migration transaction; the rest block on
/// the busy timeout and then observe the fully migrated schema. Nobody fails,
/// nobody re-applies DDL.
#[test]
fn concurrent_open_of_unmigrated_database_migrates_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    // Create the database file without any schema: version 0, every migration pending.
    rusqlite::Connection::open(&db_path).unwrap();

    const OPENERS: usize = 4;
    let barrier = Arc::new(Barrier::new(OPENERS));
    let mut handles = Vec::new();
    for i in 0..OPENERS {
        let path = db_path.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            // Real threads with separate connections: the opens genuinely race.
            barrier.wait();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let db = SessionDB::open(&path).await?;
                // Prove the migrated schema works from every opener's handle.
                db.create_session(holmes_session::db::CreateSessionParams {
                    id: Some(format!("opener-{i}")),
                    title: None,
                    mode: None,
                    model: None,
                    system_prompt: None,
                    parent_session_id: None,
                    fork_point: None,
                    source: Some("test".into()),
                    tags: vec![],
                })
                .await?;
                Ok::<_, holmes_session::SessionError>(db)
            })
        }));
    }

    for handle in handles {
        handle
            .join()
            .unwrap()
            .expect("every concurrent opener must succeed");
    }

    // The final schema state: all migrations applied exactly once.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let version: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, schema::SCHEMA_VERSION);
    let applied: u32 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        applied,
        schema::SCHEMA_VERSION,
        "each migration recorded exactly once"
    );
    // Spot-check DDL from the later migrations landed (tasks table v2,
    // memories.source column v3, tasks.payload column v4).
    let has_tasks: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'tasks'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(has_tasks, "tasks table exists");
    let mut stmt = conn.prepare("PRAGMA table_info(memories)").unwrap();
    let columns: Vec<String> = stmt
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(columns.iter().any(|c| c == "source"));
    let mut stmt = conn.prepare("PRAGMA table_info(tasks)").unwrap();
    let columns: Vec<String> = stmt
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(columns.iter().any(|c| c == "payload"));
}

/// Re-opening an already migrated database applies nothing and never errors
/// (idempotent second open behind the same immediate-transaction path).
#[tokio::test]
async fn reopening_migrated_database_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let db1 = SessionDB::open(&db_path).await.unwrap();
    db1.create_session(holmes_session::db::CreateSessionParams {
        id: Some("kept".into()),
        title: None,
        mode: None,
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    })
    .await
    .unwrap();
    drop(db1);

    let db2 = SessionDB::open(&db_path).await.unwrap();
    let session = db2.get_session("kept").await.unwrap();
    assert!(session.is_some(), "data survives a reopen");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let applied: u32 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(applied, schema::SCHEMA_VERSION);
}
