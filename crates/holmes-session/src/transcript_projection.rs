//! Transcript JSONL projection (AGT-008, hardened in P1-08).
//!
//! The SQLite `events` table is the authoritative record of a session; the
//! per-session `transcript.jsonl` file is a *derived projection* of it, never
//! a second source of truth. Lines are appended only AFTER the owning event
//! transaction has committed, serialised through a single worker so line order
//! matches commit order. A projection failure cannot fail or roll back an
//! already-committed event: the failed projection is queued for rebuild and
//! the open-time reconcile (see `SessionDB::open`) regenerates the file from
//! the event store.
//!
//! P1-08 hardening:
//! - the job channel is BOUNDED (`DEFAULT_QUEUE_CAPACITY`): `project` awaits
//!   channel capacity, so a stalled worker applies backpressure to the commit
//!   path instead of growing memory without bound;
//! - one projector per database identity: every `SessionDB` handle on the same
//!   file shares a single worker via [`TranscriptProjector::for_database`], so
//!   projection order is a single global commit order, not per-handle; the
//!   registry key is the canonical database path (P1-13), never a shared
//!   parent directory;
//! - the per-session projected high watermark is persisted in the
//!   `projection_state` table, and whole-file rebuilds run as jobs INSIDE the
//!   worker (`ProjectionJob::Rebuild`), so a rebuild can never interleave with
//!   queued appends: appends at or below the rebuilt watermark are skipped.
//!
//! P1-12 hardening: every entry point that turns a session id into a
//! filesystem path re-validates it (`is_path_safe_session_id`), so a
//! path-unsafe id can never escape the sessions directory even if a caller
//! forgets to check.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Bounded projection queue depth. At ~1 queued line per committed event this
/// absorbs long tool bursts while capping worst-case buffered payload memory.
const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// A projection that failed after its event had already committed. Rebuilding
/// the session's transcript from the event store heals it. Kept in memory for
/// observability/tests only — durability comes from `projection_state`: the
/// offset of a session with a failed line never covers the gap, and the
/// open-time reconcile re-derives every missing tail from the events table.
#[derive(Debug, Clone)]
pub struct PendingRebuild {
    pub session_id: String,
    pub event_index: u64,
    pub error: String,
}

enum ProjectionJob {
    Append {
        session_id: String,
        event_index: u64,
        line: String,
    },
    /// Rewrite the session's whole transcript from the events table. Runs
    /// inside the worker so it is serialised against queued appends; the ack
    /// resolves with the number of lines written.
    Rebuild {
        session_id: String,
        ack: tokio::sync::oneshot::Sender<Result<usize, String>>,
    },
    /// Barrier: resolves once every job queued before it has been applied.
    /// Used by tests and as the graceful-shutdown flush point.
    Flush(tokio::sync::oneshot::Sender<()>),
}

struct Shared {
    sessions_dir: PathBuf,
    tx: tokio::sync::mpsc::Sender<ProjectionJob>,
    rebuild_queue: Mutex<Vec<PendingRebuild>>,
}

/// Asynchronous appender for `transcript.jsonl`. Cheap to clone; all clones
/// feed the same worker, so appends stay ordered across concurrent turns.
#[derive(Clone)]
pub struct TranscriptProjector {
    shared: Arc<Shared>,
}

/// Registry of live projectors keyed by canonical DATABASE identity (P1-13):
/// every `SessionDB` handle opened on the same database file shares one
/// worker (P1-08), which is what gives a database-path-level global
/// projection order. Two different database files in the same directory have
/// different identities AND different sessions directories, so they can never
/// share a worker, a connection, projection offsets or transcript files.
fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<Shared>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<Shared>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Defense in depth (P1-12): entry points (`SessionDB::create_*`, fork)
/// reject path-unsafe session ids, but the projector is the LAST code that
/// turns an id into a filesystem path, so it re-validates before every join
/// instead of trusting callers.
fn is_path_safe_session_id(session_id: &str) -> bool {
    crate::db::validate_session_id_for_path(session_id).is_ok()
}

