//! MCP server for the Mnemosyne session memory system.
//!
//! Exposes 11 tools over stdio JSON-RPC for searching past sessions,
//! saving project context, logging bugs, and managing do-not-repeat rules.
//! Claude Code manages the server lifecycle — spawning it at session start
//! and communicating via newline-delimited JSON-RPC.

mod tools;

use anyhow::Result;
use memory_common::db::{self, truncate_utf8};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ServerInfo;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use rusqlite::types::{ToSql, ToSqlOutput};
use rusqlite::Connection;
use std::sync::Mutex;
use tools::*;

/// Stack-allocated SQL parameter enum. Avoids `Box<dyn ToSql>` heap allocations
/// and the double-vec indirection (`Vec<Box<dyn ToSql>>` + `Vec<&dyn ToSql>`)
/// that was previously needed for dynamic query building with optional filters.
///
/// MCP tool queries build SQL dynamically based on which optional parameters the
/// caller provides (project, days, file_path, tags, etc.). This requires a
/// heterogeneous parameter list. Rather than boxing each parameter on the heap,
/// this enum wraps the three types we actually use — the match dispatches
/// statically, and `Vec<Param>` is a single contiguous allocation.
#[allow(dead_code)] // Int reserved for future queries (e.g., get_token_stats)
enum Param {
    Text(String),
    Int(i64),
}

impl ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        match self {
            Param::Text(s) => s.to_sql(),
            Param::Int(i) => i.to_sql(),
        }
    }
}

/// S1: Escape FTS5 query to prevent operator abuse. Wraps in double quotes as a phrase.
fn escape_fts_query(q: &str) -> String {
    format!("\"{}\"", q.replace('"', "\"\""))
}

/// S2: Clamp limit to a safe range.
fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(10).clamp(1, 100)
}

/// S2: Clamp days to a safe range.
fn clamp_days(days: Option<i64>) -> i64 {
    days.unwrap_or(7).clamp(1, 365)
}

struct MnemosyneServer {
    // S8: If a tool handler panics, Mutex becomes poisoned. All subsequent .lock() calls
    // return PoisonError, which we map to MCP error responses — the server degrades
    // gracefully rather than crashing, but DB operations stop working.
    db: Mutex<Connection>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[tool_router]
impl MnemosyneServer {
    /// Search across all past session messages using full-text search.
    #[tool(name = "search_sessions")]
    fn search_sessions(
        &self,
        Parameters(input): Parameters<SearchSessionsInput>,
    ) -> Result<Json<SessionResultList>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let limit = clamp_limit(input.limit);

        // FTS5's snippet() requires the FTS table to be directly in the FROM clause
        // with an active MATCH context. GROUP BY breaks this context, causing
        // "unable to use function snippet in the requested context" errors.
        //
        // Instead of GROUP BY, we return one row per matching message (which may
        // yield multiple rows per session) and dedup by session_id in Rust.
        // This keeps snippet() working with a single efficient FTS scan.
        let mut sql = String::from(
            "SELECT m.session_id, s.project, s.start_time, s.message_count, \
             s.total_input_tokens, s.total_output_tokens, \
             snippet(messages_fts, -1, '>>>', '<<<', '...', 64) as excerpt \
             FROM messages_fts \
             JOIN messages m ON messages_fts.uuid = m.uuid \
             JOIN sessions s ON m.session_id = s.session_id \
             WHERE messages_fts MATCH ?1",
        );
        let mut params: Vec<Param> = vec![Param::Text(escape_fts_query(&input.query))];

        if let Some(ref project) = input.project {
            sql.push_str(" AND s.project = ?2");
            params.push(Param::Text(project.clone()));
        }

