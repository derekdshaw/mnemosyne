//! Pre-write hook: warns about known bugs and do-not-repeat rules for the target file.

use anyhow::Result;
use memory_common::db::{normalize_path, truncate_utf8};
use rusqlite::Connection;
use std::fmt::Write as _;

use crate::HookInput;

pub fn run(conn: &Connection, input: &HookInput) -> Result<usize> {
    let file_path = match input.file_path() {
        Some(fp) => fp,
        None => return Ok(0),
    };
    let project = input.project();
    let filename = file_path.rsplit('/').next().unwrap_or(&file_path);

    let mut buf = String::new();

    // Check bugs database for this file
    let mut stmt = conn.prepare(
        "SELECT error_message, fix_description FROM bugs \
         WHERE file_path = ?1 OR file_path = ?2 \
         ORDER BY created_at DESC LIMIT 3",
    )?;

    // Match on both the full path and just the filename
    let bugs: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![file_path, filename], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (error_msg, fix_desc) in &bugs {
        // C3: Use truncate_utf8 for safe multi-byte truncation
        let error_short = truncate_utf8(error_msg, 100);
        let fix_short = truncate_utf8(fix_desc, 100);
        writeln!(
            buf,
            "\u{1f41b} Known bug on {filename}: {error_short} \u{2014} Fix: {fix_short}"
        )?;
    }

    // Check do-not-repeat rules
    if let Some(ref proj) = project {
        let mut stmt = conn.prepare(
            "SELECT rule, reason FROM do_not_repeat \
             WHERE (project = ?1 OR project IS NULL) \
             AND (file_path = ?2 OR file_path = ?3 OR file_path IS NULL) \
             ORDER BY created_at DESC",
        )?;

        let rules: Vec<(String, Option<String>)> = stmt
            .query_map(
                rusqlite::params![proj, file_path, normalize_path(filename)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )?
            .filter_map(|r| r.ok())
            .collect();

        for (rule, reason) in &rules {
            let reason_str = reason
                .as_deref()
                .map(|r| format!(" \u{2014} Reason: {r}"))
                .unwrap_or_default();
            writeln!(buf, "\u{1f6ab} Do not: {rule}{reason_str}")?;
        }
    }

    let bytes = buf.len();
    eprint!("{buf}");
    Ok(bytes)
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
    fn test_pre_write_no_bugs() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        // No bugs or do_not_repeat rules — should return Ok
        let _ = run(&conn, &make_input()).unwrap();
    }

    #[test]
    fn test_pre_write_matches_bug() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        conn.execute(
            "INSERT INTO bugs (project, error_message, fix_description, file_path, created_at) \
             VALUES ('myproject', 'null pointer', 'add null check', 'D:/r/myproject/src/main.rs', datetime('now'))",
            [],
        ).unwrap();
        let bytes = run(&conn, &make_input()).unwrap();
        assert!(bytes > 0, "matched-bug warning should contribute overhead");
    }

    #[test]
    fn test_pre_write_returns_zero_when_nothing_matches() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        let bytes = run(&conn, &make_input()).unwrap();
        assert_eq!(
            bytes, 0,
            "no bugs and no DNR rules → no stderr emission, no overhead"
        );
    }
}
