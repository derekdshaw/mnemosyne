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
        let anatomy: Option<(String, i64)> = conn
            .query_row(
                "SELECT description, estimated_tokens FROM file_anatomy \
                 WHERE project = ?1 AND file_path = ?2",
                rusqlite::params![proj, file_path],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .ok();

        if let Some((description, tokens)) = anatomy {
            let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
            eprintln!("\u{1f4c4} {filename}: {description} (~{tokens} tokens)");
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
