//! Pre-read hook: shows file anatomy and warns on repeated reads within a session.

use anyhow::Result;
use rusqlite::Connection;

use crate::HookInput;

pub fn run(conn: &Connection, input: &HookInput) -> Result<()> {
    let file_path = match input.file_path() {
        Some(fp) => fp,
        None => return Ok(()),
    };
    let project = input.project();
    let session_id = input.session_id.as_deref().unwrap_or("");

    // Check file anatomy
    if let Some(ref proj) = project {
        // M7: Use Option<i64> for estimated_tokens which may be NULL
        let anatomy: Option<(String, Option<i64>)> = conn
            .query_row(
                "SELECT description, estimated_tokens FROM file_anatomy \
                 WHERE project = ?1 AND file_path = ?2",
                rusqlite::params![proj, file_path],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .ok();

        if let Some((description, tokens)) = anatomy {
            let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
            let token_info = tokens.map(|t| format!(" (~{t} tokens)")).unwrap_or_default();
            eprintln!("\u{1f4c4} {filename}: {description}{token_info}");
        }
    }

    // Check for repeated reads in this session
    let last_read: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT read_at, token_estimate FROM session_reads \
             WHERE session_id = ?1 AND file_path = ?2 \
             ORDER BY read_at DESC LIMIT 1",
            rusqlite::params![session_id, file_path],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .ok();

    if let Some((read_at, tokens)) = last_read {
        let token_info = tokens
            .map(|t| format!(" ({t} tokens)"))
            .unwrap_or_default();
        eprintln!(
            "\u{26a0}\u{fe0f} Already read at {read_at}{token_info}. Consider if re-read is needed."
        );
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
            tool_response: None,
        }
    }

    #[test]
    fn test_pre_read_no_anatomy() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        let input = make_input();
        // No anatomy or session_reads data — should return Ok without panic
        assert!(run(&conn, &input).is_ok());
    }

    #[test]
    fn test_pre_read_repeated_read() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        let input = make_input();
        // Seed a prior read for this session + file
        conn.execute(
            "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
             VALUES ('test-session', 'D:/r/myproject/src/main.rs', datetime('now'), 500)",
            [],
        ).unwrap();
        // Should return Ok (stderr warning emitted but we just verify no error)
        assert!(run(&conn, &input).is_ok());
    }
}
