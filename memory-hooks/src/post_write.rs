//! Post-write hook: updates file anatomy write count and modification time.

use anyhow::Result;
use rusqlite::Connection;

use crate::HookInput;

pub fn run(conn: &Connection, input: &HookInput) -> Result<usize> {
    let file_path = match input.file_path() {
        Some(fp) => fp,
        None => return Ok(0),
    };
    let project = match input.project() {
        Some(p) => p,
        None => return Ok(0),
    };

    // UPSERT anatomy: single statement instead of UPDATE-check-INSERT
    let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
    let description = format!("File: {filename}");
    conn.execute(
        "INSERT INTO file_anatomy (project, file_path, description, last_modified, last_scanned, times_read, times_written) \
         VALUES (?1, ?2, ?3, datetime('now'), datetime('now'), 0, 1) \
         ON CONFLICT(project, file_path) DO UPDATE SET \
         times_written = times_written + 1, last_modified = datetime('now')",
        rusqlite::params![project, file_path, description],
    )?;

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookInput;
    use serde_json::json;

    fn make_input() -> HookInput {
        HookInput {
            session_id: Some("test-session".into()),
            cwd: Some("D:/r/myproject".into()),
            tool_name: Some("Edit".into()),
            tool_input: Some(json!({"file_path": "D:/r/myproject/src/main.rs"})),
            tool_response: None,
        }
    }

    #[test]
    fn test_post_write_always_returns_zero_overhead() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        let bytes = run(&conn, &make_input()).unwrap();
        assert_eq!(
            bytes, 0,
            "post_write must not contribute overhead (no stdout/stderr)"
        );
    }

    #[test]
    fn test_post_write_creates_anatomy() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        run(&conn, &make_input()).unwrap();

        let times_written: i64 = conn.query_row(
            "SELECT times_written FROM file_anatomy WHERE project = 'myproject' AND file_path = 'D:/r/myproject/src/main.rs'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(times_written, 1);
    }

    #[test]
    fn test_post_write_upsert_increments() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        let input = make_input();
        run(&conn, &input).unwrap();
        run(&conn, &input).unwrap();

        let times_written: i64 = conn.query_row(
            "SELECT times_written FROM file_anatomy WHERE project = 'myproject' AND file_path = 'D:/r/myproject/src/main.rs'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(times_written, 2);
    }
}
