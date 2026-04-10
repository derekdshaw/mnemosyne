//! SQL DDL statements for all Mnemosyne database tables and indexes.
//!
//! All statements use CREATE IF NOT EXISTS for idempotent execution.
//! PRAGMAs are set procedurally in [`crate::db::setup_pragmas`].

pub const CREATE_INGESTION_LOG: &str = "\
    CREATE TABLE IF NOT EXISTS ingestion_log (\
        file_path TEXT PRIMARY KEY,\
        ingested_at TEXT NOT NULL,\
        line_count INTEGER NOT NULL,\
        file_size INTEGER NOT NULL,\
        file_mtime TEXT NOT NULL\
    );\
";

pub const CREATE_SESSIONS: &str = "\
    CREATE TABLE IF NOT EXISTS sessions (\
        session_id TEXT PRIMARY KEY,\
        project TEXT,\
        start_time TEXT,\
        end_time TEXT,\
        cwd TEXT,\
        git_branch TEXT,\
        message_count INTEGER DEFAULT 0,\
        total_input_tokens INTEGER DEFAULT 0,\
        total_output_tokens INTEGER DEFAULT 0\
    );\
";

pub const CREATE_MESSAGES: &str = "\
    CREATE TABLE IF NOT EXISTS messages (\
        uuid TEXT PRIMARY KEY,\
        session_id TEXT NOT NULL REFERENCES sessions(session_id),\
        parent_uuid TEXT,\
        role TEXT NOT NULL,\
        content_type TEXT,\
        content TEXT,\
        tool_name TEXT,\
        timestamp TEXT,\
        model TEXT\
    );\
";

pub const CREATE_MESSAGES_INDEX: &str = "\
    CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);\
";

pub const CREATE_MESSAGES_FTS: &str = "\
    CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(\
        uuid UNINDEXED,\
        session_id UNINDEXED,\
        content\
    );\
";

pub const CREATE_TOOL_CALLS: &str = "\
    CREATE TABLE IF NOT EXISTS tool_calls (\
        id INTEGER PRIMARY KEY AUTOINCREMENT,\
        message_uuid TEXT NOT NULL REFERENCES messages(uuid),\
        session_id TEXT NOT NULL REFERENCES sessions(session_id),\
        tool_name TEXT NOT NULL,\
        tool_input_summary TEXT,\
        file_path TEXT,\
        timestamp TEXT\
    );\
";

pub const CREATE_TOOL_CALLS_INDEXES: &str = "\
    CREATE INDEX IF NOT EXISTS idx_tool_calls_file ON tool_calls(file_path);\
    CREATE INDEX IF NOT EXISTS idx_tool_calls_session ON tool_calls(session_id);\
";

pub const CREATE_TOKEN_USAGE: &str = "\
    CREATE TABLE IF NOT EXISTS token_usage (\
        message_uuid TEXT PRIMARY KEY REFERENCES messages(uuid),\
        session_id TEXT NOT NULL REFERENCES sessions(session_id),\
        input_tokens INTEGER,\
        output_tokens INTEGER,\
        cache_read_tokens INTEGER,\
        cache_creation_tokens INTEGER\
    );\
";

pub const CREATE_CONTEXT_ITEMS: &str = "\
    CREATE TABLE IF NOT EXISTS context_items (\
        id INTEGER PRIMARY KEY AUTOINCREMENT,\
        project TEXT,\
        category TEXT NOT NULL,\
        content TEXT NOT NULL,\
        created_at TEXT NOT NULL,\
        updated_at TEXT,\
        source_session_id TEXT\
    );\
";

pub const CREATE_CONTEXT_FTS: &str = "\
    CREATE VIRTUAL TABLE IF NOT EXISTS context_fts USING fts5(\
        item_id UNINDEXED,\
        project UNINDEXED,\
        category UNINDEXED,\
        content\
    );\
";

pub const CREATE_BUGS: &str = "\
    CREATE TABLE IF NOT EXISTS bugs (\
        id INTEGER PRIMARY KEY AUTOINCREMENT,\
        project TEXT,\
        error_message TEXT NOT NULL,\
        root_cause TEXT,\
        fix_description TEXT NOT NULL,\
        tags TEXT,\
        file_path TEXT,\
        created_at TEXT NOT NULL,\
        source_session_id TEXT\
    );\
";

pub const CREATE_BUGS_FTS: &str = "\
    CREATE VIRTUAL TABLE IF NOT EXISTS bugs_fts USING fts5(\
        bug_id UNINDEXED,\
        project UNINDEXED,\
        file_path UNINDEXED,\
        error_message, root_cause, fix_description\
    );\
";

pub const CREATE_FILE_ANATOMY: &str = "\
    CREATE TABLE IF NOT EXISTS file_anatomy (\
        project TEXT NOT NULL,\
        file_path TEXT NOT NULL,\
        description TEXT,\
        estimated_tokens INTEGER,\
        last_modified TEXT,\
        last_scanned TEXT,\
        times_read INTEGER DEFAULT 0,\
        times_written INTEGER DEFAULT 0,\
        PRIMARY KEY (project, file_path)\
    );\
";

pub const CREATE_SESSION_READS: &str = "\
    CREATE TABLE IF NOT EXISTS session_reads (\
        id INTEGER PRIMARY KEY AUTOINCREMENT,\
        session_id TEXT NOT NULL,\
        file_path TEXT NOT NULL,\
        read_at TEXT NOT NULL,\
        token_estimate INTEGER\
    );\
";

pub const CREATE_SESSION_READS_INDEX: &str = "\
    CREATE INDEX IF NOT EXISTS idx_session_reads_lookup ON session_reads(session_id, file_path);\
";

pub const CREATE_DO_NOT_REPEAT: &str = "\
    CREATE TABLE IF NOT EXISTS do_not_repeat (\
        id INTEGER PRIMARY KEY AUTOINCREMENT,\
        project TEXT,\
        rule TEXT NOT NULL,\
        reason TEXT,\
        file_path TEXT,\
        created_at TEXT NOT NULL,\
        source_session_id TEXT\
    );\
";

pub const CREATE_DO_NOT_REPEAT_INDEX: &str = "\
    CREATE INDEX IF NOT EXISTS idx_do_not_repeat_project ON do_not_repeat(project);\
";

/// All migration statements in order.
pub const ALL_MIGRATIONS: &[&str] = &[
    CREATE_INGESTION_LOG,
    CREATE_SESSIONS,
    CREATE_MESSAGES,
    CREATE_MESSAGES_INDEX,
    CREATE_TOOL_CALLS,
    CREATE_TOKEN_USAGE,
    CREATE_CONTEXT_ITEMS,
    CREATE_BUGS,
    CREATE_FILE_ANATOMY,
    CREATE_SESSION_READS,
    CREATE_SESSION_READS_INDEX,
    CREATE_DO_NOT_REPEAT,
    CREATE_DO_NOT_REPEAT_INDEX,
];

/// FTS tables must be created separately (they fail if re-created when they already exist
/// even with IF NOT EXISTS in some SQLite versions, so we check first).
/// Note: do_not_repeat has no FTS table — rules are retrieved by exact project/file match,
/// not free-text search. The table is small (tens of rules) so FTS adds no value.
pub const FTS_MIGRATIONS: &[(&str, &str)] = &[
    ("messages_fts", CREATE_MESSAGES_FTS),
    ("context_fts", CREATE_CONTEXT_FTS),
    ("bugs_fts", CREATE_BUGS_FTS),
];

/// Index migrations that use multi-statement strings.
pub const INDEX_MIGRATIONS: &[&str] = &[
    CREATE_TOOL_CALLS_INDEXES,
];