impl TranscriptProjector {
    /// Standalone projector with no database connection: offsets are not
    /// persisted and worker-run rebuilds fail. Intended for unit tests;
    /// production goes through `for_database`.
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self::spawn(sessions_dir, None, DEFAULT_QUEUE_CAPACITY, None)
    }

    /// The projector for a database, creating it on first use and sharing it
    /// across every `SessionDB` handle on the same file. `db_identity` is the
    /// canonical database path and is the registry key (P1-13); the worker
    /// writes under `sessions_dir` and persists projection offsets through
    /// `conn`.
    pub fn for_database(
        db_identity: &Path,
        sessions_dir: &Path,
        conn: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    ) -> Self {
        let key = std::fs::canonicalize(db_identity).unwrap_or_else(|_| db_identity.to_path_buf());
        let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
        registry.retain(|_, weak| weak.strong_count() > 0);
        if let Some(shared) = registry.get(&key).and_then(Weak::upgrade) {
            return Self { shared };
        }
        let sessions_dir =
            std::fs::canonicalize(sessions_dir).unwrap_or_else(|_| sessions_dir.to_path_buf());
        let projector = Self::spawn(sessions_dir, Some(conn), DEFAULT_QUEUE_CAPACITY, None);
        registry.insert(key, Arc::downgrade(&projector.shared));
        projector
    }

    fn spawn(
        sessions_dir: PathBuf,
        conn: Option<Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
        capacity: usize,
        // Test hook (always `None` in production constructors): while held,
        // the worker blocks before each job so tests can fill the bounded
        // queue and observe backpressure deterministically.
        pause_gate: Option<Arc<tokio::sync::Mutex<()>>>,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel::<ProjectionJob>(capacity);
        let shared = Arc::new(Shared {
            sessions_dir,
            tx,
            rebuild_queue: Mutex::new(Vec::new()),
        });
        tokio::spawn(run_worker(rx, Arc::clone(&shared), conn, pause_gate));
        Self { shared }
    }

    /// Queue one committed event for projection, awaiting channel capacity:
    /// a full queue applies backpressure to the caller instead of buffering
    /// without bound (P1-08). Never fails the caller on filesystem I/O —
    /// projection errors surface via the rebuild queue, keeping the database
    /// fact untouched. If the worker is gone the store is shutting down; the
    /// open-time reconcile regenerates the file, so dropping the job is safe.
    pub async fn project(&self, session_id: &str, event_index: u64, event_data: &str) {
        if !is_path_safe_session_id(session_id) {
            // Refuse to turn an unsafe id into a path (P1-12). The event is
            // already committed; the open-time reconcile (which re-validates
            // and skips) is the only handler for such rows.
            tracing::warn!(
                session_id = %session_id,
                event_index,
                "refusing to project path-unsafe session id"
            );
            return;
        }
        let job = ProjectionJob::Append {
            session_id: session_id.to_string(),
            event_index,
            line: format!("{event_data}\n"),
        };
        if self.shared.tx.send(job).await.is_err() {
            tracing::warn!(
                session_id = %session_id,
                event_index,
                "transcript projector shut down; projection deferred to open-time reconcile"
            );
        }
    }

    /// Rewrite the session's transcript from the events table as a worker job,
    /// serialised against queued appends. Resolves with the lines written.
    pub async fn rebuild_via_worker(&self, session_id: &str) -> Result<usize, String> {
        if !is_path_safe_session_id(session_id) {
            return Err(format!(
                "refusing to rebuild transcript for path-unsafe session id '{session_id}'"
            ));
        }
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if self
            .shared
            .tx
            .send(ProjectionJob::Rebuild {
                session_id: session_id.to_string(),
                ack: ack_tx,
            })
            .await
            .is_err()
        {
            return Err("projector worker shut down".to_string());
        }
        ack_rx
            .await
            .unwrap_or_else(|_| Err("projector worker dropped rebuild".to_string()))
    }

    /// Wait until every projection queued so far has been applied (or failed
    /// into the rebuild queue). This is the graceful-shutdown barrier: calling
    /// it before exit drains the bounded queue. Tests use it for determinism.
    pub async fn flush(&self) {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if self
            .shared
            .tx
            .send(ProjectionJob::Flush(ack_tx))
            .await
            .is_ok()
        {
            let _ = ack_rx.await;
        }
    }

    /// Drain the queue of projections that failed after commit.
    pub fn take_rebuild_queue(&self) -> Vec<PendingRebuild> {
        std::mem::take(
            &mut *self
                .shared
                .rebuild_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Rewrite `transcript.jsonl` for a session from already-serialised event
    /// payloads (read from the authoritative `events` table by the caller).
    /// Atomic via write-temp-then-rename: a crash mid-rebuild never leaves a
    /// half-written transcript. Returns the number of lines written.
    pub fn rebuild_from_events(
        &self,
        session_id: &str,
        event_data_lines: &[String],
    ) -> std::io::Result<usize> {
        rebuild_file(&self.shared.sessions_dir, session_id, event_data_lines)
    }
}

/// The single projection worker loop. `rebuilt_through` records, per session,
/// the highest event index covered by a worker-run rebuild: appends at or
/// below it are skipped because the rebuild already wrote their lines.
async fn run_worker(
    mut rx: tokio::sync::mpsc::Receiver<ProjectionJob>,
    shared: Arc<Shared>,
    conn: Option<Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    pause_gate: Option<Arc<tokio::sync::Mutex<()>>>,
) {
    let mut rebuilt_through: HashMap<String, u64> = HashMap::new();
    while let Some(job) = rx.recv().await {
        let _pause = match &pause_gate {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        match job {
            ProjectionJob::Append {
                session_id,
                event_index,
                line,
            } => {
                let covered = rebuilt_through
                    .get(&session_id)
                    .is_some_and(|through| event_index <= *through);
                if covered {
                    continue;
                }
                match append_line(&shared.sessions_dir, &session_id, &line) {
                    Ok(()) => {
                        if let Some(conn) = &conn {
                            persist_offset(conn, &session_id, event_index).await;
                        }
                    }
                    Err(error) => {
                        tracing::error!(
                            session_id = %session_id,
                            event_index,
                            error = %error,
                            "transcript projection failed after commit; queued for rebuild"
                        );
                        shared
                            .rebuild_queue
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(PendingRebuild {
                                session_id,
                                event_index,
                                error,
                            });
                    }
                }
            }
            ProjectionJob::Rebuild { session_id, ack } => {
                let _ = ack.send(
                    match rebuild_session(&shared, conn.as_ref(), &session_id).await {
                        Ok((lines, max_index)) => {
                            if let Some(max_index) = max_index {
                                rebuilt_through.insert(session_id.clone(), max_index);
                                if let Some(conn) = &conn {
                                    persist_offset(conn, &session_id, max_index).await;
                                }
                            }
                            Ok(lines)
                        }
                        Err(error) => Err(error),
                    },
                );
            }
            ProjectionJob::Flush(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

/// Read every committed event payload for a session and rewrite its
/// transcript. Returns (lines written, highest event index covered).
async fn rebuild_session(
    shared: &Shared,
    conn: Option<&Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    session_id: &str,
) -> Result<(usize, Option<u64>), String> {
    let Some(conn) = conn else {
        return Err("projector has no database connection".to_string());
    };
    let rows: Vec<(i64, String)> = {
        let conn = conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT event_index, event_data FROM events
                 WHERE session_id = ?1 ORDER BY event_index",
            )
            .map_err(|e| e.to_string())?;
        let collected: Result<Vec<(i64, String)>, _> = stmt
            .query_map(rusqlite::params![session_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|e| e.to_string())?
            .collect();
        collected.map_err(|e| e.to_string())?
    };
    let max_index = rows.iter().map(|(index, _)| *index as u64).max();
    let payloads: Vec<String> = rows.into_iter().map(|(_, data)| data).collect();
    let written = rebuild_file(&shared.sessions_dir, session_id, &payloads)
        .map_err(|e| format!("rebuild transcript for {session_id}: {e}"))?;
    Ok((written, max_index))
}

/// Persist the per-session projection high watermark (monotonic). Failures are
/// logged, not fatal: a stale offset only makes the next open-time reconcile
/// rebuild the tail, which is idempotent.
async fn persist_offset(
    conn: &Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    session_id: &str,
    event_index: u64,
) {
    let conn = conn.lock().await;
    let result = conn.execute(
        "INSERT INTO projection_state (session_id, projected_event_index)
         VALUES (?1, ?2)
         ON CONFLICT(session_id) DO UPDATE SET
             projected_event_index = MAX(projected_event_index, excluded.projected_event_index),
             updated_at = datetime('now')",
        rusqlite::params![session_id, event_index as i64],
    );
    if let Err(error) = result {
        tracing::warn!(
            session_id = %session_id,
            event_index,
            error = %error,
            "failed to persist projection offset; open-time reconcile will cover the gap"
        );
    }
}

fn rebuild_file(
    sessions_dir: &Path,
    session_id: &str,
    event_data_lines: &[String],
) -> std::io::Result<usize> {
    if !is_path_safe_session_id(session_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path-unsafe session id '{session_id}'"),
        ));
    }
    let session_dir = sessions_dir.join(session_id);
    std::fs::create_dir_all(&session_dir)?;
    let final_path = session_dir.join("transcript.jsonl");
    let tmp_path = session_dir.join("transcript.jsonl.rebuild.tmp");
    let mut body = String::new();
    for line in event_data_lines {
        body.push_str(line);
        body.push('\n');
    }
    std::fs::write(&tmp_path, body)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(event_data_lines.len())
}

fn append_line(sessions_dir: &Path, session_id: &str, line: &str) -> Result<(), String> {
    use std::io::Write;
    if !is_path_safe_session_id(session_id) {
        return Err(format!("path-unsafe session id '{session_id}'"));
    }
    let session_dir = sessions_dir.join(session_id);
    std::fs::create_dir_all(&session_dir).map_err(|e| e.to_string())?;
    let jsonl_path = session_dir.join("transcript.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&jsonl_path)
        .map_err(|e| e.to_string())?;
    file.write_all(line.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn projection_appends_in_commit_order() {
        let dir = tempfile::tempdir().unwrap();
        let projector = TranscriptProjector::new(dir.path().to_path_buf());
        for i in 0..5 {
            projector.project("s1", i, &format!("{{\"n\":{i}}}")).await;
        }
        projector.flush().await;
        let content = std::fs::read_to_string(dir.path().join("s1/transcript.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[3], "{\"n\":3}");
        assert!(projector.take_rebuild_queue().is_empty());
    }

    #[tokio::test]
    async fn projection_failure_lands_in_rebuild_queue() {
        let dir = tempfile::tempdir().unwrap();
        // Block projection: a regular FILE named after the session makes both
        // create_dir_all and the open underneath fail.
        std::fs::write(dir.path().join("s-blocked"), "not a dir").unwrap();
        let projector = TranscriptProjector::new(dir.path().to_path_buf());
        projector.project("s-blocked", 0, "{}").await;
        projector.flush().await;
        let queue = projector.take_rebuild_queue();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].session_id, "s-blocked");
        assert_eq!(queue[0].event_index, 0);
        // Queue is drained by the take.
        assert!(projector.take_rebuild_queue().is_empty());
    }

    #[tokio::test]
    async fn rebuild_rewrites_file_atomically_from_event_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let projector = TranscriptProjector::new(dir.path().to_path_buf());
        let payloads = vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()];
        let written = projector.rebuild_from_events("s2", &payloads).unwrap();
        assert_eq!(written, 2);
        let content = std::fs::read_to_string(dir.path().join("s2/transcript.jsonl")).unwrap();
        assert_eq!(content, "{\"a\":1}\n{\"b\":2}\n");
        assert!(!dir.path().join("s2/transcript.jsonl.rebuild.tmp").exists());
    }

    /// P1-08: the bounded queue applies backpressure — with the worker paused
    /// and the channel full, `project` pends instead of buffering; once the
    /// worker resumes every queued line lands exactly once (no loss, no OOM).
    #[tokio::test]
    async fn full_queue_applies_backpressure_without_losing_events() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let projector =
            TranscriptProjector::spawn(dir.path().to_path_buf(), None, 1, Some(gate.clone()));

        // Hold the gate: the worker pulls job 0 out of the channel and blocks
        // on the gate; the channel (capacity 1) then holds exactly job 1.
        let hold = gate.lock().await;
        projector.project("s", 0, "{\"n\":0}").await;
        // Returns only once the worker took job 0, freeing the channel slot.
        projector.project("s", 1, "{\"n\":1}").await;

        // Channel full + worker parked: the third send must pend (backpressure).
        let pending = {
            let projector = projector.clone();
            tokio::spawn(async move { projector.project("s", 2, "{\"n\":2}").await })
        };
        tokio::task::yield_now().await;
        assert!(
            !pending.is_finished(),
            "third project must wait for channel capacity"
        );

        // Resume the worker: the pending send completes and all lines land.
        drop(hold);
        pending.await.unwrap();
        projector.flush().await;

        let content = std::fs::read_to_string(dir.path().join("s/transcript.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines, vec!["{\"n\":0}", "{\"n\":1}", "{\"n\":2}"]);
    }

    /// P1-08/P1-13: one projector per database identity — two lookups for the
    /// same database file share a single worker.
    #[tokio::test]
    async fn for_database_shares_one_worker_per_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("holmes.db");
        std::fs::write(&db_path, "").unwrap();
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let conn = Arc::new(tokio::sync::Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        ));
        let p1 = TranscriptProjector::for_database(&db_path, &sessions_dir, conn.clone());
        let p2 = TranscriptProjector::for_database(&db_path, &sessions_dir, conn);
        assert!(Arc::ptr_eq(&p1.shared, &p2.shared));
        p1.project("s", 0, "{}").await;
        // Flushing through the second handle drains the first handle's jobs.
        p2.flush().await;
        assert!(sessions_dir.join("s/transcript.jsonl").exists());
    }

    /// P1-13: two different database files in the SAME directory get distinct
    /// projectors (distinct workers, distinct namespaces).
    #[tokio::test]
    async fn for_database_distinguishes_databases_in_one_directory() {
        let dir = tempfile::tempdir().unwrap();
        let db_a = dir.path().join("a.db");
        let db_b = dir.path().join("b.db");
        std::fs::write(&db_a, "").unwrap();
        std::fs::write(&db_b, "").unwrap();
        let sessions_a = dir.path().join("sessions-a");
        let sessions_b = dir.path().join("sessions-b");
        std::fs::create_dir_all(&sessions_a).unwrap();
        std::fs::create_dir_all(&sessions_b).unwrap();
        let conn = Arc::new(tokio::sync::Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        ));
        let pa = TranscriptProjector::for_database(&db_a, &sessions_a, conn.clone());
        let pb = TranscriptProjector::for_database(&db_b, &sessions_b, conn);
        assert!(
            !Arc::ptr_eq(&pa.shared, &pb.shared),
            "different databases must not share a worker"
        );
        pa.project("shared-id", 0, "{\"db\":\"a\"}").await;
        pb.project("shared-id", 0, "{\"db\":\"b\"}").await;
        pa.flush().await;
        pb.flush().await;
        let a = std::fs::read_to_string(sessions_a.join("shared-id/transcript.jsonl")).unwrap();
        let b = std::fs::read_to_string(sessions_b.join("shared-id/transcript.jsonl")).unwrap();
        assert_eq!(a, "{\"db\":\"a\"}\n");
        assert_eq!(b, "{\"db\":\"b\"}\n");
    }

    /// P1-12: a path-unsafe session id is refused at every projector entry
    /// point and never touches the filesystem.
    #[tokio::test]
    async fn path_unsafe_session_id_is_refused_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        let projector = TranscriptProjector::new(dir.path().join("sessions"));
        projector.project("../outside", 0, "{}").await;
        projector.flush().await;
        assert!(
            !outside.exists(),
            "projection must not escape the sessions dir"
        );
        assert!(
            projector.take_rebuild_queue().is_empty(),
            "refused projection is dropped, not queued for rebuild"
        );
        assert!(projector.rebuild_via_worker("../outside").await.is_err());
        assert!(projector
            .rebuild_from_events("../outside", &["{}".into()])
            .is_err());
        assert!(!outside.exists());
    }
}
