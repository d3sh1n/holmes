pub const SCHEMA_VERSION: u32 = 1;

pub const MIGRATIONS: &[&str] = &[r#"
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

    PRAGMA journal_mode=WAL;
    PRAGMA busy_timeout=1000;
    PRAGMA foreign_keys=ON;
    "#];

pub fn schema_version_table() -> &'static str {
    r#"
    CREATE TABLE IF NOT EXISTS schema_version (
        version INTEGER PRIMARY KEY,
        applied_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    "#
}
