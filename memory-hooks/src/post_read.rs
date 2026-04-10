use anyhow::Result;
use rusqlite::Connection;

use crate::HookInput;

pub fn run(conn: &Connection, input: &HookInput) -> Result<()> {
    let file_path = match input.file_path() {
        Some(fp) => fp,
        None => return Ok(()),
    };
    let session_id = input.session_id.as_deref().unwrap_or("");
    let project = input.project();

    // Estimate tokens from response content
    let token_estimate = input
        .tool_response
        .as_ref()
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| (s.len() as f64 / 3.5) as i64);

    // Record this read in session_reads
    conn.execute(
        "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
         VALUES (?1, ?2, datetime('now'), ?3)",
        rusqlite::params![session_id, file_path, token_estimate],
    )?;

    // UPSERT anatomy: single statement instead of UPDATE-check-INSERT
    if let Some(ref proj) = project {
        let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
        let description = format!("File: {filename}");
        conn.execute(
            "INSERT INTO file_anatomy (project, file_path, description, estimated_tokens, last_scanned, times_read, times_written) \
             VALUES (?1, ?2, ?3, ?4, datetime('now'), 1, 0) \
             ON CONFLICT(project, file_path) DO UPDATE SET times_read = times_read + 1",
            rusqlite::params![proj, file_path, description, token_estimate],
        )?;
    }

    Ok(())
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
            tool_name: Some("Read".into()),
            tool_input: Some(json!({"file_path": "D:/r/myproject/src/main.rs"})),
            tool_response: Some(json!({"content": "fn main() { println!(\"hello\"); }"})),
        }
    }

    #[test]
    fn test_post_read_inserts_session_read() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        run(&conn, &make_input()).unwrap();

        let count: i64 = conn.query_row(
            "SELECT count(*) FROM session_reads WHERE session_id = 'test-session' AND file_path = 'D:/r/myproject/src/main.rs'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_post_read_creates_anatomy() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        run(&conn, &make_input()).unwrap();

        let times_read: i64 = conn.query_row(
            "SELECT times_read FROM file_anatomy WHERE project = 'myproject' AND file_path = 'D:/r/myproject/src/main.rs'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(times_read, 1);
    }
}