        sql.push_str(" ORDER BY s.start_time DESC");
        // Over-fetch to account for dedup — we'll trim to limit after
        sql.push_str(&format!(" LIMIT {}", limit * 5));

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let all_rows: Vec<SessionResult> = stmt
            .query_map(rusqlite::params_from_iter(&params), |row| {
                Ok(SessionResult {
                    session_id: row.get(0)?,
                    project: row.get(1)?,
                    start_time: row.get(2)?,
                    message_count: row.get(3)?,
                    total_input_tokens: row.get(4)?,
                    total_output_tokens: row.get(5)?,
                    matching_excerpt: row.get(6)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        // Dedup: keep the first (best) match per session
        let mut seen = std::collections::HashSet::new();
        let results: Vec<SessionResult> = all_rows
            .into_iter()
            .filter(|r| seen.insert(r.session_id.clone()))
            .take(limit as usize)
            .collect();

        Ok(Json(SessionResultList { results }))
    }

    /// List recent sessions, optionally filtered by project.
    #[tool(name = "get_recent_sessions")]
    fn get_recent_sessions(
        &self,
        Parameters(input): Parameters<GetRecentSessionsInput>,
    ) -> Result<Json<SessionResultList>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let days = clamp_days(input.days);

        let mut sql = String::from(
            "SELECT session_id, project, start_time, message_count, \
             total_input_tokens, total_output_tokens \
             FROM sessions WHERE start_time >= datetime('now', ?1)",
        );
        let mut params: Vec<Param> = vec![Param::Text(format!("-{days} days"))];

        if let Some(ref project) = input.project {
            sql.push_str(" AND project = ?2");
            params.push(Param::Text(project.clone()));
        }
        sql.push_str(" ORDER BY start_time DESC LIMIT 50");

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(rusqlite::params_from_iter(&params), |row| {
                Ok(SessionResult {
                    session_id: row.get(0)?,
                    project: row.get(1)?,
                    start_time: row.get(2)?,
                    message_count: row.get(3)?,
                    total_input_tokens: row.get(4)?,
                    total_output_tokens: row.get(5)?,
                    matching_excerpt: None,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(Json(SessionResultList { results }))
    }

    /// Get detailed information about a specific session.
    #[tool(name = "get_session_detail")]
    fn get_session_detail(
        &self,
        Parameters(input): Parameters<GetSessionDetailInput>,
    ) -> Result<Json<SessionDetail>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let session: SessionDetail = conn
            .query_row(
                "SELECT session_id, project, start_time, end_time, cwd, git_branch, \
                 message_count, total_input_tokens, total_output_tokens \
                 FROM sessions WHERE session_id = ?1",
                [&input.session_id],
                |row| {
                    Ok(SessionDetail {
                        session_id: row.get(0)?,
                        project: row.get(1)?,
                        start_time: row.get(2)?,
                        end_time: row.get(3)?,
                        cwd: row.get(4)?,
                        git_branch: row.get(5)?,
                        message_count: row.get(6)?,
                        total_input_tokens: row.get(7)?,
                        total_output_tokens: row.get(8)?,
                        first_user_message: None,
                        last_user_message: None,
                        tool_summary: Vec::new(),
                    })
                },
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Get first and last user messages
        let first_msg: Option<String> = conn
            .query_row(
                "SELECT content FROM messages WHERE session_id = ?1 AND role = 'user' AND content_type = 'text' \
                 ORDER BY timestamp ASC LIMIT 1",
                [&input.session_id],
                |row| row.get(0),
            )
            .ok();

        let last_msg: Option<String> = conn
            .query_row(
                "SELECT content FROM messages WHERE session_id = ?1 AND role = 'user' AND content_type = 'text' \
                 ORDER BY timestamp DESC LIMIT 1",
                [&input.session_id],
                |row| row.get(0),
            )
            .ok();

        // Get tool call summary
        let mut stmt = conn
            .prepare(
                "SELECT tool_name, COUNT(*) as cnt FROM tool_calls WHERE session_id = ?1 \
                 GROUP BY tool_name ORDER BY cnt DESC",
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let tool_summary: Vec<ToolSummaryEntry> = stmt
            .query_map([&input.session_id], |row| {
                Ok(ToolSummaryEntry {
                    tool_name: row.get(0)?,
                    count: row.get(1)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(Json(SessionDetail {
            first_user_message: first_msg,
            last_user_message: last_msg,
            tool_summary,
            ..session
        }))
    }

    /// Get tool calls that touched a specific file, with session context.
    #[tool(name = "get_file_history")]
    fn get_file_history(
        &self,
        Parameters(input): Parameters<GetFileHistoryInput>,
    ) -> Result<Json<FileHistoryList>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Static query with 3 fixed params — empty string means "no filter".
        let file_path_pattern = input
            .file_path
            .as_ref()
            .map(|fp| format!("%{fp}%"))
            .unwrap_or_default();
        let project = input.project.clone().unwrap_or_default();
        let days_param = input
            .days
            .map(|d| format!("-{} days", d.clamp(1, 365)))
            .unwrap_or_default();
        let params = [
            Param::Text(file_path_pattern),
            Param::Text(project),
            Param::Text(days_param),
        ];

        let sql = "SELECT tc.session_id, s.project, tc.tool_name, tc.tool_input_summary, \
             tc.file_path, tc.timestamp \
             FROM tool_calls tc \
             JOIN sessions s ON tc.session_id = s.session_id \
             WHERE (?1 = '' OR tc.file_path LIKE ?1) \
             AND (?2 = '' OR s.project = ?2) \
             AND (?3 = '' OR tc.timestamp >= datetime('now', ?3)) \
             ORDER BY tc.timestamp DESC LIMIT 50";

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(rusqlite::params_from_iter(&params), |row| {
                Ok(FileHistoryEntry {
                    session_id: row.get(0)?,
                    project: row.get(1)?,
                    tool_name: row.get(2)?,
                    tool_input_summary: row.get(3)?,
                    file_path: row.get(4)?,
                    timestamp: row.get(5)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(Json(FileHistoryList { results }))
    }

    /// Save a context item (decision, convention, architecture note).
    #[tool(name = "save_context")]
    fn save_context(
        &self,
        Parameters(input): Parameters<SaveContextInput>,
    ) -> Result<Json<SimpleResult>, rmcp::ErrorData> {
        // S4: Validate and truncate input
        if input.content.is_empty() {
            return Err(rmcp::ErrorData::invalid_request(
                "content must not be empty",
                None,
            ));
        }
        let content = truncate_utf8(&input.content, 10_000);

        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        conn.execute(
            "INSERT INTO context_items (project, category, content, created_at) \
             VALUES (?1, ?2, ?3, datetime('now'))",
            rusqlite::params![input.project, input.category, content],
        )
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO context_fts (item_id, project, category, content) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id.to_string(), input.project, input.category, content],
        )
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(Json(SimpleResult {
            success: true,
            message: format!("Context saved (id: {id})"),
        }))
    }

    /// Search saved context items using full-text search.
    #[tool(name = "search_context")]
    fn search_context(
        &self,
        Parameters(input): Parameters<SearchContextInput>,
    ) -> Result<Json<ContextItemList>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let limit = clamp_limit(input.limit);

        let mut sql = String::from(
            "SELECT c.id, c.project, c.category, c.content, c.created_at \
             FROM context_fts f \
             JOIN context_items c ON f.item_id = CAST(c.id AS TEXT) \
             WHERE context_fts MATCH ?1",
        );
        let mut params: Vec<Param> = vec![Param::Text(escape_fts_query(&input.query))];

        if let Some(ref category) = input.category {
            sql.push_str(" AND c.category = ?2");
            params.push(Param::Text(category.clone()));
        }
        if let Some(ref project) = input.project {
            sql.push_str(&format!(" AND c.project = ?{}", params.len() + 1));
            params.push(Param::Text(project.clone()));
        }
        sql.push_str(&format!(" LIMIT {limit}"));

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(rusqlite::params_from_iter(&params), |row| {
                Ok(ContextItemResult {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    category: row.get(2)?,
                    content: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(Json(ContextItemList { results }))
    }

    /// Get a comprehensive summary of a project's accumulated knowledge.
    #[tool(name = "get_project_summary")]
    fn get_project_summary(
        &self,
        Parameters(input): Parameters<GetProjectSummaryInput>,
    ) -> Result<Json<ProjectSummary>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Context items
        let mut stmt = conn
            .prepare(
                "SELECT id, project, category, content, created_at FROM context_items \
                 WHERE (?1 IS NULL OR project = ?1) ORDER BY category, created_at DESC",
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let context_items: Vec<ContextItemResult> = stmt
            .query_map([&input.project], |row| {
                Ok(ContextItemResult {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    category: row.get(2)?,
                    content: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        // Recent bugs
        let mut stmt = conn
            .prepare(
                "SELECT id, error_message, root_cause, fix_description, tags, file_path, created_at \
                 FROM bugs WHERE (?1 IS NULL OR project = ?1) ORDER BY created_at DESC LIMIT 20",
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let recent_bugs: Vec<BugResult> = stmt
            .query_map([&input.project], |row| {
                Ok(BugResult {
                    id: row.get(0)?,
                    error_message: row.get(1)?,
                    root_cause: row.get(2)?,
                    fix_description: row.get(3)?,
                    tags: row.get(4)?,
                    file_path: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        // Do-not-repeat rules
        let mut stmt = conn
            .prepare(
                "SELECT id, rule, reason, file_path, created_at FROM do_not_repeat \
                 WHERE (?1 IS NULL OR project = ?1) ORDER BY created_at DESC",
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let do_not_repeat: Vec<DoNotRepeatResult> = stmt
            .query_map([&input.project], |row| {
                Ok(DoNotRepeatResult {
                    id: row.get(0)?,
                    rule: row.get(1)?,
                    reason: row.get(2)?,
                    file_path: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        // Token stats
        let (total_sessions, total_input, total_output): (i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(total_input_tokens), 0), \
                 COALESCE(SUM(total_output_tokens), 0) FROM sessions \
                 WHERE (?1 IS NULL OR project = ?1)",
                [&input.project],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(Json(ProjectSummary {
            project: input.project,
            context_items,
            recent_bugs,
            do_not_repeat,
            total_sessions,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
        }))
    }

    /// Log a bug with error message, fix description, and optional root cause.
    #[tool(name = "log_bug")]
    fn log_bug(
        &self,
        Parameters(input): Parameters<LogBugInput>,
    ) -> Result<Json<SimpleResult>, rmcp::ErrorData> {
        // S4: Validate and truncate input
        if input.error_message.is_empty() || input.fix_description.is_empty() {
            return Err(rmcp::ErrorData::invalid_request(
                "error_message and fix_description must not be empty",
                None,
            ));
        }
        let error_message = truncate_utf8(&input.error_message, 10_000);
        let fix_description = truncate_utf8(&input.fix_description, 10_000);
        let root_cause = input
            .root_cause
            .as_deref()
            .map(|s| truncate_utf8(s, 10_000));

        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let file_path = input.file_path.as_deref().map(db::normalize_path);

        // M4: Use input.project instead of hardcoded NULL
        conn.execute(
            "INSERT INTO bugs (project, error_message, root_cause, fix_description, tags, file_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            rusqlite::params![
                input.project,
                error_message,
                root_cause,
                fix_description,
                input.tags,
                file_path,
            ],
        )
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO bugs_fts (bug_id, project, file_path, error_message, root_cause, fix_description) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id.to_string(),
                input.project,
                file_path,
                error_message,
                root_cause,
                fix_description,
            ],
        )
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(Json(SimpleResult {
            success: true,
            message: format!("Bug logged (id: {id})"),
        }))
    }

    /// Search logged bugs using full-text search.
    #[tool(name = "search_bugs")]
    fn search_bugs(
        &self,
        Parameters(input): Parameters<SearchBugsInput>,
    ) -> Result<Json<BugResultList>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let mut sql = String::from(
            "SELECT b.id, b.error_message, b.root_cause, b.fix_description, b.tags, b.file_path, b.created_at \
             FROM bugs_fts f \
             JOIN bugs b ON f.bug_id = CAST(b.id AS TEXT) \
             WHERE bugs_fts MATCH ?1"
        );
        let mut params: Vec<Param> = vec![Param::Text(escape_fts_query(&input.query))];

        if let Some(ref project) = input.project {
            sql.push_str(&format!(" AND b.project = ?{}", params.len() + 1));
            params.push(Param::Text(project.clone()));
        }
        // M6: Filter by tags if provided
        if let Some(ref tags) = input.tags {
            for tag in tags.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()) {
                sql.push_str(&format!(" AND b.tags LIKE ?{}", params.len() + 1));
                params.push(Param::Text(format!("%{tag}%")));
            }
        }
        sql.push_str(" ORDER BY b.created_at DESC LIMIT 20");

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(rusqlite::params_from_iter(&params), |row| {
                Ok(BugResult {
                    id: row.get(0)?,
                    error_message: row.get(1)?,
                    root_cause: row.get(2)?,
                    fix_description: row.get(3)?,
                    tags: row.get(4)?,
                    file_path: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(Json(BugResultList { results }))
    }

    /// Add a rule to the do-not-repeat list.
    #[tool(name = "add_do_not_repeat")]
    fn add_do_not_repeat(
        &self,
        Parameters(input): Parameters<AddDoNotRepeatInput>,
    ) -> Result<Json<SimpleResult>, rmcp::ErrorData> {
        // S4: Validate and truncate input
        if input.rule.is_empty() {
            return Err(rmcp::ErrorData::invalid_request(
                "rule must not be empty",
                None,
            ));
        }
        let rule = truncate_utf8(&input.rule, 10_000);
        let reason = input.reason.as_deref().map(|s| truncate_utf8(s, 10_000));

        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let file_path = input.file_path.as_deref().map(db::normalize_path);

        // No FTS table for do_not_repeat — rules are few per project and retrieved
        // by exact project/file match, not free-text search.
        conn.execute(
            "INSERT INTO do_not_repeat (project, rule, reason, file_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            rusqlite::params![input.project, rule, reason, file_path],
        )
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let id = conn.last_insert_rowid();
        Ok(Json(SimpleResult {
            success: true,
            message: format!("Do-not-repeat rule added (id: {id})"),
        }))
    }

    /// Get do-not-repeat rules, optionally filtered by project or file.
    #[tool(name = "get_do_not_repeat")]
    fn get_do_not_repeat(
        &self,
        Parameters(input): Parameters<GetDoNotRepeatInput>,
    ) -> Result<Json<DoNotRepeatList>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Static query with nullable params — NULL means "no filter".
        // file_path filter also includes rules with NULL file_path (global rules).
        let file_path = input.file_path.as_deref().map(db::normalize_path);
        let params = [
            Param::Text(input.project.clone().unwrap_or_default()),
            Param::Text(file_path.unwrap_or_default()),
        ];

        let sql = "SELECT id, rule, reason, file_path, created_at FROM do_not_repeat \
                   WHERE (?1 = '' OR project = ?1) \
                   AND (?2 = '' OR file_path = ?2 OR file_path IS NULL) \
                   ORDER BY created_at DESC LIMIT 100";

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(rusqlite::params_from_iter(&params), |row| {
                Ok(DoNotRepeatResult {
                    id: row.get(0)?,
                    rule: row.get(1)?,
                    reason: row.get(2)?,
                    file_path: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(Json(DoNotRepeatList { results }))
    }

    /// Get token usage statistics with savings estimates.
    #[tool(name = "get_token_stats")]
    fn get_token_stats(
        &self,
        Parameters(input): Parameters<GetTokenStatsInput>,
    ) -> Result<Json<TokenStatsReport>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let days = clamp_days(input.days.or(Some(30)));
        let project = input.project.clone().unwrap_or_default();
        let params = [
            Param::Text(format!("-{days} days")),
            Param::Text(project.clone()),
        ];

        // Session + token aggregates
        let (total_sessions, total_input, total_output): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(total_input_tokens), 0), COALESCE(SUM(total_output_tokens), 0) \
             FROM sessions WHERE start_time >= datetime('now', ?1) AND (?2 = '' OR project = ?2)",
            rusqlite::params_from_iter(&params),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Cache tokens
        let (cache_read, cache_creation): (i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(tu.cache_read_tokens), 0), COALESCE(SUM(tu.cache_creation_tokens), 0) \
             FROM token_usage tu JOIN sessions s ON tu.session_id = s.session_id \
             WHERE s.start_time >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2)",
            rusqlite::params_from_iter(&params),
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let avg_input = if total_sessions > 0 {
            total_input / total_sessions
        } else {
            0
        };
        let avg_output = if total_sessions > 0 {
            total_output / total_sessions
        } else {
            0
        };

        // Files with anatomy
        let files_with_anatomy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_anatomy WHERE (?1 = '' OR project = ?1)",
                [&Param::Text(project.clone())],
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Total file reads and repeated reads
        let total_file_reads: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_reads", [], |row| row.get(0))
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let repeated_reads: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (SELECT session_id, file_path FROM session_reads \
             GROUP BY session_id, file_path HAVING COUNT(*) > 1)",
                [],
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Estimated saveable tokens: sum of token_estimate for non-first reads per (session, file)
        let saveable: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(token_estimate), 0) FROM session_reads \
             WHERE id NOT IN (SELECT MIN(id) FROM session_reads GROUP BY session_id, file_path)",
                [],
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Top 5 sessions by total tokens
        let mut stmt = conn.prepare(
            "SELECT session_id, project, (total_input_tokens + total_output_tokens) as total, start_time \
             FROM sessions WHERE start_time >= datetime('now', ?1) AND (?2 = '' OR project = ?2) \
             ORDER BY total DESC LIMIT 5"
        ).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let top_sessions: Vec<TokenSessionEntry> = stmt
            .query_map(rusqlite::params_from_iter(&params), |row| {
                Ok(TokenSessionEntry {
                    session_id: row.get(0)?,
                    project: row.get(1)?,
                    total_tokens: row.get(2)?,
                    start_time: row.get(3)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(Json(TokenStatsReport {
            period_days: days,
            project: input.project,
            total_sessions,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cache_read_tokens: cache_read,
            total_cache_creation_tokens: cache_creation,
            avg_input_per_session: avg_input,
            avg_output_per_session: avg_output,
            files_with_anatomy,
            total_file_reads,
            repeated_reads_warned: repeated_reads,
            estimated_tokens_saveable: saveable,
            top_sessions_by_tokens: top_sessions,
        }))
    }

    /// Get a comprehensive analytics report: usage, productivity, savings, and memory health.
    #[tool(name = "get_analytics")]
    fn get_analytics(
        &self,
        Parameters(input): Parameters<GetAnalyticsInput>,
    ) -> Result<Json<AnalyticsReport>, rmcp::ErrorData> {
        let conn = self
            .db
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let days = clamp_days(input.days.or(Some(30)));
        let project = input.project.clone().unwrap_or_default();
        let params = [
            Param::Text(format!("-{days} days")),
            Param::Text(project.clone()),
        ];

        // --- Usage ---
        let (total_sessions, total_input, total_output): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(total_input_tokens), 0), COALESCE(SUM(total_output_tokens), 0) \
             FROM sessions WHERE start_time >= datetime('now', ?1) AND (?2 = '' OR project = ?2)",
            rusqlite::params_from_iter(&params),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let cache_read: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(tu.cache_read_tokens), 0) \
             FROM token_usage tu JOIN sessions s ON tu.session_id = s.session_id \
             WHERE s.start_time >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2)",
                rusqlite::params_from_iter(&params),
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // --- Productivity ---
        let mut stmt = conn
            .prepare(
                "SELECT tc.tool_name, COUNT(*) as cnt FROM tool_calls tc \
             JOIN sessions s ON tc.session_id = s.session_id \
             WHERE s.start_time >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2) \
             GROUP BY tc.tool_name ORDER BY cnt DESC",
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let tool_call_breakdown: Vec<ToolBreakdownEntry> = stmt
            .query_map(rusqlite::params_from_iter(&params), |row| {
                Ok(ToolBreakdownEntry {
                    tool_name: row.get(0)?,
                    count: row.get(1)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        // Top read files
        let mut stmt = conn
            .prepare(
                "SELECT file_path, times_read, estimated_tokens FROM file_anatomy \
             WHERE (?1 = '' OR project = ?1) ORDER BY times_read DESC LIMIT 10",
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let top_read_files: Vec<FileStatsEntry> = stmt
            .query_map([&Param::Text(project.clone())], |row| {
                Ok(FileStatsEntry {
                    file_path: row.get(0)?,
                    count: row.get(1)?,
                    estimated_tokens: row.get(2)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        // Top written files
        let mut stmt = conn.prepare(
            "SELECT file_path, times_written, estimated_tokens FROM file_anatomy \
             WHERE (?1 = '' OR project = ?1) AND times_written > 0 ORDER BY times_written DESC LIMIT 10"
        ).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let top_written_files: Vec<FileStatsEntry> = stmt
            .query_map([&Param::Text(project.clone())], |row| {
                Ok(FileStatsEntry {
                    file_path: row.get(0)?,
                    count: row.get(1)?,
                    estimated_tokens: row.get(2)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        // Bug count in period
        let bug_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM bugs WHERE created_at >= datetime('now', ?1) AND (?2 = '' OR project = ?2)",
            rusqlite::params_from_iter(&params),
            |row| row.get(0),
        ).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Bugs by file (top 5)
        let mut stmt = conn
            .prepare(
                "SELECT file_path, COUNT(*) as cnt FROM bugs \
             WHERE file_path IS NOT NULL AND (?1 = '' OR project = ?1) \
             GROUP BY file_path ORDER BY cnt DESC LIMIT 5",
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let bugs_by_file: Vec<FileBugCount> = stmt
            .query_map([&Param::Text(project.clone())], |row| {
                Ok(FileBugCount {
                    file_path: row.get(0)?,
                    bug_count: row.get(1)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        // --- Savings ---
        let files_with_anatomy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_anatomy WHERE (?1 = '' OR project = ?1)",
                [&Param::Text(project.clone())],
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let total_file_reads: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_reads", [], |row| row.get(0))
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let repeated_reads: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (SELECT session_id, file_path FROM session_reads \
             GROUP BY session_id, file_path HAVING COUNT(*) > 1)",
                [],
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let saveable: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(token_estimate), 0) FROM session_reads \
             WHERE id NOT IN (SELECT MIN(id) FROM session_reads GROUP BY session_id, file_path)",
                [],
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // --- Memory Health ---
        let mut stmt = conn
            .prepare(
                "SELECT category, COUNT(*) FROM context_items \
             WHERE (?1 = '' OR project = ?1) GROUP BY category ORDER BY COUNT(*) DESC",
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let context_items_by_category: Vec<CategoryCount> = stmt
            .query_map([&Param::Text(project.clone())], |row| {
                Ok(CategoryCount {
                    category: row.get(0)?,
                    count: row.get(1)?,
                })
            })
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        let total_dnr: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM do_not_repeat WHERE (?1 = '' OR project = ?1)",
                [&Param::Text(project.clone())],
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let total_bugs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bugs WHERE (?1 = '' OR project = ?1)",
                [&Param::Text(project.clone())],
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let oldest_context: Option<String> = conn
            .query_row(
                "SELECT MIN(created_at) FROM context_items WHERE (?1 = '' OR project = ?1)",
                [&Param::Text(project.clone())],
                |row| row.get(0),
            )
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Projects with context vs without
        let mut stmt = conn
            .prepare("SELECT DISTINCT project FROM context_items WHERE project IS NOT NULL")
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let projects_with: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        let mut stmt = conn.prepare(
            "SELECT DISTINCT project FROM sessions WHERE project IS NOT NULL \
             AND project NOT IN (SELECT DISTINCT project FROM context_items WHERE project IS NOT NULL)"
        ).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let projects_without: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(Json(AnalyticsReport {
            period_days: days,
            project: input.project,
            total_sessions,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cache_read_tokens: cache_read,
            tool_call_breakdown,
            top_read_files,
            top_written_files,
            bug_count,
            bugs_by_file,
            files_with_anatomy,
            total_file_reads,
            repeated_reads_detected: repeated_reads,
            estimated_tokens_saveable: saveable,
            context_items_by_category,
            total_do_not_repeat_rules: total_dnr,
            total_bugs_logged: total_bugs,
            oldest_context_item: oldest_context,
            projects_with_context: projects_with,
            projects_without_context: projects_without,
        }))
    }
}

#[tool_handler]
impl ServerHandler for MnemosyneServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.server_info.name = "mnemosyne".into();
        info.server_info.version = "0.1.0".into();
        info.instructions = Some(
            "Mnemosyne: Claude Code session memory system. Search past sessions, \
            save context, log bugs, and manage do-not-repeat rules."
                .into(),
        );
        info
    }
}

impl MnemosyneServer {
    #[cfg(test)]
    fn new_with_conn(conn: Connection) -> Self {
        Self {
            db: Mutex::new(conn),
            tool_router: Self::tool_router(),
        }
    }

    fn new() -> Result<Self> {
        let conn = db::open_db()?;
        Ok(Self {
            db: Mutex::new(conn),
            tool_router: Self::tool_router(),
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr so it doesn't interfere with MCP stdio transport
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();

    let server = MnemosyneServer::new()?;
    tracing::info!("Mnemosyne MCP server starting on stdio");

    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("failed to start MCP service: {e}"))?;

    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP service error: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::wrapper::Parameters;
    use rusqlite::types::ToSql;

    fn test_server() -> MnemosyneServer {
        let conn = memory_common::db::open_db_in_memory().unwrap();
        MnemosyneServer::new_with_conn(conn)
    }

    // --- Group 2: Helper function tests ---

    #[test]
    fn test_escape_fts_query() {
        assert_eq!(escape_fts_query("hello"), "\"hello\"");
        assert_eq!(escape_fts_query("a\"b"), "\"a\"\"b\"");
        assert_eq!(escape_fts_query("*"), "\"*\"");
        assert_eq!(escape_fts_query("content:secret"), "\"content:secret\"");
    }

    #[test]
    fn test_clamp_limit() {
        assert_eq!(clamp_limit(None), 10);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(200)), 100);
        assert_eq!(clamp_limit(Some(50)), 50);
        assert_eq!(clamp_limit(Some(-5)), 1);
    }

    #[test]
    fn test_clamp_days() {
        assert_eq!(clamp_days(None), 7);
        assert_eq!(clamp_days(Some(-5)), 1);
        assert_eq!(clamp_days(Some(1000)), 365);
        assert_eq!(clamp_days(Some(30)), 30);
    }

    #[test]
    fn test_param_to_sql() {
        let text = Param::Text("hello".to_string());
        let int = Param::Int(42);
        // Verify they produce valid ToSqlOutput without panicking
        text.to_sql().expect("Text param should convert");
        int.to_sql().expect("Int param should convert");
    }

    // --- Group 4: MCP tool handler tests ---

    #[test]
    fn test_save_context_and_search() {
        let server = test_server();
        let save_result = server.save_context(Parameters(SaveContextInput {
            content: "Arena allocators prevent drop-time regression".to_string(),
            category: "architecture".to_string(),
            project: Some("test_proj".to_string()),
        }));
        assert!(save_result.is_ok());

        let search_result = server.search_context(Parameters(SearchContextInput {
            query: "arena allocators".to_string(),
            category: None,
            project: None,
            limit: None,
        }));
        let Json(list) = search_result.unwrap();
        assert_eq!(list.results.len(), 1);
        assert!(list.results[0].content.contains("Arena allocators"));
    }

    #[test]
    fn test_save_context_empty_rejected() {
        let server = test_server();
        let result = server.save_context(Parameters(SaveContextInput {
            content: "".to_string(),
            category: "test".to_string(),
            project: None,
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_save_context_truncation() {
        let server = test_server();
        let long_content = "x".repeat(20_000);
        let result = server.save_context(Parameters(SaveContextInput {
            content: long_content,
            category: "test".to_string(),
            project: None,
        }));
        assert!(result.is_ok());

        // Verify stored content is truncated
        let conn = server.db.lock().unwrap();
        let stored: String = conn
            .query_row("SELECT content FROM context_items LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(stored.len() <= 10_004); // 10000 + "..."
        assert!(stored.ends_with("..."));
    }

    #[test]
    fn test_log_bug_and_search() {
        let server = test_server();
        let log_result = server.log_bug(Parameters(LogBugInput {
            error_message: "index out of bounds".to_string(),
            fix_description: "check array length first".to_string(),
            root_cause: Some("missing bounds check".to_string()),
            tags: Some("safety".to_string()),
            file_path: Some("src/main.rs".to_string()),
            project: Some("test_proj".to_string()),
        }));
        assert!(log_result.is_ok());

        let search_result = server.search_bugs(Parameters(SearchBugsInput {
            query: "index out of bounds".to_string(),
            tags: None,
            project: None,
        }));
        let Json(list) = search_result.unwrap();
        assert_eq!(list.results.len(), 1);
        assert_eq!(list.results[0].error_message, "index out of bounds");
    }

    #[test]
    fn test_log_bug_with_tags_filter() {
        let server = test_server();
        server
            .log_bug(Parameters(LogBugInput {
                error_message: "perf regression".to_string(),
                fix_description: "use arena".to_string(),
                root_cause: None,
                tags: Some("perf,memory".to_string()),
                file_path: None,
                project: None,
            }))
            .unwrap();

        let result = server.search_bugs(Parameters(SearchBugsInput {
            query: "regression".to_string(),
            tags: Some("perf".to_string()),
            project: None,
        }));
        let Json(list) = result.unwrap();
        assert_eq!(list.results.len(), 1);
    }

    #[test]
    fn test_log_bug_empty_rejected() {
        let server = test_server();
        let result = server.log_bug(Parameters(LogBugInput {
            error_message: "".to_string(),
            fix_description: "some fix".to_string(),
            root_cause: None,
            tags: None,
            file_path: None,
            project: None,
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_add_and_get_do_not_repeat() {
        let server = test_server();
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "Don't use individual Vec<u8>".to_string(),
                reason: Some("causes drop-time regression".to_string()),
                project: Some("test_proj".to_string()),
                file_path: None,
            }))
            .unwrap();

        let Json(list) = server
            .get_do_not_repeat(Parameters(GetDoNotRepeatInput {
                project: Some("test_proj".to_string()),
                file_path: None,
            }))
            .unwrap();
        assert_eq!(list.results.len(), 1);
        assert!(list.results[0].rule.contains("Vec<u8>"));
    }

    #[test]
    fn test_get_do_not_repeat_file_filter() {
        let server = test_server();
        // Global rule (no file_path)
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "global rule".to_string(),
                reason: None,
                project: Some("proj".to_string()),
                file_path: None,
            }))
            .unwrap();
        // Scoped rule
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "scoped rule".to_string(),
                reason: None,
                project: Some("proj".to_string()),
                file_path: Some("src/main.rs".to_string()),
            }))
            .unwrap();

        let Json(list) = server
            .get_do_not_repeat(Parameters(GetDoNotRepeatInput {
                project: Some("proj".to_string()),
                file_path: Some("src/main.rs".to_string()),
            }))
            .unwrap();
        // Should return both global (file_path IS NULL) and scoped rules
        assert_eq!(list.results.len(), 2);
    }

    #[test]
    fn test_get_project_summary_empty() {
        let server = test_server();
        let Json(summary) = server
            .get_project_summary(Parameters(GetProjectSummaryInput {
                project: Some("nonexistent".to_string()),
            }))
            .unwrap();
        assert!(summary.context_items.is_empty());
        assert!(summary.recent_bugs.is_empty());
        assert!(summary.do_not_repeat.is_empty());
        assert_eq!(summary.total_sessions, 0);
    }

    #[test]
    fn test_get_project_summary_with_data() {
        let server = test_server();
        server
            .save_context(Parameters(SaveContextInput {
                content: "test context".to_string(),
                category: "arch".to_string(),
                project: Some("proj".to_string()),
            }))
            .unwrap();
        server
            .log_bug(Parameters(LogBugInput {
                error_message: "test bug".to_string(),
                fix_description: "test fix".to_string(),
                root_cause: None,
                tags: None,
                file_path: None,
                project: Some("proj".to_string()),
            }))
            .unwrap();
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "test rule".to_string(),
                reason: None,
                project: Some("proj".to_string()),
                file_path: None,
            }))
            .unwrap();

        let Json(summary) = server
            .get_project_summary(Parameters(GetProjectSummaryInput {
                project: Some("proj".to_string()),
            }))
            .unwrap();
        assert_eq!(summary.context_items.len(), 1);
        assert_eq!(summary.recent_bugs.len(), 1);
        assert_eq!(summary.do_not_repeat.len(), 1);
    }

    #[test]
    fn test_get_recent_sessions() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute("INSERT INTO sessions (session_id, project, start_time, message_count, total_input_tokens, total_output_tokens) VALUES ('s1', 'proj', datetime('now', '-1 hour'), 10, 100, 200)", []).unwrap();
            conn.execute("INSERT INTO sessions (session_id, project, start_time, message_count, total_input_tokens, total_output_tokens) VALUES ('s2', 'proj', datetime('now'), 5, 50, 100)", []).unwrap();
        }
        let Json(list) = server
            .get_recent_sessions(Parameters(GetRecentSessionsInput {
                days: Some(1),
                project: None,
            }))
            .unwrap();
        assert_eq!(list.results.len(), 2);
        // Most recent first
        assert_eq!(list.results[0].session_id, "s2");
    }

    #[test]
    fn test_get_session_detail() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute("INSERT INTO sessions (session_id, project, start_time, cwd, git_branch, message_count, total_input_tokens, total_output_tokens) VALUES ('sd1', 'proj', '2026-01-01', '/test', 'main', 2, 100, 200)", []).unwrap();
            conn.execute("INSERT INTO messages (uuid, session_id, role, content_type, content, timestamp) VALUES ('m1', 'sd1', 'user', 'text', 'first message', '2026-01-01T00:00:00Z')", []).unwrap();
            conn.execute("INSERT INTO messages (uuid, session_id, role, content_type, content, timestamp) VALUES ('m2', 'sd1', 'user', 'text', 'last message', '2026-01-01T01:00:00Z')", []).unwrap();
            conn.execute("INSERT INTO tool_calls (message_uuid, session_id, tool_name, timestamp) VALUES ('m2', 'sd1', 'Read', '2026-01-01T01:00:00Z')", []).unwrap();
            conn.execute("INSERT INTO tool_calls (message_uuid, session_id, tool_name, timestamp) VALUES ('m2', 'sd1', 'Read', '2026-01-01T01:01:00Z')", []).unwrap();
        }
        let Json(detail) = server
            .get_session_detail(Parameters(GetSessionDetailInput {
                session_id: "sd1".to_string(),
            }))
            .unwrap();
        assert_eq!(detail.session_id, "sd1");
        assert_eq!(detail.first_user_message, Some("first message".to_string()));
        assert_eq!(detail.last_user_message, Some("last message".to_string()));
        assert_eq!(detail.tool_summary.len(), 1);
        assert_eq!(detail.tool_summary[0].tool_name, "Read");
        assert_eq!(detail.tool_summary[0].count, 2);
    }

    #[test]
    fn test_search_sessions_fts() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute("INSERT INTO sessions (session_id, project, start_time, message_count, total_input_tokens, total_output_tokens) VALUES ('fts1', 'proj', datetime('now'), 1, 0, 0)", []).unwrap();
            conn.execute("INSERT INTO messages (uuid, session_id, role, content_type, content) VALUES ('fm1', 'fts1', 'user', 'text', 'the arena allocator prevents drop-time regression')", []).unwrap();
            conn.execute("INSERT INTO messages_fts (uuid, session_id, content) VALUES ('fm1', 'fts1', 'the arena allocator prevents drop-time regression')", []).unwrap();
        }
        let Json(list) = server
            .search_sessions(Parameters(SearchSessionsInput {
                query: "regression".to_string(),
                limit: None,
                project: None,
            }))
            .unwrap();
        assert_eq!(list.results.len(), 1);
        assert_eq!(list.results[0].session_id, "fts1");
    }

    #[test]
    fn test_get_file_history() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute("INSERT INTO sessions (session_id, project, start_time, message_count, total_input_tokens, total_output_tokens) VALUES ('fh1', 'proj', datetime('now'), 1, 0, 0)", []).unwrap();
            conn.execute("INSERT INTO messages (uuid, session_id, role, content_type) VALUES ('fhm1', 'fh1', 'assistant', 'tool_use')", []).unwrap();
            conn.execute("INSERT INTO tool_calls (message_uuid, session_id, tool_name, file_path, timestamp) VALUES ('fhm1', 'fh1', 'Edit', 'src/parser/pack.rs', datetime('now'))", []).unwrap();
        }
        let Json(list) = server
            .get_file_history(Parameters(GetFileHistoryInput {
                file_path: Some("pack.rs".to_string()),
                project: None,
                days: None,
            }))
            .unwrap();
        assert_eq!(list.results.len(), 1);
        assert_eq!(list.results[0].tool_name, "Edit");
    }

    // --- Analytics tool tests ---

    #[test]
    fn test_get_token_stats_empty() {
        let server = test_server();
        let Json(report) = server
            .get_token_stats(Parameters(GetTokenStatsInput {
                project: None,
                days: Some(30),
            }))
            .unwrap();
        assert_eq!(report.total_sessions, 0);
        assert_eq!(report.total_input_tokens, 0);
        assert_eq!(report.total_output_tokens, 0);
        assert!(report.top_sessions_by_tokens.is_empty());
    }

    #[test]
    fn test_get_token_stats_with_data() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute("INSERT INTO sessions (session_id, project, start_time, message_count, total_input_tokens, total_output_tokens) \
                VALUES ('ts1', 'proj', datetime('now'), 10, 5000, 3000)", []).unwrap();
            conn.execute(
                "INSERT INTO messages (uuid, session_id, role, content_type, content) \
                VALUES ('tu1', 'ts1', 'assistant', 'text', 'response')",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO token_usage (message_uuid, session_id, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens) \
                VALUES ('tu1', 'ts1', 5000, 3000, 1000, 500)", []).unwrap();
            conn.execute("INSERT INTO file_anatomy (project, file_path, description, estimated_tokens, times_read, times_written, last_scanned) \
                VALUES ('proj', 'src/main.rs', 'Main entry', 200, 3, 1, datetime('now'))", []).unwrap();
            // Two reads of the same file in the same session → one repeated read
            conn.execute("INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) VALUES ('ts1', 'src/main.rs', datetime('now', '-2 minutes'), 200)", []).unwrap();
            conn.execute("INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) VALUES ('ts1', 'src/main.rs', datetime('now'), 200)", []).unwrap();
        }
        let Json(report) = server
            .get_token_stats(Parameters(GetTokenStatsInput {
                project: Some("proj".to_string()),
                days: Some(7),
            }))
            .unwrap();
        assert_eq!(report.total_sessions, 1);
        assert_eq!(report.total_input_tokens, 5000);
        assert_eq!(report.total_output_tokens, 3000);
        assert_eq!(report.total_cache_read_tokens, 1000);
        assert_eq!(report.avg_input_per_session, 5000);
        assert_eq!(report.files_with_anatomy, 1);
        assert_eq!(report.total_file_reads, 2);
        assert_eq!(report.repeated_reads_warned, 1);
        assert_eq!(report.estimated_tokens_saveable, 200);
        assert_eq!(report.top_sessions_by_tokens.len(), 1);
        assert_eq!(report.top_sessions_by_tokens[0].total_tokens, 8000);
    }

    #[test]
    fn test_get_analytics_empty() {
        let server = test_server();
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(30),
            }))
            .unwrap();
        assert_eq!(report.total_sessions, 0);
        assert!(report.tool_call_breakdown.is_empty());
        assert!(report.top_read_files.is_empty());
        assert!(report.context_items_by_category.is_empty());
        assert!(report.projects_with_context.is_empty());
    }

    #[test]
    fn test_get_analytics_with_data() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            // Session + messages
            conn.execute("INSERT INTO sessions (session_id, project, start_time, message_count, total_input_tokens, total_output_tokens) \
                VALUES ('a1', 'proj', datetime('now'), 5, 2000, 1000)", []).unwrap();
            conn.execute(
                "INSERT INTO messages (uuid, session_id, role, content_type, content) \
                VALUES ('m1', 'a1', 'assistant', 'tool_use', 'reading file')",
                [],
            )
            .unwrap();
            // Tool calls
            conn.execute("INSERT INTO tool_calls (message_uuid, session_id, tool_name, file_path, timestamp) \
                VALUES ('m1', 'a1', 'Read', 'src/lib.rs', datetime('now'))", []).unwrap();
            conn.execute("INSERT INTO tool_calls (message_uuid, session_id, tool_name, file_path, timestamp) \
                VALUES ('m1', 'a1', 'Read', 'src/main.rs', datetime('now'))", []).unwrap();
            conn.execute("INSERT INTO tool_calls (message_uuid, session_id, tool_name, file_path, timestamp) \
                VALUES ('m1', 'a1', 'Edit', 'src/main.rs', datetime('now'))", []).unwrap();
            // Anatomy
            conn.execute("INSERT INTO file_anatomy (project, file_path, description, estimated_tokens, times_read, times_written, last_scanned) \
                VALUES ('proj', 'src/main.rs', 'Entry point', 150, 5, 2, datetime('now'))", []).unwrap();
            // Bugs
            conn.execute("INSERT INTO bugs (project, error_message, fix_description, file_path, created_at) \
                VALUES ('proj', 'null ref', 'add check', 'src/main.rs', datetime('now'))", []).unwrap();
            // Context
            conn.execute(
                "INSERT INTO context_items (project, category, content, created_at) \
                VALUES ('proj', 'architecture', 'uses arena allocators', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO context_items (project, category, content, created_at) \
                VALUES ('proj', 'conventions', 'snake_case everywhere', datetime('now'))",
                [],
            )
            .unwrap();
            // Do-not-repeat
            conn.execute(
                "INSERT INTO do_not_repeat (project, rule, created_at) \
                VALUES ('proj', 'no Vec<u8> for delta', datetime('now'))",
                [],
            )
            .unwrap();
            // Another project with sessions but no context
            conn.execute("INSERT INTO sessions (session_id, project, start_time, message_count, total_input_tokens, total_output_tokens) \
                VALUES ('a2', 'orphan', datetime('now'), 1, 100, 50)", []).unwrap();
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(30),
            }))
            .unwrap();
        assert_eq!(report.total_sessions, 2);
        assert!(!report.tool_call_breakdown.is_empty());
        assert_eq!(report.tool_call_breakdown[0].tool_name, "Read");
        assert_eq!(report.tool_call_breakdown[0].count, 2);
        assert_eq!(report.bug_count, 1);
        assert_eq!(report.bugs_by_file.len(), 1);
        assert_eq!(report.context_items_by_category.len(), 2);
        assert_eq!(report.total_do_not_repeat_rules, 1);
        assert_eq!(report.total_bugs_logged, 1);
        assert!(report.oldest_context_item.is_some());
        assert!(report.projects_with_context.contains(&"proj".to_string()));
        assert!(report
            .projects_without_context
            .contains(&"orphan".to_string()));
    }
}
