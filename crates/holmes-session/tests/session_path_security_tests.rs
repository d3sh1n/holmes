//! P1-12 / P1-13 acceptance tests: path-unsafe session ids are rejected with
//! zero database and zero filesystem side effects, and two databases in one
//! directory are fully isolated projection namespaces.

use holmes_core::event::Event;
use holmes_core::types::SessionMode;
use holmes_session::db::{sessions_dir_for, CreateSessionParams, SessionDB};
use holmes_session::store::ForkStartupSpec;
use holmes_session::SessionStore;

fn params(id: &str) -> CreateSessionParams {
    CreateSessionParams {
        id: Some(id.into()),
        title: None,
        mode: Some(SessionMode::Pentest),
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    }
}

fn user_message(text: &str) -> Event {
    Event::UserMessage {
        content: text.into(),
        timestamp: chrono::Utc::now(),
    }
}

/// P1-12: every path-unsafe id shape is refused at session creation; neither
/// the database nor the filesystem shows any trace of the attempt.
#[tokio::test]
async fn unsafe_session_ids_are_rejected_with_zero_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();
    let sessions_dir = sessions_dir_for(&db_path);
    let outside = dir.path().join("outside");
    let parent_listing_before: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    for bad in [
        "../../outside",
        "../outside",
        "a/b",
        "/tmp/abs",
        "..",
        ".",
        "",
        "trailing/",
        "sub/../../outside",
    ] {
        let err = db
            .create_session(params(bad))
            .await
            .expect_err(&format!("unsafe id '{bad}' must be rejected"));
        assert!(
            err.to_string().contains("invalid session id"),
            "id '{bad}': unexpected error {err}"
        );
        assert!(
            db.get_session(bad).await.unwrap().is_none(),
            "id '{bad}' must not exist in the database"
        );
    }

    assert!(
        !outside.exists(),
        "no directory may escape the sessions dir"
    );
    let parent_listing_after: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        parent_listing_before, parent_listing_after,
        "rejected creates must leave the parent directory untouched"
    );
    // The sessions dir itself only holds what the open created (nothing
    // per-session).
    let session_entries: Vec<_> = std::fs::read_dir(&sessions_dir)
        .map(|rd| rd.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(
        session_entries.is_empty(),
        "no per-session directories may be created: {session_entries:?}"
    );
}

/// P1-12: a caller-supplied fork id follows the same rule — rejected before
/// any row or directory appears.
#[tokio::test]
async fn unsafe_fork_id_is_rejected_with_zero_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();
    let parent = db.create_session(params("parent")).await.unwrap();
    db.append_event("parent", &user_message("hello"))
        .await
        .unwrap();

    let spec = ForkStartupSpec {
        new_session_id: "../../evil".into(),
        model: None,
        provider: None,
        fallback_system_prompt: "fallback".into(),
        active_tool_names: vec![],
        branch_summary: None,
    };
    let err = db
        .fork_session_with_events(&parent.id, 0, "fork", spec)
        .await
        .expect_err("unsafe fork id must be rejected");
    assert!(
        err.to_string().contains("invalid session id"),
        "unexpected error {err}"
    );
    assert!(db.get_session("../../evil").await.unwrap().is_none());
    assert!(
        !dir.path().join("evil").exists() && !dir.path().join("outside").exists(),
        "fork must not create directories outside the sessions dir"
    );
}

/// P1-13: the default database keeps the historical `<parent>/sessions`
/// layout; any other database file in the same directory gets its own
/// hash-namespaced directory.
#[tokio::test]
async fn sessions_dir_layout_default_vs_namespaced() {
    let dir = tempfile::tempdir().unwrap();
    // The layout helper works on canonical paths; macOS tempdirs live behind
    // the /var → /private/var symlink, so canonicalize the expectation too.
    let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
    let default_db = dir.path().join("holmes.db");
    let other_db = dir.path().join("other.db");

    assert_eq!(
        sessions_dir_for(&default_db),
        canonical_dir.join("sessions")
    );
    let namespaced = sessions_dir_for(&other_db);
    assert_ne!(namespaced, canonical_dir.join("sessions"));
    assert_eq!(namespaced.parent(), Some(canonical_dir.as_path()));
    assert!(
        namespaced
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("sessions-"),
        "non-default database gets a hash namespace: {}",
        namespaced.display()
    );
    // Deterministic: the same file always maps to the same namespace, and it
    // survives reopening.
    assert_eq!(sessions_dir_for(&other_db), namespaced);
}

/// P1-13 acceptance: two databases in one directory, same session id in both,
/// concurrent appends, a rebuild on one side and a reopen on the other —
/// transcripts and projection state stay isolated throughout.
#[tokio::test]
async fn two_databases_in_one_directory_are_fully_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let db_a_path = dir.path().join("a.db");
    let db_b_path = dir.path().join("b.db");

    let db_a = SessionDB::open(&db_a_path).await.unwrap();
    let db_b = SessionDB::open(&db_b_path).await.unwrap();
    let dir_a = sessions_dir_for(&db_a_path);
    let dir_b = sessions_dir_for(&db_b_path);
    assert_ne!(dir_a, dir_b, "each database has its own namespace");

    db_a.create_session(params("shared")).await.unwrap();
    db_b.create_session(params("shared")).await.unwrap();

    // Concurrent appends to the same session id in both databases.
    let event_a = user_message("from-a");
    let event_b = user_message("from-b");
    let (ra, rb) = tokio::join!(
        db_a.append_event("shared", &event_a),
        db_b.append_event("shared", &event_b),
    );
    ra.unwrap();
    rb.unwrap();
    db_a.projector().flush().await;
    db_b.projector().flush().await;

    let transcript_a = dir_a.join("shared/transcript.jsonl");
    let transcript_b = dir_b.join("shared/transcript.jsonl");
    let content_a = std::fs::read_to_string(&transcript_a).unwrap();
    let content_b = std::fs::read_to_string(&transcript_b).unwrap();
    assert!(content_a.contains("from-a"), "a: {content_a}");
    assert!(!content_a.contains("from-b"), "a polluted: {content_a}");
    assert!(content_b.contains("from-b"), "b: {content_b}");
    assert!(!content_b.contains("from-a"), "b polluted: {content_b}");

    // A worker-run rebuild on one side never touches the other side's files.
    let before_b = std::fs::read_to_string(&transcript_b).unwrap();
    db_a.rebuild_transcript("shared").await.unwrap();
    assert_eq!(std::fs::read_to_string(&transcript_b).unwrap(), before_b);

    // Reopening B reconciles against B's own database only: same session id,
    // no cross-contamination of either transcript or projection offsets.
    drop(db_b);
    let db_b = SessionDB::open(&db_b_path).await.unwrap();
    db_b.projector().flush().await;
    assert_eq!(
        std::fs::read_to_string(&transcript_b).unwrap(),
        before_b,
        "reopen must not rewrite B's transcript from A's events"
    );
    assert_eq!(std::fs::read_to_string(&transcript_a).unwrap(), content_a);
}
