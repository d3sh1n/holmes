pub const SCHEMA_VERSION: u32 = 10;

pub const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        title TEXT,
        mode TEXT NOT NULL DEFAULT 'pentest',
        model TEXT,
        model_config TEXT,
        system_prompt TEXT,
        parent_session_id TEXT,
        fork_point INTEGER,
        source TEXT NOT NULL DEFAULT 'cli',
        tags TEXT NOT NULL DEFAULT '[]',
        started_at TEXT NOT NULL,
        ended_at TEXT,
        end_reason TEXT,
        message_count INTEGER NOT NULL DEFAULT 0,
        tool_call_count INTEGER NOT NULL DEFAULT 0,
        subagent_count INTEGER NOT NULL DEFAULT 0,
        input_tokens INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0,
        cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        estimated_cost_usd REAL NOT NULL DEFAULT 0.0,
        goal_condition TEXT,
        goal_achieved INTEGER NOT NULL DEFAULT 0,
        FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
    );

    CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);
    CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);
    CREATE INDEX IF NOT EXISTS idx_sessions_source ON sessions(source);
    CREATE INDEX IF NOT EXISTS idx_sessions_mode ON sessions(mode);

    CREATE TABLE IF NOT EXISTS events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL,
        event_index INTEGER NOT NULL,
        turn_index INTEGER,
        event_type TEXT NOT NULL,
        event_data TEXT NOT NULL,
        timestamp TEXT NOT NULL,
        FOREIGN KEY (session_id) REFERENCES sessions(id)
    );

    CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, event_index);
    CREATE INDEX IF NOT EXISTS idx_events_turn ON events(session_id, turn_index);
    CREATE INDEX IF NOT EXISTS idx_events_type ON events(session_id, event_type);

    CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
        event_type,
        content_text,
        session_id UNINDEXED,
        content='events',
        content_rowid='id'
    );

    CREATE TRIGGER IF NOT EXISTS events_ai AFTER INSERT ON events BEGIN
        INSERT INTO events_fts(rowid, event_type, content_text, session_id)
        VALUES (new.id, new.event_type,
                json_extract(new.event_data, '$.content_text'),
                new.session_id);
    END;

    CREATE TRIGGER IF NOT EXISTS events_ad AFTER DELETE ON events BEGIN
        INSERT INTO events_fts(events_fts, rowid, event_type, content_text, session_id)
        VALUES ('delete', old.id, old.event_type,
                json_extract(old.event_data, '$.content_text'),
                old.session_id);
    END;

    CREATE TRIGGER IF NOT EXISTS events_au AFTER UPDATE ON events BEGIN
        INSERT INTO events_fts(events_fts, rowid, event_type, content_text, session_id)
        VALUES ('delete', old.id, old.event_type,
                json_extract(old.event_data, '$.content_text'),
                old.session_id);
        INSERT INTO events_fts(rowid, event_type, content_text, session_id)
        VALUES (new.id, new.event_type,
                json_extract(new.event_data, '$.content_text'),
                new.session_id);
    END;

    CREATE TABLE IF NOT EXISTS memories (
        id TEXT PRIMARY KEY,
        category TEXT NOT NULL,
        content TEXT NOT NULL,
        tags TEXT NOT NULL DEFAULT '[]',
        attack_type TEXT,
        tech_stack TEXT NOT NULL DEFAULT '[]',
        success INTEGER NOT NULL DEFAULT 0,
        relevance_score REAL NOT NULL DEFAULT 0.0,
        source_session_id TEXT,
        consolidated_from TEXT,
        created_at TEXT NOT NULL,
        accessed_at TEXT,
        access_count INTEGER NOT NULL DEFAULT 0
    );

    CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
        content,
        attack_type,
        category,
        content='memories',
        content_rowid='rowid'
    );

    CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
        INSERT INTO memories_fts(rowid, content, attack_type, category)
        VALUES (new.rowid, new.content, new.attack_type, new.category);
    END;

    CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
        INSERT INTO memories_fts(memories_fts, rowid, content, attack_type, category)
        VALUES ('delete', old.rowid, old.content, old.attack_type, old.category);
    END;

    CREATE TABLE IF NOT EXISTS subtasks (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        goal_event_id INTEGER NOT NULL,
        description TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending',
        parent_subtask_id TEXT,
        sort_order INTEGER NOT NULL DEFAULT 0,
        completed_at TEXT,
        note TEXT,
        FOREIGN KEY (session_id) REFERENCES sessions(id)
    );

    CREATE INDEX IF NOT EXISTS idx_subtasks_session ON subtasks(session_id);
    -- NOTE: no PRAGMAs here. journal_mode/foreign_keys cannot change inside a
    -- transaction, and migrations run inside BEGIN IMMEDIATE (P1-04);
    -- `SessionDB::open` applies WAL / busy_timeout=5000 / foreign_keys=ON per
    -- connection before the migration transaction starts.
    "#,
    r#"
    -- Durable background task store (AGT-007): task state survives process
    -- restarts; leases distinguish a live runner from a crashed one.
    CREATE TABLE IF NOT EXISTS tasks (
        task_id TEXT PRIMARY KEY,
        parent_session_id TEXT,
        child_session_id TEXT,
        kind TEXT NOT NULL DEFAULT 'subagent',
        description TEXT NOT NULL DEFAULT '',
        state TEXT NOT NULL DEFAULT 'queued',
        lease_owner TEXT,
        lease_expires_at TEXT,
        attempt INTEGER NOT NULL DEFAULT 0,
        idempotency_key TEXT UNIQUE,
        checkpoint TEXT,
        result TEXT,
        last_error TEXT,
        safe_to_retry INTEGER NOT NULL DEFAULT 1,
        delivered INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
    );

    CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_session_id);
    CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(state);
    CREATE INDEX IF NOT EXISTS idx_tasks_lease ON tasks(state, lease_expires_at);
    "#,
    r#"
    -- Richer memory data model (AGT-011/012): provenance, confidence,
    -- freshness, scope, conflict/supersede links, sensitivity, lifecycle
    -- status (staged/active/disabled/archived), local embedding for hybrid
    -- recall, and skill versioning/validation/usage stats.
    ALTER TABLE memories ADD COLUMN source TEXT NOT NULL DEFAULT 'agent_inferred';
    ALTER TABLE memories ADD COLUMN confidence REAL NOT NULL DEFAULT 0.5;
    ALTER TABLE memories ADD COLUMN scope TEXT NOT NULL DEFAULT 'global';
    ALTER TABLE memories ADD COLUMN status TEXT NOT NULL DEFAULT 'active';
    ALTER TABLE memories ADD COLUMN last_verified_at TEXT;
    ALTER TABLE memories ADD COLUMN expires_at TEXT;
    ALTER TABLE memories ADD COLUMN conflicts_with TEXT NOT NULL DEFAULT '[]';
    ALTER TABLE memories ADD COLUMN supersedes TEXT;
    ALTER TABLE memories ADD COLUMN sensitive INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE memories ADD COLUMN embedding TEXT;
    ALTER TABLE memories ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE memories ADD COLUMN parent_version_id TEXT;
    ALTER TABLE memories ADD COLUMN validation_status TEXT NOT NULL DEFAULT 'none';
    ALTER TABLE memories ADD COLUMN approved_by TEXT;
    ALTER TABLE memories ADD COLUMN use_count INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE memories ADD COLUMN success_count INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE memories ADD COLUMN failure_count INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE memories ADD COLUMN last_used_at TEXT;

    CREATE INDEX IF NOT EXISTS idx_memories_status ON memories(status);
    CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope);
    "#,
    r#"
    -- Durable task re-execution payload (P1-02): the spawn-time arguments a
    -- scheduler needs to re-run a leased task after recovery. NULL for tasks
    -- that cannot be re-executed (no registered executor for their kind).
    ALTER TABLE tasks ADD COLUMN payload TEXT;
    "#,
    r#"
    -- Tool event correlation (P1-03): ToolCall / ToolResult / ToolBlocked event
    -- payloads now carry the native `call_id` so replay and supervision can bind
    -- results to calls without relying on adjacency or name matching. Events are
    -- stored as JSON (`events.event_data`), so no DDL is required: old payloads
    -- without `call_id` stay readable via serde defaults, and replay synthesizes
    -- failure tool-results for legacy `ToolBlocked` / dangling `ToolCall`
    -- events. This migration is intentionally comment-only; it exists so the
    -- payload contract change is pinned to a schema version.
    "#,
    r#"
    -- SQLite as the sole authoritative store for large tool results (P1-08):
    -- oversized ToolResult payloads live in content-addressed blob chunks,
    -- inserted in the SAME transaction as their event row, so the database
    -- alone can always restore the evidence (the old tool-results sidecar
    -- pointer made the DB unrecoverable once the file was lost).
    -- `projection_state` persists the transcript projector's per-session high
    -- watermark so `SessionDB::open` can reconcile the derived transcript
    -- against the authoritative events after a crash or projection failure.
    CREATE TABLE IF NOT EXISTS blobs (
        sha256 TEXT PRIMARY KEY,
        codec TEXT NOT NULL,
        original_size INTEGER NOT NULL,
        chunk_count INTEGER NOT NULL,
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    );

    CREATE TABLE IF NOT EXISTS blob_chunks (
        blob_sha256 TEXT NOT NULL,
        chunk_index INTEGER NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (blob_sha256, chunk_index),
        FOREIGN KEY (blob_sha256) REFERENCES blobs(sha256)
    );

    CREATE TABLE IF NOT EXISTS projection_state (
        session_id TEXT PRIMARY KEY,
        projected_event_index INTEGER NOT NULL DEFAULT -1,
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    "#,
    r#"
    -- Typed tool outcomes (Hypothesis Ledger v2 Phase 0): ToolResult event JSON now
    -- carries optional `outcome` (succeeded/failed/timed_out/cancelled/denied).
    -- No DDL is required. Legacy payloads without the field remain readable and
    -- derive Succeeded/Failed from their `success` boolean; new writers persist both
    -- and consumers fail closed when they disagree.
    "#,
    r#"
    -- Hypothesis Ledger v2 Phase 1: conversations remain session-scoped while
    -- epistemic state is shared by a case. Existing parent/child trees are
    -- backfilled to the same root case; disconnected legacy rows receive their
    -- own case. New root sessions use independent case ids and children inherit.
    ALTER TABLE sessions ADD COLUMN case_id TEXT;

    WITH RECURSIVE lineage(session_id, case_id) AS (
        SELECT id, id FROM sessions WHERE parent_session_id IS NULL
        UNION ALL
        SELECT child.id, lineage.case_id
        FROM sessions child
        JOIN lineage ON child.parent_session_id = lineage.session_id
    )
    UPDATE sessions
    SET case_id = (
        SELECT lineage.case_id FROM lineage WHERE lineage.session_id = sessions.id
    );

    UPDATE sessions SET case_id = id WHERE case_id IS NULL;
    CREATE INDEX IF NOT EXISTS idx_sessions_case ON sessions(case_id);

    CREATE TABLE IF NOT EXISTS cases (
        case_id TEXT PRIMARY KEY NOT NULL,
        root_session_id TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'open',
        ledger_version INTEGER NOT NULL DEFAULT 0,
        next_evidence_seq INTEGER NOT NULL DEFAULT 1,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );

    INSERT OR IGNORE INTO cases (
        case_id, root_session_id, status, ledger_version,
        next_evidence_seq, created_at, updated_at
    )
    SELECT grouped.case_id,
           grouped.case_id,
           'open',
           0,
           1,
           COALESCE(root.started_at, datetime('now')),
           COALESCE(root.started_at, datetime('now'))
    FROM (SELECT DISTINCT case_id FROM sessions) AS grouped
    LEFT JOIN sessions AS root ON root.id = grouped.case_id;

    CREATE TABLE IF NOT EXISTS case_ledger_commands (
        case_id TEXT NOT NULL,
        command_id TEXT NOT NULL,
        payload_hash TEXT NOT NULL,
        expected_version INTEGER NOT NULL,
        first_seq INTEGER NOT NULL,
        event_count INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (case_id, command_id),
        FOREIGN KEY (case_id) REFERENCES cases(case_id)
    );

    CREATE TABLE IF NOT EXISTS case_ledger_events (
        case_id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        event_id TEXT NOT NULL,
        command_id TEXT NOT NULL,
        aggregate_kind TEXT NOT NULL,
        aggregate_id TEXT NOT NULL,
        aggregate_revision INTEGER NOT NULL,
        actor_session_id TEXT NOT NULL,
        event_type TEXT NOT NULL,
        event_data TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (case_id, seq),
        UNIQUE (event_id),
        FOREIGN KEY (case_id) REFERENCES cases(case_id),
        FOREIGN KEY (case_id, command_id)
            REFERENCES case_ledger_commands(case_id, command_id)
    );

    CREATE INDEX IF NOT EXISTS idx_case_ledger_events_aggregate
    ON case_ledger_events(case_id, aggregate_kind, aggregate_id, seq);
    CREATE INDEX IF NOT EXISTS idx_case_ledger_events_command
    ON case_ledger_events(case_id, command_id, seq);

    CREATE TABLE IF NOT EXISTS case_ledger_snapshots (
        case_id TEXT PRIMARY KEY NOT NULL,
        projected_seq INTEGER NOT NULL,
        state_json TEXT NOT NULL,
        checksum TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        FOREIGN KEY (case_id) REFERENCES cases(case_id)
    );
    "#,
    r#"
    -- Hypothesis Ledger v2 Phase 2: evidence receipt commands remember the
    -- session ToolResult index committed with their case event. This makes a
    -- retry return the original receipt without appending or projecting a
    -- duplicate transcript line. Phase 1 meta commands leave this NULL.
    ALTER TABLE case_ledger_commands ADD COLUMN session_event_index INTEGER;
    "#,
    r#"
    -- Hypothesis Ledger v2 Phase 5: durable tasks may be the exclusive
    -- execution adapter for one case-scoped Experiment. lease_duration_ms is
    -- persisted so heartbeat/recovery keeps the claim policy across restart.
    ALTER TABLE tasks ADD COLUMN case_id TEXT REFERENCES cases(case_id);
    ALTER TABLE tasks ADD COLUMN experiment_id TEXT;
    ALTER TABLE tasks ADD COLUMN lease_duration_ms INTEGER NOT NULL DEFAULT 300000;
    ALTER TABLE tasks ADD COLUMN max_concurrent_per_case INTEGER NOT NULL DEFAULT 4;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_case_experiment
        ON tasks(case_id, experiment_id)
        WHERE case_id IS NOT NULL AND experiment_id IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_tasks_case_state
        ON tasks(case_id, state, lease_expires_at);
    "#,
];

pub fn schema_version_table() -> &'static str {
    r#"
    CREATE TABLE IF NOT EXISTS schema_version (
        version INTEGER PRIMARY KEY,
        applied_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    "#
}
