use anyhow::Result;
use rusqlite::Connection;

use crate::HookInput;

pub fn run(conn: &Connection, input: &HookInput) -> Result<()> {
    let file_path = match input.file_path() {
        Some(fp) => fp,
        None => return Ok(()),
    };
    let project = match input.project() {
        Some(p) => p,
        None => return Ok(()),
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

    Ok(())
}
