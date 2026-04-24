//! SQLite database connection management for Mnemosyne.
//!
//! Opens the database at `~/.claude/memory/memory.db`, configures WAL mode
//! and safety PRAGMAs, runs schema migrations with version checking, and
//! provides path normalization utilities.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;

use crate::schema;

/// Returns the path to the Mnemosyne database: `~/.claude/memory/memory.db`
pub fn db_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".claude").join("memory").join("memory.db"))
}

/// Opens (or creates) the Mnemosyne SQLite database with proper PRAGMAs and runs migrations.
pub fn open_db() -> Result<Connection> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
        // S6: Verify DB parent is not a symlink
        let meta = std::fs::symlink_metadata(parent)
            .with_context(|| format!("failed to read metadata: {}", parent.display()))?;
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "database parent directory is a symlink: {}",
                parent.display()
            );
        }
    }
    let conn = Connection::open(&path)
        .with_context(|| format!("failed to open database: {}", path.display()))?;
    // S7: Set restrictive permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }
    setup_pragmas(&conn)?;
    run_migrations(&conn)?;
    Ok(conn)
}

/// Runs `PRAGMA wal_checkpoint(TRUNCATE)` so a subsequent close doesn't leave
/// WAL frames pinned on disk. Intended to be called just before the MCP server
/// process exits — the in-memory sqlite automatic checkpointer doesn't always
/// flush before the process is killed, which left us with a 5 MB WAL pinning
/// the DB across sessions.
pub fn checkpoint_wal(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

/// Opens an in-memory database for testing. Available to all crates in the workspace.
pub fn open_db_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    setup_pragmas(&conn)?;
    run_migrations(&conn)?;
    Ok(conn)
}

fn setup_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA busy_timeout=3000;")?;
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(())
}

/// Current schema version. Bump this whenever schema changes.
const SCHEMA_VERSION: i64 = 4;

/// Runs schema migrations only if the database is behind the current version.
/// Uses PRAGMA user_version to skip all migration work when schema is current.
pub fn run_migrations(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    run_migrations_unconditionally(conn)?;

    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    Ok(())
}

