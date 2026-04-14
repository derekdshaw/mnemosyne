//! SessionStart hook: prints project summary (do-not-repeat rules, context items,
//! recent bugs) to stdout so Claude Code injects it into the conversation automatically.

use anyhow::Result;
use memory_common::db;
use rusqlite::Connection;

use crate::HookInput;

pub fn run(conn: &Connection, input: &HookInput) -> Result<()> {
    let project = input.project();

    // Build project filter params — empty string means "no filter" for Param-style queries,
    // but here we use Option<&str> with IS NULL for the nullable pattern.
    let project_ref = project.as_deref();

    // Do-not-repeat rules (global + project-scoped)
    let mut stmt = conn.prepare(
        "SELECT rule, reason, file_path, project FROM do_not_repeat \
         WHERE (project IS NULL OR ?1 IS NULL OR project = ?1) \
         ORDER BY project IS NOT NULL, created_at DESC",
    )?;
    let rules: Vec<(String, Option<String>, Option<String>, Option<String>)> = stmt
        .query_map(rusqlite::params![project_ref], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Context items (global + project-scoped)
    let mut stmt = conn.prepare(
        "SELECT category, content, project FROM context_items \
         WHERE (project IS NULL OR ?1 IS NULL OR project = ?1) \
         ORDER BY category, created_at DESC LIMIT 20",
    )?;
    let context_items: Vec<(String, String, Option<String>)> = stmt
        .query_map(rusqlite::params![project_ref], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Recent bugs (global + project-scoped)
    let mut stmt = conn.prepare(
        "SELECT error_message, root_cause, fix_description, file_path, project FROM bugs \
         WHERE (project IS NULL OR ?1 IS NULL OR project = ?1) \
         ORDER BY created_at DESC LIMIT 10",
    )?;
    let bugs: Vec<(String, Option<String>, String, Option<String>, Option<String>)> = stmt
        .query_map(rusqlite::params![project_ref], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Session stats
    let (total_sessions, total_input, total_output): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(total_input_tokens), 0), \
         COALESCE(SUM(total_output_tokens), 0) FROM sessions \
         WHERE (?1 IS NULL OR project = ?1)",
        rusqlite::params![project_ref],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    // Only print if there's something to show
    if rules.is_empty() && context_items.is_empty() && bugs.is_empty() && total_sessions == 0 {
        return Ok(());
    }

    let proj_label = project_ref.unwrap_or("all projects");
    println!("--- Mnemosyne Session Briefing ({proj_label}) ---");

    if !rules.is_empty() {
        println!("\n## Do-Not-Repeat Rules");
        for (rule, reason, file_path, proj) in &rules {
            let scope = format_scope(proj.as_deref(), file_path.as_deref());
            print!("- {rule}");
            if let Some(r) = reason {
                print!(" — Why: {r}");
            }
            if !scope.is_empty() {
                print!(" [{scope}]");
            }
            println!();
        }
    }

    if !context_items.is_empty() {
        println!("\n## Saved Context");
        let mut current_category = String::new();
        for (category, content, proj) in &context_items {
            if *category != current_category {
                println!("### {category}");
                current_category.clone_from(category);
            }
            let scope = if proj.is_none() { " [global]" } else { "" };
            let content_short = db::truncate_utf8(content, 200);
            println!("- {content_short}{scope}");
        }
    }

    if !bugs.is_empty() {
        println!("\n## Recent Bugs");
        for (error_msg, root_cause, fix_desc, file_path, _proj) in &bugs {
            let error_short = db::truncate_utf8(error_msg, 100);
            let fix_short = db::truncate_utf8(fix_desc, 100);
            print!("- {error_short} → Fix: {fix_short}");
            if let Some(rc) = root_cause {
                let rc_short = db::truncate_utf8(rc, 80);
                print!(" (cause: {rc_short})");
            }
            if let Some(fp) = file_path {
                print!(" [{fp}]");
            }
            println!();
        }
    }

    println!("\n## Stats: {total_sessions} sessions, {total_input} input tokens, {total_output} output tokens");
    println!("---");

    Ok(())
}

fn format_scope(project: Option<&str>, file_path: Option<&str>) -> String {
    match (project, file_path) {
        (None, None) => "global".to_string(),
        (None, Some(f)) => format!("global, file={f}"),
        (Some(p), None) => format!("project={p}"),
        (Some(p), Some(f)) => format!("project={p}, file={f}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookInput;
    fn make_input(cwd: &str) -> HookInput {
        HookInput {
            session_id: Some("test-session".into()),
            cwd: Some(cwd.into()),
            tool_name: None,
            tool_input: None,
            tool_response: None,
        }
    }

    #[test]
    fn test_session_start_empty_db() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        // No data — should succeed silently
        assert!(run(&conn, &make_input("/Users/me/r/myproject")).is_ok());
    }

    #[test]
    fn test_session_start_with_global_rules() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        conn.execute(
            "INSERT INTO do_not_repeat (project, rule, reason, created_at) \
             VALUES (NULL, 'always do X', 'because Y', datetime('now'))",
            [],
        )
        .unwrap();
        // Global rule should appear for any project
        assert!(run(&conn, &make_input("/Users/me/r/myproject")).is_ok());
    }

    #[test]
    fn test_session_start_with_project_data() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        // Global rule
        conn.execute(
            "INSERT INTO do_not_repeat (project, rule, created_at) \
             VALUES (NULL, 'global rule', datetime('now'))",
            [],
        )
        .unwrap();
        // Project-scoped rule
        conn.execute(
            "INSERT INTO do_not_repeat (project, rule, created_at) \
             VALUES ('myproject', 'project rule', datetime('now'))",
            [],
        )
        .unwrap();
        // Context item
        conn.execute(
            "INSERT INTO context_items (project, category, content, created_at) \
             VALUES ('myproject', 'architecture', 'use X pattern', datetime('now'))",
            [],
        )
        .unwrap();
        // Bug
        conn.execute(
            "INSERT INTO bugs (project, error_message, fix_description, created_at) \
             VALUES ('myproject', 'panic at line 42', 'add bounds check', datetime('now'))",
            [],
        )
        .unwrap();
        assert!(run(&conn, &make_input("/Users/me/r/myproject")).is_ok());
    }
}
