//! Pre-read hook: shows file anatomy and warns on repeated reads within a session.

use anyhow::Result;
use rusqlite::Connection;

use crate::HookInput;
use memory_common::anatomy::Symbol;

pub fn run(conn: &Connection, input: &HookInput) -> Result<()> {
    let file_path = match input.file_path() {
        Some(fp) => fp,
        None => return Ok(()),
    };
    let project = input.project();
    let session_id = input.session_id.as_deref().unwrap_or("");

    if let Some(ref proj) = project {
        let anatomy: Option<(String, Option<i64>, Option<String>)> = conn
            .query_row(
                "SELECT description, estimated_tokens, top_symbols_json FROM file_anatomy \
                 WHERE project = ?1 AND file_path = ?2",
                rusqlite::params![proj, file_path],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .ok();

        if let Some((description, tokens, symbols_json)) = anatomy {
            let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
            let token_info = tokens
                .map(|t| format!(" (~{t} tokens)"))
                .unwrap_or_default();
            eprintln!("\u{1f4c4} {filename}: {description}{token_info}");

            if let Some(json) = symbols_json {
                if let Some(line) = format_symbols(&json) {
                    eprintln!("   Symbols: {line}");
                }
            }
        }
    }

    // Check for repeated reads in this session. Harden the warning copy so
    // Claude treats it as directive, not advisory — the anatomy above is the
    // reason to skip.
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
        let token_info = tokens.map(|t| format!(" ~{t} tokens")).unwrap_or_default();
        let ago = humanize_ago(&read_at);
        eprintln!(
            "\u{1f6ab} DUPLICATE READ \u{2014} already read {ago} in this session.{token_info} \
             Anatomy above shows available context; only re-read if anatomy is insufficient."
        );
    }

    Ok(())
}

/// Format up to 8 symbols from the JSON-encoded `top_symbols_json` column as
/// `name@line` entries. Returns `None` if the JSON is empty or malformed.
fn format_symbols(json: &str) -> Option<String> {
    let parsed: Vec<Symbol> = serde_json::from_str(json).ok()?;
    if parsed.is_empty() {
        return None;
    }
    const MAX: usize = 8;
    let shown: Vec<String> = parsed
        .iter()
        .take(MAX)
        .map(|s| format!("{}@{}", s.name, s.line))
        .collect();
    let extra = parsed.len().saturating_sub(MAX);
    if extra > 0 {
        Some(format!("{} (+{extra} more)", shown.join(", ")))
    } else {
        Some(shown.join(", "))
    }
}

/// Format a UTC timestamp as a short "ago" delta like `"3 min ago"`. Falls
/// back to the raw timestamp if parsing fails.
fn humanize_ago(read_at: &str) -> String {
    use chrono::{NaiveDateTime, Utc};
    let parsed = NaiveDateTime::parse_from_str(read_at, "%Y-%m-%d %H:%M:%S").ok();
    let Some(ts) = parsed else {
        return format!("at {read_at}");
    };
    let now = Utc::now().naive_utc();
    let delta = now.signed_duration_since(ts);
    let secs = delta.num_seconds();
    if secs < 0 {
        // Clock skew or future-dated row — just show the raw time
        return format!("at {read_at}");
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins} min ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookInput;
    use memory_common::anatomy::SymbolKind;
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
        assert!(run(&conn, &input).is_ok());
    }

    #[test]
    fn test_pre_read_repeated_read() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        let input = make_input();
        conn.execute(
            "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
             VALUES ('test-session', 'D:/r/myproject/src/main.rs', datetime('now'), 500)",
            [],
        )
        .unwrap();
        assert!(run(&conn, &input).is_ok());
    }

    #[test]
    fn test_pre_read_with_symbols_json() {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        // Seed an anatomy row with symbols so the SELECT returns them.
        conn.execute(
            "INSERT INTO file_anatomy (project, file_path, description, estimated_tokens, top_symbols_json) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "myproject",
                "D:/r/myproject/src/main.rs",
                "Main entry point",
                100i64,
                r#"[{"kind":"fn","name":"main","line":5}]"#,
            ],
        )
        .unwrap();
        let input = make_input();
        assert!(run(&conn, &input).is_ok());
    }

    #[test]
    fn test_format_symbols_basic() {
        let json =
            r#"[{"kind":"fn","name":"foo","line":10},{"kind":"struct","name":"Bar","line":20}]"#;
        let line = format_symbols(json).unwrap();
        assert_eq!(line, "foo@10, Bar@20");
    }

    #[test]
    fn test_format_symbols_truncation() {
        let syms: Vec<Symbol> = (1..=10)
            .map(|i| Symbol {
                kind: SymbolKind::Fn,
                name: format!("fn{i}"),
                line: i as u32,
            })
            .collect();
        let json = serde_json::to_string(&syms).unwrap();
        let line = format_symbols(&json).unwrap();
        assert!(line.ends_with("(+2 more)"), "got: {line}");
        assert!(line.contains("fn1@1"));
        assert!(line.contains("fn8@8"));
        assert!(!line.contains("fn9@9"));
    }

    #[test]
    fn test_format_symbols_empty_and_malformed() {
        assert!(format_symbols("[]").is_none());
        assert!(format_symbols("not json").is_none());
    }

    #[test]
    fn test_humanize_ago_seconds() {
        use chrono::{Duration, Utc};
        let past = (Utc::now() - Duration::seconds(30))
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let out = humanize_ago(&past);
        assert!(out.ends_with("s ago"), "got: {out}");
    }

    #[test]
    fn test_humanize_ago_minutes() {
        use chrono::{Duration, Utc};
        let past = (Utc::now() - Duration::minutes(5))
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let out = humanize_ago(&past);
        assert!(out.ends_with("min ago"), "got: {out}");
    }

    #[test]
    fn test_humanize_ago_fallback() {
        let out = humanize_ago("not a timestamp");
        assert!(out.starts_with("at "), "got: {out}");
    }
}
