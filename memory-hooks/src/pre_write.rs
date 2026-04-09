use anyhow::Result;
use memory_common::db::normalize_path;
use rusqlite::Connection;

use crate::HookInput;

pub fn run(conn: &Connection, input: &HookInput) -> Result<()> {
    let file_path = match input.file_path() {
        Some(fp) => fp,
        None => return Ok(()),
    };
    let project = input.project();
    let filename = file_path.rsplit('/').next().unwrap_or(&file_path);

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
        let error_short = if error_msg.len() > 100 {
            format!("{}...", &error_msg[..100])
        } else {
            error_msg.clone()
        };
        let fix_short = if fix_desc.len() > 100 {
            format!("{}...", &fix_desc[..100])
        } else {
            fix_desc.clone()
        };
        eprintln!("\u{1f41b} Known bug on {filename}: {error_short} \u{2014} Fix: {fix_short}");
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
            eprintln!("\u{1f6ab} Do not: {rule}{reason_str}");
        }
    }

    Ok(())
}
