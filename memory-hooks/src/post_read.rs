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

    // Update anatomy stats
    if let Some(ref proj) = project {
        let updated = conn.execute(
            "UPDATE file_anatomy SET times_read = times_read + 1 \
             WHERE project = ?1 AND file_path = ?2",
            rusqlite::params![proj, file_path],
        )?;

        // If no anatomy entry exists, create one with basic info
        if updated == 0 {
            let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
            let description = format!("File: {filename}");
            conn.execute(
                "INSERT OR IGNORE INTO file_anatomy \
                 (project, file_path, description, estimated_tokens, last_scanned, times_read, times_written) \
                 VALUES (?1, ?2, ?3, ?4, datetime('now'), 1, 0)",
                rusqlite::params![proj, file_path, description, token_estimate],
            )?;
        }
    }

    Ok(())
}
