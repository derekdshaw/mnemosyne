use anyhow::{Context, Result};
use clap::Parser;
use memory_common::db::{self, normalize_path, project_from_cwd};
use memory_common::jsonl::{self, ContentBlock, Record};
use rusqlite::Connection;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Parser)]
#[command(name = "session-ingester", about = "Ingest Claude Code JSONL transcripts into Mnemosyne SQLite")]
struct Cli {
    /// Path to the .claude directory
    #[arg(long, default_value_t = default_claude_dir())]
    claude_dir: String,

    /// Print verbose output
    #[arg(long)]
    verbose: bool,

    /// Ingest a specific session ID immediately (bypasses active-session mtime guard).
    /// Used by SessionEnd hook to ingest the just-finished session.
    #[arg(long)]
    session_id: Option<String>,

    /// Read session_id from stdin JSON (for use as a SessionEnd hook).
    /// Claude Code pipes hook input JSON to stdin containing session_id.
    #[arg(long)]
    from_stdin: bool,
}

fn default_claude_dir() -> String {
    dirs::home_dir()
        .map(|h| h.join(".claude").to_string_lossy().to_string())
        .unwrap_or_else(|| ".claude".to_string())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("session-ingester error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut cli = Cli::parse();

    // If --from-stdin, read session_id from the hook input JSON on stdin
    if cli.from_stdin {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&input) {
            if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                cli.session_id = Some(sid.to_string());
            }
        }
    }

    if cli.verbose {
        tracing_subscriber::fmt()
            .with_env_filter("info")
            .with_writer(std::io::stderr)
            .init();
    }

    let conn = db::open_db().context("failed to open database")?;

    let projects_dir = PathBuf::from(&cli.claude_dir).join("projects");
    if !projects_dir.exists() {
        tracing::info!("no projects directory found at {}", projects_dir.display());
        return Ok(());
    }

    let mut total_files = 0;
    let mut total_messages = 0;

    // Scan all project subdirectories for .jsonl files
    for entry in fs::read_dir(&projects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let project_dir = entry.path();

        for file_entry in fs::read_dir(&project_dir)? {
            let file_entry = file_entry?;
            let path = file_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            // If --session-id is set, only ingest that specific file
            let force = cli.session_id.as_ref().map_or(false, |sid| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map_or(false, |stem| stem == sid)
            });
            if cli.session_id.is_some() && !force {
                continue;
            }

            match ingest_file(&conn, &path, cli.verbose, force) {
                Ok(IngestResult::Skipped) => {
                    if cli.verbose {
                        tracing::info!("skipped (unchanged): {}", path.display());
                    }
                }
                Ok(IngestResult::SkippedActive) => {
                    if cli.verbose {
                        tracing::info!("skipped (active session): {}", path.display());
                    }
                }
                Ok(IngestResult::Ingested { messages }) => {
                    total_files += 1;
                    total_messages += messages;
                    if cli.verbose {
                        tracing::info!(
                            "ingested {} messages from {}",
                            messages,
                            path.display()
                        );
                    }
                }
                Err(e) => {
                    eprintln!("warning: failed to ingest {}: {e:#}", path.display());
                }
            }
        }
    }

    if total_files > 0 || cli.verbose {
        eprintln!(
            "mnemosyne: ingested {total_messages} messages from {total_files} transcript(s)"
        );
    }

    Ok(())
}

enum IngestResult {
    Skipped,
    SkippedActive,
    Ingested { messages: usize },
}

