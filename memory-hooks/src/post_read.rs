//! Post-read hook: records the read, extracts file anatomy from content,
//! and updates the `file_anatomy` and `session_reads` tables.

use anyhow::Result;
use memory_common::anatomy;
use rusqlite::Connection;

use crate::HookInput;

pub fn run(conn: &Connection, input: &HookInput) -> Result<()> {
    let file_path = match input.file_path() {
        Some(fp) => fp,
        None => return Ok(()),
    };
    let session_id = input.session_id.as_deref().unwrap_or("");
    let project = input.project();

    // Get file content from tool response (available in PostToolUse hooks)
    let content_str = input
        .tool_response
        .as_ref()
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_str());

    // Estimate tokens from response content
    let token_estimate = content_str.map(|s| (s.len() as f64 / 3.5) as i64);

    // Record this read in session_reads
    conn.execute(
        "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
         VALUES (?1, ?2, datetime('now'), ?3)",
        rusqlite::params![session_id, file_path, token_estimate],
    )?;

    // Extract a meaningful description from file content so the pre-read hook
    // can show Claude useful context — helping it decide whether to re-read
    // the file or rely on the summary.
    if let Some(ref proj) = project {
        let description = match content_str {
            Some(content) => anatomy::extract_description(content, &file_path),
            None => {
                let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
                format!("File: {filename}")
            }
        };

        // UPSERT: on first read, insert anatomy. On subsequent reads, update
        // the description (file content may have changed) and increment count.
        conn.execute(
            "INSERT INTO file_anatomy (project, file_path, description, estimated_tokens, last_scanned, times_read, times_written) \
             VALUES (?1, ?2, ?3, ?4, datetime('now'), 1, 0) \
             ON CONFLICT(project, file_path) DO UPDATE SET \
             times_read = times_read + 1, \
             description = ?3, \
             estimated_tokens = ?4, \
             last_scanned = datetime('now')",
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
            cwd: Some("/home/user/myproject".into()),
            tool_name: Some("Read".into()),
            tool_input: Some(json!({"file_path": "/home/user/myproject/src/main.rs"})),
            tool_response: Some(
                json!({"content": "//! Main entry point\npub fn main() {\n    println!(\"hello\");\n}\n"}),
            ),
        }
    }

    #[test]
    fn test_post_read_inserts_session_read() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        run(&conn, &make_input()).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM session_reads WHERE session_id = 'test-session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_post_read_creates_rich_anatomy() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        run(&conn, &make_input()).unwrap();

        let description: String = conn
            .query_row(
                "SELECT description FROM file_anatomy WHERE project = 'myproject'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Should contain extracted content, not just "File: main.rs"
        assert!(
            description.contains("Main entry point"),
            "got: {description}"
        );
        assert!(description.contains("pub fn main"), "got: {description}");
    }

    #[test]
    fn test_post_read_updates_description_on_reread() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        run(&conn, &make_input()).unwrap();

        // Second read with different content
        let input2 = HookInput {
            session_id: Some("test-session".into()),
            cwd: Some("/home/user/myproject".into()),
            tool_name: Some("Read".into()),
            tool_input: Some(json!({"file_path": "/home/user/myproject/src/main.rs"})),
            tool_response: Some(json!({"content": "//! Updated module\npub struct Config {}\n"})),
        };
        run(&conn, &input2).unwrap();

        let (desc, times_read): (String, i64) = conn
            .query_row(
                "SELECT description, times_read FROM file_anatomy WHERE project = 'myproject'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(times_read, 2);
        assert!(
            desc.contains("Updated module"),
            "description should reflect latest content, got: {desc}"
        );
    }
}
