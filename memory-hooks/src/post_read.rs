//! Post-read hook: records the read, extracts file anatomy from content,
//! and updates the `file_anatomy` and `session_reads` tables.

use anyhow::Result;
use memory_common::anatomy;
use rusqlite::Connection;

use crate::HookInput;

pub fn run(conn: &Connection, input: &HookInput) -> Result<usize> {
    let file_path = match input.file_path() {
        Some(fp) => fp,
        None => return Ok(0),
    };
    let session_id = input.session_id.as_deref().unwrap_or("");
    let project = input.project();

    // Get file content from tool response (available in PostToolUse hooks).
    // Claude Code nests the content at tool_response.file.content
    let content_str = input
        .tool_response
        .as_ref()
        .and_then(|r| r.get("file"))
        .and_then(|f| f.get("content"))
        .and_then(|c| c.as_str());

    // Estimate tokens from response content
    let token_estimate = content_str.map(|s| (s.len() as f64 / 3.5) as i64);

    // Record this read in session_reads
    conn.execute(
        "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
         VALUES (?1, ?2, datetime('now'), ?3)",
        rusqlite::params![session_id, file_path, token_estimate],
    )?;

    // Extract a meaningful description plus symbol-line index from file content
    // so the pre-read hook can show Claude useful context — helping it decide
    // whether to re-read the file or rely on the summary.
    if let Some(ref proj) = project {
        let (description, symbols_json) = match content_str {
            Some(content) => {
                let anatomy = anatomy::extract_anatomy(content, &file_path);
                let json = if anatomy.symbols.is_empty() {
                    None
                } else {
                    serde_json::to_string(&anatomy.symbols).ok()
                };
                (anatomy.description, json)
            }
            None => {
                let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
                (format!("File: {filename}"), None)
            }
        };

        // UPSERT: on first read, insert anatomy. On subsequent reads, update
        // the description (file content may have changed) and increment count.
        conn.execute(
            "INSERT INTO file_anatomy (project, file_path, description, estimated_tokens, last_scanned, times_read, times_written, top_symbols_json) \
             VALUES (?1, ?2, ?3, ?4, datetime('now'), 1, 0, ?5) \
             ON CONFLICT(project, file_path) DO UPDATE SET \
             times_read = times_read + 1, \
             description = ?3, \
             estimated_tokens = ?4, \
             last_scanned = datetime('now'), \
             top_symbols_json = ?5",
            rusqlite::params![proj, file_path, description, token_estimate, symbols_json],
        )?;
    }

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
            cwd: Some("/home/user/myproject".into()),
            tool_name: Some("Read".into()),
            tool_input: Some(json!({"file_path": "/home/user/myproject/src/main.rs"})),
            tool_response: Some(json!({
                "type": "text",
                "file": {
                    "filePath": "/home/user/myproject/src/main.rs",
                    "content": "//! Main entry point\npub fn main() {\n    println!(\"hello\");\n}\n",
                    "numLines": 4,
                    "startLine": 1,
                    "totalLines": 4
                }
            })),
        }
    }

    #[test]
    fn test_post_read_always_returns_zero_overhead() {
        // post_read has no user-visible output — it only writes to the DB.
        let conn = memory_common::db::open_db_in_memory().unwrap();
        let bytes = run(&conn, &make_input()).unwrap();
        assert_eq!(
            bytes, 0,
            "post_read must not contribute overhead (no stdout/stderr)"
        );
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

        // Verify token estimate is populated
        let token_est: Option<i64> = conn
            .query_row(
                "SELECT token_estimate FROM session_reads WHERE session_id = 'test-session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(token_est.is_some(), "token_estimate should be populated");
        assert!(token_est.unwrap() > 0, "token_estimate should be > 0");
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
    fn test_post_read_persists_symbols() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        run(&conn, &make_input()).unwrap();

        let symbols_json: Option<String> = conn
            .query_row(
                "SELECT top_symbols_json FROM file_anatomy WHERE project = 'myproject'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let json = symbols_json.expect("top_symbols_json should be populated for a .rs file");
        assert!(json.contains("\"name\":\"main\""), "got: {json}");
        assert!(json.contains("\"kind\":\"fn\""), "got: {json}");
        assert!(json.contains("\"line\":2"), "got: {json}");
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
            tool_response: Some(json!({
                "type": "text",
                "file": {
                    "filePath": "/home/user/myproject/src/main.rs",
                    "content": "//! Updated module\npub struct Config {}\n",
                    "numLines": 2,
                    "startLine": 1,
                    "totalLines": 2
                }
            })),
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