fn ingest_file(conn: &Connection, path: &PathBuf, verbose: bool, force: bool) -> Result<IngestResult> {
    let metadata = fs::metadata(path)?;
    let file_size = metadata.len() as i64;
    let file_mtime = metadata
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    let mtime_str = file_mtime.to_string();

    // Skip files modified in the last 60 seconds (likely active session)
    // Unless force=true (SessionEnd hook for a specific session)
    if !force {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        if now - file_mtime < 60 {
            return Ok(IngestResult::SkippedActive);
        }
    }

    let file_path_str = normalize_path(&path.to_string_lossy());

    // Check ingestion log
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT file_size, file_mtime FROM ingestion_log WHERE file_path = ?1",
            [&file_path_str],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    if let Some((prev_size, prev_mtime)) = &existing {
        if *prev_size == file_size && *prev_mtime == mtime_str {
            return Ok(IngestResult::Skipped);
        }
    }

    // Determine if we need full or incremental ingestion
    let start_offset = match &existing {
        Some((prev_size, _)) if *prev_size < file_size => *prev_size as u64,
        Some(_) => 0, // File was rewritten, re-ingest from scratch
        None => 0,
    };

    // If re-ingesting from scratch and we have prior data, clean it up
    if start_offset == 0 && existing.is_some() {
        // We'd need to delete old records for this file, but since records
        // are keyed by session_id/uuid (not file), just re-ingest and let
        // INSERT OR IGNORE handle duplicates.
    }

    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);

    let mut line_count = 0i64;
    let mut message_count = 0usize;
    let mut bytes_read = 0u64;

    // Track session metadata from first user message
    let mut session_meta: std::collections::HashMap<String, SessionMeta> =
        std::collections::HashMap::new();

    let tx = conn.unchecked_transaction()?;

    // Defer foreign key checks until commit so we can insert messages before sessions
    tx.execute_batch("PRAGMA defer_foreign_keys = ON;")?;

    for line_result in reader.lines() {
        let line = line_result?;
        bytes_read += line.len() as u64 + 1; // +1 for newline
        line_count += 1;

        // Skip lines we've already ingested
        if bytes_read <= start_offset {
            continue;
        }

        let record = match jsonl::parse_line(&line) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => {
                if verbose {
                    tracing::warn!("line {line_count}: parse error: {e}");
                }
                continue;
            }
        };

        match record {
            Record::UserMessage {
                uuid,
                session_id,
                parent_uuid,
                cwd,
                git_branch,
                timestamp,
                content,
            } => {
                // Track session metadata
                if !session_meta.contains_key(&session_id) {
                    let project = cwd.as_deref().map(project_from_cwd);
                    session_meta.insert(
                        session_id.clone(),
                        SessionMeta {
                            cwd: cwd.clone(),
                            git_branch: git_branch.clone(),
                            project,
                            first_timestamp: timestamp.clone(),
                            last_timestamp: timestamp.clone(),
                        },
                    );
                } else if let Some(meta) = session_meta.get_mut(&session_id) {
                    meta.last_timestamp = timestamp.clone();
                }

                // Insert message
                tx.execute(
                    "INSERT OR IGNORE INTO messages (uuid, session_id, parent_uuid, role, content_type, content, timestamp) \
                     VALUES (?1, ?2, ?3, 'user', 'text', ?4, ?5)",
                    rusqlite::params![uuid, session_id, parent_uuid, content, timestamp],
                )?;

                // Insert into FTS
                tx.execute(
                    "INSERT OR IGNORE INTO messages_fts (uuid, session_id, content) VALUES (?1, ?2, ?3)",
                    rusqlite::params![uuid, session_id, content],
                )?;

                message_count += 1;
            }

            Record::ToolResult {
                uuid,
                session_id,
                parent_uuid,
                timestamp,
                results,
            } => {
                if let Some(meta) = session_meta.get_mut(&session_id) {
                    meta.last_timestamp = timestamp.clone();
                }

                // Store tool results as messages (content is the result text, truncated)
                for result in &results {
                    let content_truncated = if result.content.len() > 500 {
                        format!("{}...", &result.content[..500])
                    } else {
                        result.content.clone()
                    };

                    tx.execute(
                        "INSERT OR IGNORE INTO messages (uuid, session_id, parent_uuid, role, content_type, content, tool_name, timestamp) \
                         VALUES (?1, ?2, ?3, 'user', 'tool_result', ?4, ?5, ?6)",
                        rusqlite::params![
                            uuid,
                            session_id,
                            parent_uuid,
                            content_truncated,
                            result.tool_use_id,
                            timestamp,
                        ],
                    )?;
                }
                message_count += 1;
            }

            Record::AssistantMessage {
                uuid,
                session_id,
                parent_uuid,
                timestamp,
                model,
                content_blocks,
                usage,
            } => {
                if let Some(meta) = session_meta.get_mut(&session_id) {
                    meta.last_timestamp = timestamp.clone();
                }

                // Collect text content for the message
                let mut text_parts: Vec<String> = Vec::new();
                let mut tool_uses: Vec<(String, String, serde_json::Value)> = Vec::new();

                for block in &content_blocks {
                    match block {
                        ContentBlock::Text(t) => text_parts.push(t.clone()),
                        ContentBlock::Thinking(t) => text_parts.push(format!("[thinking] {t}")),
                        ContentBlock::ToolUse { name, id, input } => {
                            tool_uses.push((name.clone(), id.clone(), input.clone()));
                        }
                    }
                }

                let content_text = if text_parts.is_empty() {
                    None
                } else {
                    Some(text_parts.join("\n"))
                };

                let content_type = if !tool_uses.is_empty() {
                    "tool_use"
                } else {
                    "text"
                };

                tx.execute(
                    "INSERT OR IGNORE INTO messages (uuid, session_id, parent_uuid, role, content_type, content, timestamp, model) \
                     VALUES (?1, ?2, ?3, 'assistant', ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        uuid,
                        session_id,
                        parent_uuid,
                        content_type,
                        content_text,
                        timestamp,
                        model,
                    ],
                )?;

                // Insert text into FTS
                if let Some(ref text) = content_text {
                    tx.execute(
                        "INSERT OR IGNORE INTO messages_fts (uuid, session_id, content) VALUES (?1, ?2, ?3)",
                        rusqlite::params![uuid, session_id, text],
                    )?;
                }

                // Insert tool calls
                for (tool_name, _tool_id, tool_input) in &tool_uses {
                    let file_path = jsonl::extract_file_path(tool_name, tool_input)
                        .map(|p| normalize_path(&p));
                    let input_summary =
                        jsonl::extract_tool_input_summary(tool_name, tool_input);

                    tx.execute(
                        "INSERT INTO tool_calls (message_uuid, session_id, tool_name, tool_input_summary, file_path, timestamp) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            uuid,
                            session_id,
                            tool_name,
                            input_summary,
                            file_path,
                            timestamp,
                        ],
                    )?;
                }

                // Insert token usage
                if let Some(usage) = usage {
                    tx.execute(
                        "INSERT OR IGNORE INTO token_usage (message_uuid, session_id, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            uuid,
                            session_id,
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cache_read_tokens,
                            usage.cache_creation_tokens,
                        ],
                    )?;
                }

                message_count += 1;
            }

            Record::Skip => {}
        }
    }

    // Upsert sessions from collected metadata
    for (session_id, meta) in &session_meta {
        tx.execute(
            "INSERT INTO sessions (session_id, project, start_time, end_time, cwd, git_branch) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(session_id) DO UPDATE SET \
             end_time = COALESCE(?4, end_time), \
             project = COALESCE(?2, project), \
             cwd = COALESCE(?5, cwd), \
             git_branch = COALESCE(?6, git_branch)",
            rusqlite::params![
                session_id,
                meta.project,
                meta.first_timestamp,
                meta.last_timestamp,
                meta.cwd.as_deref().map(normalize_path),
                meta.git_branch,
            ],
        )?;
    }

    // Update session aggregates
    for session_id in session_meta.keys() {
        tx.execute(
            "UPDATE sessions SET \
             message_count = (SELECT COUNT(*) FROM messages WHERE session_id = ?1), \
             total_input_tokens = COALESCE((SELECT SUM(input_tokens) FROM token_usage WHERE session_id = ?1), 0), \
             total_output_tokens = COALESCE((SELECT SUM(output_tokens) FROM token_usage WHERE session_id = ?1), 0) \
             WHERE session_id = ?1",
            [session_id],
        )?;
    }

    // Update ingestion log
    tx.execute(
        "INSERT OR REPLACE INTO ingestion_log (file_path, ingested_at, line_count, file_size, file_mtime) \
         VALUES (?1, datetime('now'), ?2, ?3, ?4)",
        rusqlite::params![file_path_str, line_count, file_size, mtime_str],
    )?;

    tx.commit()?;

    Ok(IngestResult::Ingested {
        messages: message_count,
    })
}

struct SessionMeta {
    cwd: Option<String>,
    git_branch: Option<String>,
    project: Option<String>,
    first_timestamp: Option<String>,
    last_timestamp: Option<String>,
}