/// Add a column to a table, ignoring "duplicate column" errors for idempotency.
fn add_column_if_not_exists(
    conn: &Connection,
    table: &str,
    column: &str,
    col_type: &str,
) -> Result<()> {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {col_type}");
    match conn.execute_batch(&sql) {
        Ok(()) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column") => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Runs all migration statements regardless of version. Used on first setup
/// and when SCHEMA_VERSION is bumped.
fn run_migrations_unconditionally(conn: &Connection) -> Result<()> {
    for sql in schema::ALL_MIGRATIONS {
        conn.execute_batch(sql)
            .with_context(|| format!("migration failed: {}", &sql[..sql.len().min(80)]))?;
    }

    for sql in schema::INDEX_MIGRATIONS {
        for stmt in sql.split(';').filter(|s| !s.trim().is_empty()) {
            conn.execute_batch(stmt).with_context(|| {
                format!("index migration failed: {}", &stmt[..stmt.len().min(80)])
            })?;
        }
    }

    for (table_name, create_sql) in schema::FTS_MIGRATIONS {
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
            [table_name],
            |row| row.get(0),
        )?;
        if !exists {
            conn.execute_batch(create_sql)
                .with_context(|| format!("FTS migration failed for {table_name}"))?;
        }
    }

    // V2: Add original_length column for caveman compression tracking
    add_column_if_not_exists(conn, "context_items", "original_length", "INTEGER")?;
    add_column_if_not_exists(conn, "messages", "original_length", "INTEGER")?;
    add_column_if_not_exists(conn, "bugs", "original_length", "INTEGER")?;

    // V3: Symbol-line index on file_anatomy. JSON-encoded array of [kind, name, line]
    // triples so the pre-read hook can let Claude jump straight to a symbol instead
    // of reading the whole file.
    add_column_if_not_exists(conn, "file_anatomy", "top_symbols_json", "TEXT")?;

    Ok(())
}

/// Normalize a file path to use forward slashes for consistent DB storage.
pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Truncate a string to at most `max` bytes, respecting UTF-8 char boundaries.
/// Appends "..." if truncated. Never panics on multi-byte characters.
pub fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Record mnemosyne's own hook output as overhead in tokens added to Claude's
/// context. Used to measure the cost side of mnemosyne's intervention (the
/// savings side is tracked via `session_reads.token_estimate` + repeat detection).
/// Uses the same `bytes / 3.5` heuristic as post-read so overhead and savings
/// are directly comparable.
pub fn record_overhead(
    conn: &Connection,
    session_id: Option<&str>,
    project: Option<&str>,
    hook_name: &str,
    output_bytes: usize,
) -> Result<()> {
    if output_bytes == 0 {
        return Ok(());
    }
    let estimated_tokens = (output_bytes as f64 / 3.5) as i64;
    conn.execute(
        "INSERT INTO mnemosyne_overhead \
         (session_id, project, hook_name, output_bytes, estimated_tokens, emitted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
        rusqlite::params![
            session_id,
            project,
            hook_name,
            output_bytes as i64,
            estimated_tokens,
        ],
    )?;
    Ok(())
}

/// Derive a project name from a working directory path.
/// Takes the last component of the path.
pub fn project_from_cwd(cwd: &str) -> String {
    let normalized = normalize_path(cwd);
    normalized
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_db_in_memory() {
        let conn = open_db_in_memory().expect("should create in-memory DB");

        // Verify all tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"messages".to_string()));
        assert!(tables.contains(&"tool_calls".to_string()));
        assert!(tables.contains(&"token_usage".to_string()));
        assert!(tables.contains(&"context_items".to_string()));
        assert!(tables.contains(&"bugs".to_string()));
        assert!(tables.contains(&"file_anatomy".to_string()));
        assert!(tables.contains(&"session_reads".to_string()));
        assert!(tables.contains(&"do_not_repeat".to_string()));
        assert!(tables.contains(&"ingestion_log".to_string()));
        assert!(tables.contains(&"messages_fts".to_string()));
        assert!(tables.contains(&"context_fts".to_string()));
        assert!(tables.contains(&"bugs_fts".to_string()));
        assert!(tables.contains(&"mnemosyne_overhead".to_string()));
        // do_not_repeat has no FTS table (exact match only, not free-text search)
    }

    #[test]
    fn test_record_overhead_basic() {
        let conn = open_db_in_memory().unwrap();
        record_overhead(&conn, Some("s1"), Some("proj"), "session_start", 7).unwrap();

        let (session_id, project, hook_name, output_bytes, estimated_tokens): (
            String,
            String,
            String,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT session_id, project, hook_name, output_bytes, estimated_tokens \
                 FROM mnemosyne_overhead",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(session_id, "s1");
        assert_eq!(project, "proj");
        assert_eq!(hook_name, "session_start");
        assert_eq!(output_bytes, 7);
        // 7 / 3.5 = 2 exactly
        assert_eq!(estimated_tokens, 2);
    }

    #[test]
    fn test_record_overhead_zero_is_noop() {
        let conn = open_db_in_memory().unwrap();
        record_overhead(&conn, Some("s1"), Some("proj"), "pre_read", 0).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mnemosyne_overhead", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "zero-byte output should not insert a row");
    }

    #[test]
    fn test_record_overhead_null_session_and_project() {
        let conn = open_db_in_memory().unwrap();
        // Hook with no session_id / project (e.g., session_start before DB has any data)
        record_overhead(&conn, None, None, "session_start", 500).unwrap();

        let (session_id, project): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT session_id, project FROM mnemosyne_overhead",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(session_id.is_none());
        assert!(project.is_none());
    }

    #[test]
    fn test_migrations_are_idempotent() {
        let conn = open_db_in_memory().expect("first open");
        // Run migrations again — should not fail
        run_migrations(&conn).expect("second migration run should succeed");
    }

    #[test]
    fn test_checkpoint_wal_truncates() {
        // File-backed DB so the WAL file is observable on disk.
        let dir = std::env::temp_dir();
        let db_path = dir.join(format!(
            "mnemosyne_checkpoint_test_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wal_path = db_path.with_extension("db-wal");
        let shm_path = db_path.with_extension("db-shm");

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            conn.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
            for i in 0..50 {
                conn.execute("INSERT INTO t VALUES (?1)", [i]).unwrap();
            }
            // With WAL mode, uncheckpointed writes leave the WAL file non-empty.
            let wal_size = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
            assert!(
                wal_size > 0,
                "expected WAL to have data before checkpoint, got {wal_size} bytes"
            );

            checkpoint_wal(&conn).expect("checkpoint should succeed");
        }

        // After connection is dropped, the WAL file exists but should be 0 bytes
        // (TRUNCATE mode zero-truncates the file in place).
        let wal_size_after = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            wal_size_after, 0,
            "WAL should be truncated to 0 bytes after checkpoint"
        );

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&wal_path);
        let _ = std::fs::remove_file(&shm_path);
    }

    #[test]
    fn test_normalize_path() {
        assert_eq!(
            normalize_path(r"D:\r\git_dag_analyzer"),
            "D:/r/git_dag_analyzer"
        );
        assert_eq!(
            normalize_path("D:/r/git_dag_analyzer"),
            "D:/r/git_dag_analyzer"
        );
    }

    #[test]
    fn test_truncate_utf8_ascii() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
        assert_eq!(truncate_utf8("hello world", 5), "hello...");
    }

    #[test]
    fn test_truncate_utf8_multibyte() {
        // Each emoji is 4 bytes. "🎉🎊" = 8 bytes
        let s = "🎉🎊🎈"; // 12 bytes
                          // Truncating at 5 should land inside the second emoji, back up to byte 4
        assert_eq!(truncate_utf8(s, 5), "🎉...");
        // Truncating at 8 should include two emojis
        assert_eq!(truncate_utf8(s, 8), "🎉🎊...");
        // Truncating at 12+ should return unchanged
        assert_eq!(truncate_utf8(s, 12), "🎉🎊🎈");
    }

    #[test]
    fn test_truncate_utf8_cjk() {
        // CJK chars are 3 bytes each
        let s = "你好世界"; // 12 bytes
        assert_eq!(truncate_utf8(s, 6), "你好...");
        assert_eq!(truncate_utf8(s, 7), "你好..."); // lands inside 世, backs up to 6
    }

    #[test]
    fn test_truncate_utf8_empty() {
        assert_eq!(truncate_utf8("", 10), "");
    }

    #[test]
    fn test_truncate_utf8_exact_boundary() {
        assert_eq!(truncate_utf8("hello", 5), "hello");
        assert_eq!(truncate_utf8("hello", 6), "hello");
    }

    #[test]
    fn test_schema_version_skip() {
        let conn = open_db_in_memory().expect("create DB");
        // user_version should be set after first migration
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(version >= 1, "user_version should be set after migration");
        // Second call should be a no-op (skip all DDL)
        run_migrations(&conn).expect("second run should skip and succeed");
    }

    #[test]
    fn test_project_from_cwd() {
        assert_eq!(
            project_from_cwd(r"D:\r\git_dag_analyzer"),
            "git_dag_analyzer"
        );
        assert_eq!(project_from_cwd("D:/r/mnemosyne/"), "mnemosyne");
        assert_eq!(project_from_cwd("/home/user/projects/foo"), "foo");
    }
}
