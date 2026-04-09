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
    }
    let conn = Connection::open(&path)
        .with_context(|| format!("failed to open database: {}", path.display()))?;
    setup_pragmas(&conn)?;
    run_migrations(&conn)?;
    Ok(conn)
}

/// Opens an in-memory database for testing.
#[cfg(test)]
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

/// Runs all schema migrations idempotently.
pub fn run_migrations(conn: &Connection) -> Result<()> {
    // Regular tables (CREATE IF NOT EXISTS)
    for sql in schema::ALL_MIGRATIONS {
        conn.execute_batch(sql)
            .with_context(|| format!("migration failed: {}", &sql[..sql.len().min(80)]))?;
    }

    // Multi-statement index migrations
    for sql in schema::INDEX_MIGRATIONS {
        // Split on semicolons and execute individually
        for stmt in sql.split(';').filter(|s| !s.trim().is_empty()) {
            conn.execute_batch(stmt)
                .with_context(|| format!("index migration failed: {}", &stmt[..stmt.len().min(80)]))?;
        }
    }

    // FTS tables need existence check since some SQLite versions
    // don't handle IF NOT EXISTS properly for virtual tables.
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

    Ok(())
}

/// Normalize a file path to use forward slashes for consistent DB storage.
pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
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
        assert!(tables.contains(&"do_not_repeat_fts".to_string()));
    }

    #[test]
    fn test_migrations_are_idempotent() {
        let conn = open_db_in_memory().expect("first open");
        // Run migrations again — should not fail
        run_migrations(&conn).expect("second migration run should succeed");
    }

    #[test]
    fn test_normalize_path() {
        assert_eq!(normalize_path(r"D:\r\git_dag_analyzer"), "D:/r/git_dag_analyzer");
        assert_eq!(normalize_path("D:/r/git_dag_analyzer"), "D:/r/git_dag_analyzer");
    }

    #[test]
    fn test_project_from_cwd() {
        assert_eq!(project_from_cwd(r"D:\r\git_dag_analyzer"), "git_dag_analyzer");
        assert_eq!(project_from_cwd("D:/r/mnemosyne/"), "mnemosyne");
        assert_eq!(project_from_cwd("/home/user/projects/foo"), "foo");
    }
}
