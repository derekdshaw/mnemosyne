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
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use rusqlite::types::{ToSql, ToSqlOutput};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tools::*;
use tracing::Instrument;

/// Waits for any shutdown signal the host OS exposes, then returns. On unix
/// that's SIGTERM / SIGHUP / SIGINT; on Windows it's ctrl_c / ctrl_break /
/// ctrl_close / ctrl_shutdown. Returning from this future is the cue for the
/// caller to fire the shutdown notifier.
#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to install SIGTERM handler: {e}");
            return;
        }
    };
    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to install SIGHUP handler: {e}");
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("SIGTERM received"),
        _ = sighup.recv() => tracing::info!("SIGHUP received"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received"),
    }
}

#[cfg(windows)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
    let mut br = match ctrl_break() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to install ctrl_break handler: {e}");
            return;
        }
    };
    let mut cl = match ctrl_close() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to install ctrl_close handler: {e}");
            return;
        }
    };
    let mut sd = match ctrl_shutdown() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to install ctrl_shutdown handler: {e}");
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("ctrl_c received"),
        _ = br.recv() => tracing::info!("ctrl_break received"),
        _ = cl.recv() => tracing::info!("ctrl_close received"),
        _ = sd.recv() => tracing::info!("ctrl_shutdown received"),
    }
}

/// Maximum time any single tool handler may run before we return an error.
/// Defense against a wedged SQL call pinning the whole stdio service.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(10);

/// Stack-allocated SQL parameter enum. Avoids `Box<dyn ToSql>` heap allocations
/// and the double-vec indirection (`Vec<Box<dyn ToSql>>` + `Vec<&dyn ToSql>`)
/// that was previously needed for dynamic query building with optional filters.
///
/// MCP tool queries build SQL dynamically based on which optional parameters the
/// caller provides (project, days, file_path, tags, etc.). This requires a
/// heterogeneous parameter list. Rather than boxing each parameter on the heap,
/// this enum wraps the three types we actually use — the match dispatches
/// statically, and `Vec<Param>` is a single contiguous allocation.
#[allow(dead_code)] // Int reserved for future queries (e.g., analytics drill-downs)
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

/// Logs the error at `error!` level (which inherits the active tracing span,
/// so the tool name set by `#[tracing::instrument]` appears in the record) and
/// converts it into an `internal_error` MCP response. Use on every fallible DB
/// or rmcp call inside a tool handler — keeps the call-site terse while still
/// producing structured logs for every error path.
trait LogErr<T> {
    fn log_internal(self, op: &'static str) -> Result<T, rmcp::ErrorData>;
}

impl<T, E: std::fmt::Display> LogErr<T> for Result<T, E> {
    fn log_internal(self, op: &'static str) -> Result<T, rmcp::ErrorData> {
        self.map_err(|e| {
            tracing::error!(op, error = %e, "tool operation failed");
            rmcp::ErrorData::internal_error(e.to_string(), None)
        })
    }
}

struct MnemosyneServer {
    // S8: If a tool handler panics, Mutex becomes poisoned. All subsequent .lock() calls
    // return PoisonError, which we map to MCP error responses — the server degrades
    // gracefully rather than crashing, but DB operations stop working.
    db: Arc<Mutex<Connection>>,
}

impl MnemosyneServer {
    /// Runs a synchronous DB closure on a blocking worker with the default
    /// HANDLER_TIMEOUT. All tool handlers go through this helper.
    ///
    /// `tool` is the MCP tool name; it becomes a span field on every log
    /// emitted by the closure (and on the timing/error records emitted by
    /// `run_db_with_timeout` itself), so a single grep on `tool=save_context`
    /// surfaces every event from that handler.
    async fn run_db<F, T>(&self, tool: &'static str, f: F) -> Result<T, rmcp::ErrorData>
    where
        F: FnOnce(&Connection) -> Result<T, rmcp::ErrorData> + Send + 'static,
        T: Send + 'static,
    {
        self.run_db_with_timeout(tool, HANDLER_TIMEOUT, f).await
    }

    /// Internal helper that bumps activity, runs `f` on the blocking pool, and
    /// enforces `timeout`. Factored out so tests can drive a short deadline
    /// without touching production constants.
    async fn run_db_with_timeout<F, T>(
        &self,
        tool: &'static str,
        timeout: Duration,
        f: F,
    ) -> Result<T, rmcp::ErrorData>
    where
        F: FnOnce(&Connection) -> Result<T, rmcp::ErrorData> + Send + 'static,
        T: Send + 'static,
    {
        let span = tracing::info_span!("tool", name = tool);
        let _enter = span.enter();
        tracing::debug!("tool invoked");
        let db = self.db.clone();
        let started = Instant::now();
        let parent = span.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let _g = parent.enter();
            let conn = match db.lock() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "DB mutex poisoned — a previous handler panicked");
                    return Err(rmcp::ErrorData::internal_error(e.to_string(), None));
                }
            };
            f(&conn)
        });
        drop(_enter);
        let outcome = tokio::time::timeout(timeout, handle)
            .instrument(span.clone())
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let _enter = span.enter();
        match outcome {
            Ok(Ok(Ok(v))) => {
                tracing::debug!(elapsed_ms, "tool ok");
                Ok(v)
            }
            Ok(Ok(Err(e))) => {
                // The closure already logged via `log_internal`; record duration so
                // we can correlate slow failures.
                tracing::debug!(elapsed_ms, "tool returned error");
                Err(e)
            }
            Ok(Err(join_err)) => {
                tracing::error!(
                    elapsed_ms,
                    error = %join_err,
                    "blocking task panicked or was cancelled"
                );
                Err(rmcp::ErrorData::internal_error(
                    format!("handler task join error: {join_err}"),
                    None,
                ))
            }
            Err(_) => {
                tracing::error!(
                    elapsed_ms,
                    timeout_secs = timeout.as_secs(),
                    "tool timed out"
                );
                Err(rmcp::ErrorData::internal_error(
                    format!("handler timed out after {}s", timeout.as_secs()),
                    None,
                ))
            }
        }
    }
}

#[tool_router]
impl MnemosyneServer {
    /// Search across all past session messages using full-text search.
    #[tool(name = "search_sessions")]
    async fn search_sessions(
        &self,
        Parameters(input): Parameters<SearchSessionsInput>,
    ) -> Result<Json<SessionResultList>, rmcp::ErrorData> {
        self.run_db("search_sessions", move |conn| {
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

            let mut stmt = conn.prepare(&sql).log_internal("search_sessions:01")?;
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
                .log_internal("search_sessions:02")?
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
        })
        .await
    }

    /// List recent sessions, optionally filtered by project.
    #[tool(name = "get_recent_sessions")]
    async fn get_recent_sessions(
        &self,
        Parameters(input): Parameters<GetRecentSessionsInput>,
    ) -> Result<Json<SessionResultList>, rmcp::ErrorData> {
        self.run_db("get_recent_sessions", move |conn| {
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

            let mut stmt = conn.prepare(&sql).log_internal("get_recent_sessions:01")?;
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
                .log_internal("get_recent_sessions:02")?
                .filter_map(|r| r.ok())
                .collect();

            Ok(Json(SessionResultList { results }))
        })
        .await
    }

    /// Get detailed information about a specific session.
    #[tool(name = "get_session_detail")]
    async fn get_session_detail(
        &self,
        Parameters(input): Parameters<GetSessionDetailInput>,
    ) -> Result<Json<SessionDetail>, rmcp::ErrorData> {
        self.run_db("get_session_detail", move |conn| {
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
                .log_internal("get_session_detail:01")?;

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
                .log_internal("get_session_detail:02")?;
            let tool_summary: Vec<ToolSummaryEntry> = stmt
                .query_map([&input.session_id], |row| {
                    Ok(ToolSummaryEntry {
                        tool_name: row.get(0)?,
                        count: row.get(1)?,
                    })
                })
                .log_internal("get_session_detail:03")?
                .filter_map(|r| r.ok())
                .collect();

            Ok(Json(SessionDetail {
                first_user_message: first_msg,
                last_user_message: last_msg,
                tool_summary,
                ..session
            }))
        })
        .await
    }

    /// Get tool calls that touched a specific file, with session context.
    #[tool(name = "get_file_history")]
    async fn get_file_history(
        &self,
        Parameters(input): Parameters<GetFileHistoryInput>,
    ) -> Result<Json<FileHistoryList>, rmcp::ErrorData> {
        self.run_db("get_file_history", move |conn| {
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

            let mut stmt = conn.prepare(sql).log_internal("get_file_history:01")?;
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
                .log_internal("get_file_history:02")?
                .filter_map(|r| r.ok())
                .collect();

            Ok(Json(FileHistoryList { results }))
        })
        .await
    }

    /// Save a context item (decision, convention, architecture note).
    #[tool(name = "save_context")]
    async fn save_context(
        &self,
        Parameters(input): Parameters<SaveContextInput>,
    ) -> Result<Json<SimpleResult>, rmcp::ErrorData> {
        // S4: Validate and truncate input (done outside the blocking closure so we
        // fail fast without needing a DB lock).
        if input.content.is_empty() {
            tracing::warn!(tool = "save_context", "rejected: empty content");
            return Err(rmcp::ErrorData::invalid_request(
                "content must not be empty",
                None,
            ));
        }
        let content = truncate_utf8(&input.content, 10_000);
        let compressed = input.compress.unwrap_or(false);
        // When compressed, estimate original length (~80% compression ratio)
        let original_length: Option<i64> = if compressed {
            Some((content.len() as f64 / 0.8) as i64)
        } else {
            None
        };
        let project = input.project.clone();
        let category = input.category.clone();

        self.run_db("save_context", move |conn| {
            conn.execute(
                "INSERT INTO context_items (project, category, content, created_at, original_length) \
                 VALUES (?1, ?2, ?3, datetime('now'), ?4)",
                rusqlite::params![project, category, content, original_length],
            )
            .log_internal("save_context:01")?;

            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO context_fts (item_id, project, category, content) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id.to_string(), project, category, content],
            )
            .log_internal("save_context:02")?;

            let suffix = if compressed { ", compressed" } else { "" };
            Ok(Json(SimpleResult {
                success: true,
                message: format!("Context saved (id: {id}{suffix})"),
            }))
        })
        .await
    }

    /// Search saved context items using full-text search.
    #[tool(name = "search_context")]
    async fn search_context(
        &self,
        Parameters(input): Parameters<SearchContextInput>,
    ) -> Result<Json<ContextItemList>, rmcp::ErrorData> {
        self.run_db("search_context", move |conn| {
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

            let mut stmt = conn.prepare(&sql).log_internal("search_context:01")?;
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
                .log_internal("search_context:02")?
                .filter_map(|r| r.ok())
                .collect();

            Ok(Json(ContextItemList { results }))
        })
        .await
    }

    /// Get a comprehensive summary of a project's accumulated knowledge.
    #[tool(name = "get_project_summary")]
    async fn get_project_summary(
        &self,
        Parameters(input): Parameters<GetProjectSummaryInput>,
    ) -> Result<Json<ProjectSummary>, rmcp::ErrorData> {
        self.run_db("get_project_summary", move |conn| {
            // Context items
            let mut stmt = conn
                .prepare(
                    "SELECT id, project, category, content, created_at FROM context_items \
                     WHERE (project IS NULL OR ?1 IS NULL OR project = ?1) ORDER BY category, created_at DESC",
                )
                .log_internal("get_project_summary:01")?;
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
                .log_internal("get_project_summary:02")?
                .filter_map(|r| r.ok())
                .collect();

            // Recent bugs
            let mut stmt = conn
                .prepare(
                    "SELECT id, error_message, root_cause, fix_description, tags, file_path, created_at \
                     FROM bugs WHERE (project IS NULL OR ?1 IS NULL OR project = ?1) ORDER BY created_at DESC LIMIT 20",
                )
                .log_internal("get_project_summary:03")?;
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
                .log_internal("get_project_summary:04")?
                .filter_map(|r| r.ok())
                .collect();

            // Do-not-repeat rules
            let mut stmt = conn
                .prepare(
                    "SELECT id, rule, reason, file_path, created_at FROM do_not_repeat \
                     WHERE (project IS NULL OR ?1 IS NULL OR project = ?1) ORDER BY created_at DESC",
                )
                .log_internal("get_project_summary:05")?;
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
                .log_internal("get_project_summary:06")?
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
                .log_internal("get_project_summary:07")?;

            Ok(Json(ProjectSummary {
                project: input.project,
                context_items,
                recent_bugs,
                do_not_repeat,
                total_sessions,
                total_input_tokens: total_input,
                total_output_tokens: total_output,
            }))
        })
        .await
    }

    /// Log a bug with error message, fix description, and optional root cause.
    #[tool(name = "log_bug")]
    async fn log_bug(
        &self,
        Parameters(input): Parameters<LogBugInput>,
    ) -> Result<Json<SimpleResult>, rmcp::ErrorData> {
        // S4: Validate and truncate input (outside the DB closure so we fail fast
        // without contending for the connection lock).
        if input.error_message.is_empty() || input.fix_description.is_empty() {
            tracing::warn!(
                tool = "log_bug",
                empty_err = input.error_message.is_empty(),
                empty_fix = input.fix_description.is_empty(),
                "rejected: empty required fields"
            );
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
        let compressed = input.compress.unwrap_or(false);
        // When compressed, estimate original length of fix_description + root_cause
        let original_length: Option<i64> = if compressed {
            let compressed_len = fix_description.len() + root_cause.as_ref().map_or(0, |s| s.len());
            Some((compressed_len as f64 / 0.8) as i64)
        } else {
            None
        };
        let file_path = input.file_path.as_deref().map(db::normalize_path);
        let project = input.project.clone();
        let tags = input.tags.clone();

        self.run_db("log_bug", move |conn| {
            // M4: Use input.project instead of hardcoded NULL
            conn.execute(
                "INSERT INTO bugs (project, error_message, root_cause, fix_description, tags, file_path, created_at, original_length) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), ?7)",
                rusqlite::params![
                    project,
                    error_message,
                    root_cause,
                    fix_description,
                    tags,
                    file_path,
                    original_length,
                ],
            )
            .log_internal("log_bug:01")?;

            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO bugs_fts (bug_id, project, file_path, error_message, root_cause, fix_description) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    id.to_string(),
                    project,
                    file_path,
                    error_message,
                    root_cause,
                    fix_description,
                ],
            )
            .log_internal("log_bug:02")?;

            let suffix = if compressed { ", compressed" } else { "" };
            Ok(Json(SimpleResult {
                success: true,
                message: format!("Bug logged (id: {id}{suffix})"),
            }))
        })
        .await
    }

    /// Search logged bugs using full-text search.
    #[tool(name = "search_bugs")]
    async fn search_bugs(
        &self,
        Parameters(input): Parameters<SearchBugsInput>,
    ) -> Result<Json<BugResultList>, rmcp::ErrorData> {
        self.run_db("search_bugs", move |conn| {
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
                .log_internal("search_bugs:01")?;
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
                .log_internal("search_bugs:02")?
                .filter_map(|r| r.ok())
                .collect();

            Ok(Json(BugResultList { results }))
        })
        .await
    }

    /// Add a rule to the do-not-repeat list.
    #[tool(name = "add_do_not_repeat")]
    async fn add_do_not_repeat(
        &self,
        Parameters(input): Parameters<AddDoNotRepeatInput>,
    ) -> Result<Json<SimpleResult>, rmcp::ErrorData> {
        // S4: Validate and truncate input (outside the DB closure).
        if input.rule.is_empty() {
            tracing::warn!(tool = "add_do_not_repeat", "rejected: empty rule");
            return Err(rmcp::ErrorData::invalid_request(
                "rule must not be empty",
                None,
            ));
        }
        let rule = truncate_utf8(&input.rule, 10_000);
        let reason = input.reason.as_deref().map(|s| truncate_utf8(s, 10_000));
        let file_path = input.file_path.as_deref().map(db::normalize_path);
        let project = input.project.clone();

        self.run_db("add_do_not_repeat", move |conn| {
            // No FTS table for do_not_repeat — rules are few per project and retrieved
            // by exact project/file match, not free-text search.
            conn.execute(
                "INSERT INTO do_not_repeat (project, rule, reason, file_path, created_at) \
                 VALUES (?1, ?2, ?3, ?4, datetime('now'))",
                rusqlite::params![project, rule, reason, file_path],
            )
            .log_internal("add_do_not_repeat:01")?;

            let id = conn.last_insert_rowid();
            let scope = match (&project, &file_path) {
                (Some(p), Some(f)) => format!("project={p}, file={f}"),
                (Some(p), None) => format!("project={p}"),
                (None, Some(f)) => format!("GLOBAL, file={f}"),
                (None, None) => "GLOBAL (applies to all projects)".to_string(),
            };
            Ok(Json(SimpleResult {
                success: true,
                message: format!("Do-not-repeat rule added (id: {id}, scope: {scope})"),
            }))
        })
        .await
    }

    /// Get do-not-repeat rules, optionally filtered by project or file.
    #[tool(name = "get_do_not_repeat")]
    async fn get_do_not_repeat(
        &self,
        Parameters(input): Parameters<GetDoNotRepeatInput>,
    ) -> Result<Json<DoNotRepeatList>, rmcp::ErrorData> {
        self.run_db("get_do_not_repeat", move |conn| {
            // Static query with nullable params — NULL means "no filter".
            // file_path filter also includes rules with NULL file_path (global rules).
            let file_path = input.file_path.as_deref().map(db::normalize_path);
            let params = [
                Param::Text(input.project.clone().unwrap_or_default()),
                Param::Text(file_path.unwrap_or_default()),
            ];

            let sql = "SELECT id, rule, reason, file_path, created_at FROM do_not_repeat \
                       WHERE (project IS NULL OR ?1 = '' OR project = ?1) \
                       AND (?2 = '' OR file_path = ?2 OR file_path IS NULL) \
                       ORDER BY created_at DESC LIMIT 100";

            let mut stmt = conn.prepare(sql).log_internal("get_do_not_repeat:01")?;
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
                .log_internal("get_do_not_repeat:02")?
                .filter_map(|r| r.ok())
                .collect();

            Ok(Json(DoNotRepeatList { results }))
        })
        .await
    }

    /// Comprehensive analytics report: usage + tokens + savings + overhead +
    /// optionally productivity + memory health. Pass `section: "tokens"` to
    /// skip the productivity and memory-health queries for a cheaper response
    /// when you only care about token accounting; anything else (including
    /// omitted) returns the full report.
    #[tool(name = "get_analytics")]
    async fn get_analytics(
        &self,
        Parameters(input): Parameters<GetAnalyticsInput>,
    ) -> Result<Json<AnalyticsReport>, rmcp::ErrorData> {
        self.run_db("get_analytics", move |conn| {
            let days = clamp_days(input.days.or(Some(30)));
            let project = input.project.clone().unwrap_or_default();
            let params = [
                Param::Text(format!("-{days} days")),
                Param::Text(project.clone()),
            ];
            let section = input.section.as_deref().unwrap_or("full").to_string();
            let tokens_only = section == "tokens";

            // --- Usage (always computed) ---
            let (total_sessions, total_input, total_output): (i64, i64, i64) = conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(total_input_tokens), 0), COALESCE(SUM(total_output_tokens), 0) \
                 FROM sessions WHERE start_time >= datetime('now', ?1) AND (?2 = '' OR project = ?2)",
                rusqlite::params_from_iter(&params),
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).log_internal("get_analytics:01")?;

            let (cache_read, cache_creation): (i64, i64) = conn.query_row(
                "SELECT COALESCE(SUM(tu.cache_read_tokens), 0), COALESCE(SUM(tu.cache_creation_tokens), 0) \
                 FROM token_usage tu JOIN sessions s ON tu.session_id = s.session_id \
                 WHERE s.start_time >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2)",
                rusqlite::params_from_iter(&params),
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).log_internal("get_analytics:02")?;

            let avg_input = if total_sessions > 0 { total_input / total_sessions } else { 0 };
            let avg_output = if total_sessions > 0 { total_output / total_sessions } else { 0 };

            // --- Productivity (skipped when section="tokens") ---
            let (tool_call_breakdown, top_read_files, top_written_files, bug_count, bugs_by_file) =
                if tokens_only {
                    (Vec::new(), Vec::new(), Vec::new(), 0, Vec::new())
                } else {
                    let mut stmt = conn
                        .prepare(
                            "SELECT tc.tool_name, COUNT(*) as cnt FROM tool_calls tc \
                         JOIN sessions s ON tc.session_id = s.session_id \
                         WHERE s.start_time >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2) \
                         GROUP BY tc.tool_name ORDER BY cnt DESC",
                        )
                        .log_internal("get_analytics:03")?;
                    let tool_call_breakdown: Vec<ToolBreakdownEntry> = stmt
                        .query_map(rusqlite::params_from_iter(&params), |row| {
                            Ok(ToolBreakdownEntry {
                                tool_name: row.get(0)?,
                                count: row.get(1)?,
                            })
                        })
                        .log_internal("get_analytics:04")?
                        .filter_map(|r| r.ok())
                        .collect();

                    let mut stmt = conn
                        .prepare(
                            "SELECT file_path, times_read, estimated_tokens FROM file_anatomy \
                         WHERE (?1 = '' OR project = ?1) ORDER BY times_read DESC LIMIT 10",
                        )
                        .log_internal("get_analytics:05")?;
                    let top_read_files: Vec<FileStatsEntry> = stmt
                        .query_map([&Param::Text(project.clone())], |row| {
                            Ok(FileStatsEntry {
                                file_path: row.get(0)?,
                                count: row.get(1)?,
                                estimated_tokens: row.get(2)?,
                            })
                        })
                        .log_internal("get_analytics:06")?
                        .filter_map(|r| r.ok())
                        .collect();

                    let mut stmt = conn.prepare(
                        "SELECT file_path, times_written, estimated_tokens FROM file_anatomy \
                         WHERE (?1 = '' OR project = ?1) AND times_written > 0 ORDER BY times_written DESC LIMIT 10"
                    ).log_internal("get_analytics:07")?;
                    let top_written_files: Vec<FileStatsEntry> = stmt
                        .query_map([&Param::Text(project.clone())], |row| {
                            Ok(FileStatsEntry {
                                file_path: row.get(0)?,
                                count: row.get(1)?,
                                estimated_tokens: row.get(2)?,
                            })
                        })
                        .log_internal("get_analytics:08")?
                        .filter_map(|r| r.ok())
                        .collect();

                    let bug_count: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM bugs WHERE created_at >= datetime('now', ?1) AND (?2 = '' OR project = ?2)",
                        rusqlite::params_from_iter(&params),
                        |row| row.get(0),
                    ).log_internal("get_analytics:09")?;

                    let mut stmt = conn
                        .prepare(
                            "SELECT file_path, COUNT(*) as cnt FROM bugs \
                         WHERE file_path IS NOT NULL AND (?1 = '' OR project = ?1) \
                         GROUP BY file_path ORDER BY cnt DESC LIMIT 5",
                        )
                        .log_internal("get_analytics:10")?;
                    let bugs_by_file: Vec<FileBugCount> = stmt
                        .query_map([&Param::Text(project.clone())], |row| {
                            Ok(FileBugCount {
                                file_path: row.get(0)?,
                                bug_count: row.get(1)?,
                            })
                        })
                        .log_internal("get_analytics:11")?
                        .filter_map(|r| r.ok())
                        .collect();

                    (
                        tool_call_breakdown,
                        top_read_files,
                        top_written_files,
                        bug_count,
                        bugs_by_file,
                    )
                };

            // --- Savings + Overhead (always computed) ---
            // Anatomy count is cheap and always useful for interpreting savings.
            let files_with_anatomy: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM file_anatomy WHERE (?1 = '' OR project = ?1)",
                    [&Param::Text(project.clone())],
                    |row| row.get(0),
                )
                .log_internal("get_analytics:12")?;

            let total_file_reads: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM session_reads sr \
                 JOIN sessions s ON sr.session_id = s.session_id \
                 WHERE sr.read_at >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2)",
                    rusqlite::params_from_iter(&params),
                    |row| row.get(0),
                )
                .log_internal("get_analytics:13")?;

            let repeated_reads: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM ( \
                       SELECT sr.session_id, sr.file_path FROM session_reads sr \
                       JOIN sessions s ON sr.session_id = s.session_id \
                       WHERE sr.read_at >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2) \
                       GROUP BY sr.session_id, sr.file_path HAVING COUNT(*) > 1 \
                     )",
                    rusqlite::params_from_iter(&params),
                    |row| row.get(0),
                )
                .log_internal("get_analytics:14")?;

            // Anatomy utilization: count distinct (session, file) pairs in the
            // window, plus a single-pass aggregate that gives both the
            // single-read token weight and the all-pairs token weight.
            let unique_pair_reads: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM ( \
                       SELECT DISTINCT sr.session_id, sr.file_path FROM session_reads sr \
                       JOIN sessions s ON sr.session_id = s.session_id \
                       WHERE sr.read_at >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2) \
                     )",
                    rusqlite::params_from_iter(&params),
                    |row| row.get(0),
                )
                .log_internal("get_analytics:14a")?;

            let (anatomy_token_weight_used, anatomy_token_weight_total): (i64, i64) = conn
                .query_row(
                    "SELECT \
                       COALESCE(SUM(CASE WHEN read_count = 1 THEN tok ELSE 0 END), 0), \
                       COALESCE(SUM(tok), 0) \
                     FROM ( \
                       SELECT MAX(sr.token_estimate) AS tok, COUNT(*) AS read_count \
                       FROM session_reads sr \
                       JOIN sessions s ON sr.session_id = s.session_id \
                       WHERE sr.read_at >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2) \
                       GROUP BY sr.session_id, sr.file_path \
                     )",
                    rusqlite::params_from_iter(&params),
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .log_internal("get_analytics:14b")?;

            let anatomy_used_rate = if total_file_reads > 0 {
                Some(unique_pair_reads as f64 / total_file_reads as f64)
            } else {
                None
            };
            let anatomy_token_coverage_rate = if anatomy_token_weight_total > 0 {
                Some(anatomy_token_weight_used as f64 / anatomy_token_weight_total as f64)
            } else {
                None
            };

            // MIN(id) subquery stays global so a re-read inside the window still
            // counts as saveable when its first read happened outside the window.
            let saveable: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(sr.token_estimate), 0) FROM session_reads sr \
                 JOIN sessions s ON sr.session_id = s.session_id \
                 WHERE sr.read_at >= datetime('now', ?1) AND (?2 = '' OR s.project = ?2) \
                   AND sr.id NOT IN (SELECT MIN(id) FROM session_reads GROUP BY session_id, file_path)",
                    rusqlite::params_from_iter(&params),
                    |row| row.get(0),
                )
                .log_internal("get_analytics:15")?;

            let overhead_tokens: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(estimated_tokens), 0) FROM mnemosyne_overhead \
                 WHERE emitted_at >= datetime('now', ?1) AND (?2 = '' OR project = ?2)",
                    rusqlite::params_from_iter(&params),
                    |row| row.get(0),
                )
                .log_internal("get_analytics:16")?;

            // Overhead breakdown by hook — distribution stats let callers spot
            // outliers (a heavy briefing project runs avg well above the mean).
            let mut stmt = conn
                .prepare(
                    "SELECT hook_name, COUNT(*), \
                        COALESCE(SUM(estimated_tokens), 0), \
                        COALESCE(AVG(estimated_tokens), 0.0), \
                        COALESCE(MIN(estimated_tokens), 0), \
                        COALESCE(MAX(estimated_tokens), 0), \
                        COALESCE(AVG(estimated_tokens * estimated_tokens), 0.0) \
                 FROM mnemosyne_overhead \
                 WHERE emitted_at >= datetime('now', ?1) AND (?2 = '' OR project = ?2) \
                 GROUP BY hook_name ORDER BY SUM(estimated_tokens) DESC",
                )
                .log_internal("get_analytics:17")?;
            let overhead_by_hook: Vec<HookOverheadEntry> = stmt
                .query_map(rusqlite::params_from_iter(&params), |row| {
                    let avg: f64 = row.get(3)?;
                    let avg_sq: f64 = row.get(6)?;
                    // Population variance = E[X^2] - E[X]^2. Clamp at 0 to guard
                    // against tiny negative values from float drift.
                    let variance = (avg_sq - avg * avg).max(0.0);
                    Ok(HookOverheadEntry {
                        hook_name: row.get(0)?,
                        invocations: row.get(1)?,
                        estimated_tokens: row.get(2)?,
                        avg_tokens: avg,
                        min_tokens: row.get(4)?,
                        max_tokens: row.get(5)?,
                        stddev_tokens: variance.sqrt(),
                    })
                })
                .log_internal("get_analytics:18")?
                .filter_map(|r| r.ok())
                .collect();

            // Top 5 sessions by total tokens (always useful alongside token stats)
            let mut stmt = conn.prepare(
                "SELECT session_id, project, (total_input_tokens + total_output_tokens) as total, start_time \
                 FROM sessions WHERE start_time >= datetime('now', ?1) AND (?2 = '' OR project = ?2) \
                 ORDER BY total DESC LIMIT 5"
            ).log_internal("get_analytics:19")?;
            let top_sessions: Vec<TokenSessionEntry> = stmt
                .query_map(rusqlite::params_from_iter(&params), |row| {
                    Ok(TokenSessionEntry {
                        session_id: row.get(0)?,
                        project: row.get(1)?,
                        total_tokens: row.get(2)?,
                        start_time: row.get(3)?,
                    })
                })
                .log_internal("get_analytics:20")?
                .filter_map(|r| r.ok())
                .collect();

            // --- Memory Health (skipped when section="tokens") ---
            let (
                context_items_by_category,
                total_dnr,
                total_bugs,
                oldest_context,
                projects_with,
                projects_without,
            ) = if tokens_only {
                (Vec::new(), 0, 0, None, Vec::new(), Vec::new())
            } else {
                let mut stmt = conn
                    .prepare(
                        "SELECT category, COUNT(*) FROM context_items \
                     WHERE (?1 = '' OR project = ?1) GROUP BY category ORDER BY COUNT(*) DESC",
                    )
                    .log_internal("get_analytics:21")?;
                let context_items_by_category: Vec<CategoryCount> = stmt
                    .query_map([&Param::Text(project.clone())], |row| {
                        Ok(CategoryCount {
                            category: row.get(0)?,
                            count: row.get(1)?,
                        })
                    })
                    .log_internal("get_analytics:22")?
                    .filter_map(|r| r.ok())
                    .collect();

                let total_dnr: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM do_not_repeat WHERE (?1 = '' OR project = ?1)",
                        [&Param::Text(project.clone())],
                        |row| row.get(0),
                    )
                    .log_internal("get_analytics:23")?;

                let total_bugs: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM bugs WHERE (?1 = '' OR project = ?1)",
                        [&Param::Text(project.clone())],
                        |row| row.get(0),
                    )
                    .log_internal("get_analytics:24")?;

                let oldest_context: Option<String> = conn
                    .query_row(
                        "SELECT MIN(created_at) FROM context_items WHERE (?1 = '' OR project = ?1)",
                        [&Param::Text(project.clone())],
                        |row| row.get(0),
                    )
                    .log_internal("get_analytics:25")?;

                let mut stmt = conn
                    .prepare(
                        "SELECT DISTINCT project FROM context_items WHERE project IS NOT NULL",
                    )
                    .log_internal("get_analytics:26")?;
                let projects_with: Vec<String> = stmt
                    .query_map([], |row| row.get(0))
                    .log_internal("get_analytics:27")?
                    .filter_map(|r| r.ok())
                    .collect();

                let mut stmt = conn.prepare(
                    "SELECT DISTINCT project FROM sessions WHERE project IS NOT NULL \
                     AND project NOT IN (SELECT DISTINCT project FROM context_items WHERE project IS NOT NULL)"
                ).log_internal("get_analytics:28")?;
                let projects_without: Vec<String> = stmt
                    .query_map([], |row| row.get(0))
                    .log_internal("get_analytics:29")?
                    .filter_map(|r| r.ok())
                    .collect();

                (
                    context_items_by_category,
                    total_dnr,
                    total_bugs,
                    oldest_context,
                    projects_with,
                    projects_without,
                )
            };

            Ok(Json(AnalyticsReport {
                period_days: days,
                project: input.project,
                section,
                total_sessions,
                total_input_tokens: total_input,
                total_output_tokens: total_output,
                total_cache_read_tokens: cache_read,
                total_cache_creation_tokens: cache_creation,
                avg_input_per_session: avg_input,
                avg_output_per_session: avg_output,
                tool_call_breakdown,
                top_read_files,
                top_written_files,
                bug_count,
                bugs_by_file,
                files_with_anatomy,
                total_file_reads,
                repeated_reads_detected: repeated_reads,
                unique_session_file_reads: unique_pair_reads,
                anatomy_used_rate,
                anatomy_token_weight_used,
                anatomy_token_weight_total,
                anatomy_token_coverage_rate,
                estimated_tokens_saveable: saveable,
                overhead_tokens,
                overhead_by_hook,
                net_savings_tokens: saveable - overhead_tokens,
                top_sessions_by_tokens: top_sessions,
                context_items_by_category,
                total_do_not_repeat_rules: total_dnr,
                total_bugs_logged: total_bugs,
                oldest_context_item: oldest_context,
                projects_with_context: projects_with,
                projects_without_context: projects_without,
            }))
        })
        .await
    }
}

#[tool_handler(
    name = "mnemosyne",
    instructions = "Mnemosyne: Claude Code session memory system. Search past sessions, save context, log bugs, and manage do-not-repeat rules."
)]
impl ServerHandler for MnemosyneServer {}

impl MnemosyneServer {
    #[cfg(test)]
    fn new_with_conn(conn: Connection) -> Self {
        Self {
            db: Arc::new(Mutex::new(conn)),
        }
    }

    fn new() -> Result<Self> {
        let conn = db::open_db()?;
        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    memory_common::logging::init("memory-mcp-server", "info");

    // Startup diagnostics. If a future invocation hangs in the dynamic linker
    // before this line runs (as happened on macOS 26 when the kernel exec
    // cache for /usr/local/bin/memory-mcp-server got wedged), the *absence*
    // of this line in the log file is the signal that startup never reached
    // user code.
    let pid = std::process::id();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<current_exe failed: {e}>"));
    let db_path = memory_common::db::db_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<db_path failed: {e}>"));
    tracing::info!(
        pid,
        version = env!("CARGO_PKG_VERSION"),
        exe = %exe,
        db = %db_path,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "Mnemosyne MCP server starting"
    );

    let server = match MnemosyneServer::new() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "failed to construct MnemosyneServer");
            return Err(e);
        }
    };
    // `db_handle` lets us run the explicit WAL checkpoint on shutdown after the
    // rmcp service has taken ownership of `server`.
    let db_handle = server.db.clone();
    let shutdown = Arc::new(Notify::new());

    // Cross-platform signal handler: any OS shutdown signal triggers graceful exit.
    let shutdown_sig = shutdown.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        shutdown_sig.notify_waiters();
    });

    // Run the service; always run the shutdown checkpoint regardless of
    // whether we exit via stdin EOF, a signal, or a startup error (e.g.
    // parent closed stdin before sending `initialize`).
    let result = run_service(server, &shutdown).await;

    match db_handle.lock() {
        Ok(conn) => {
            if let Err(e) = db::checkpoint_wal(&conn) {
                tracing::warn!("WAL checkpoint on shutdown failed: {e}");
            } else {
                tracing::info!("WAL checkpointed, exiting cleanly");
            }
        }
        Err(e) => tracing::warn!("could not lock DB for shutdown checkpoint: {e}"),
    }

    result
}

/// Runs the rmcp stdio service until either the transport terminates or the
/// shutdown notifier fires. Separated from main() so the caller can always run
/// the WAL checkpoint on the way out.
async fn run_service(server: MnemosyneServer, shutdown: &Notify) -> Result<()> {
    let service = match server.serve(rmcp::transport::stdio()).await {
        Ok(s) => s,
        Err(e) => return Err(anyhow::anyhow!("failed to start MCP service: {e}")),
    };
    tokio::select! {
        res = service.waiting() => {
            res.map_err(|e| anyhow::anyhow!("MCP service error: {e}"))?;
        }
        _ = shutdown.notified() => {
            tracing::info!("shutdown requested, draining");
        }
    }
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

    #[tokio::test]
    async fn test_save_context_and_search() {
        let server = test_server();
        let save_result = server
            .save_context(Parameters(SaveContextInput {
                content: "Arena allocators prevent drop-time regression".to_string(),
                category: "architecture".to_string(),
                project: Some("test_proj".to_string()),
                ..Default::default()
            }))
            .await;
        assert!(save_result.is_ok());

        let search_result = server
            .search_context(Parameters(SearchContextInput {
                query: "arena allocators".to_string(),
                category: None,
                project: None,
                limit: None,
            }))
            .await;
        let Json(list) = search_result.unwrap();
        assert_eq!(list.results.len(), 1);
        assert!(list.results[0].content.contains("Arena allocators"));
    }

    #[tokio::test]
    async fn test_save_context_empty_rejected() {
        let server = test_server();
        let result = server
            .save_context(Parameters(SaveContextInput {
                content: "".to_string(),
                category: "test".to_string(),
                ..Default::default()
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_save_context_truncation() {
        let server = test_server();
        let long_content = "x".repeat(20_000);
        let result = server
            .save_context(Parameters(SaveContextInput {
                content: long_content,
                category: "test".to_string(),
                ..Default::default()
            }))
            .await;
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

    #[tokio::test]
    async fn test_log_bug_and_search() {
        let server = test_server();
        let log_result = server
            .log_bug(Parameters(LogBugInput {
                error_message: "index out of bounds".to_string(),
                fix_description: "check array length first".to_string(),
                root_cause: Some("missing bounds check".to_string()),
                tags: Some("safety".to_string()),
                file_path: Some("src/main.rs".to_string()),
                project: Some("test_proj".to_string()),
                ..Default::default()
            }))
            .await;
        assert!(log_result.is_ok());

        let search_result = server
            .search_bugs(Parameters(SearchBugsInput {
                query: "index out of bounds".to_string(),
                tags: None,
                project: None,
            }))
            .await;
        let Json(list) = search_result.unwrap();
        assert_eq!(list.results.len(), 1);
        assert_eq!(list.results[0].error_message, "index out of bounds");
    }

    #[tokio::test]
    async fn test_log_bug_with_tags_filter() {
        let server = test_server();
        server
            .log_bug(Parameters(LogBugInput {
                error_message: "perf regression".to_string(),
                fix_description: "use arena".to_string(),
                tags: Some("perf,memory".to_string()),
                ..Default::default()
            }))
            .await
            .unwrap();

        let result = server
            .search_bugs(Parameters(SearchBugsInput {
                query: "regression".to_string(),
                tags: Some("perf".to_string()),
                project: None,
            }))
            .await;
        let Json(list) = result.unwrap();
        assert_eq!(list.results.len(), 1);
    }

    #[tokio::test]
    async fn test_log_bug_empty_rejected() {
        let server = test_server();
        let result = server
            .log_bug(Parameters(LogBugInput {
                error_message: "".to_string(),
                fix_description: "some fix".to_string(),
                ..Default::default()
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_add_and_get_do_not_repeat() {
        let server = test_server();
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "Don't use individual Vec<u8>".to_string(),
                reason: Some("causes drop-time regression".to_string()),
                project: Some("test_proj".to_string()),
                file_path: None,
            }))
            .await
            .unwrap();

        let Json(list) = server
            .get_do_not_repeat(Parameters(GetDoNotRepeatInput {
                project: Some("test_proj".to_string()),
                file_path: None,
            }))
            .await
            .unwrap();
        assert_eq!(list.results.len(), 1);
        assert!(list.results[0].rule.contains("Vec<u8>"));
    }

    #[tokio::test]
    async fn test_get_do_not_repeat_file_filter() {
        let server = test_server();
        // Global rule (no file_path)
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "global rule".to_string(),
                reason: None,
                project: Some("proj".to_string()),
                file_path: None,
            }))
            .await
            .unwrap();
        // Scoped rule
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "scoped rule".to_string(),
                reason: None,
                project: Some("proj".to_string()),
                file_path: Some("src/main.rs".to_string()),
            }))
            .await
            .unwrap();

        let Json(list) = server
            .get_do_not_repeat(Parameters(GetDoNotRepeatInput {
                project: Some("proj".to_string()),
                file_path: Some("src/main.rs".to_string()),
            }))
            .await
            .unwrap();
        // Should return both global (file_path IS NULL) and scoped rules
        assert_eq!(list.results.len(), 2);
    }

    #[tokio::test]
    async fn test_global_rules_visible_with_project_filter() {
        let server = test_server();
        // Global rule (no project)
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "global rule".to_string(),
                reason: None,
                project: None,
                file_path: None,
            }))
            .await
            .unwrap();
        // Project-scoped rule
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "project rule".to_string(),
                reason: None,
                project: Some("myproj".to_string()),
                file_path: None,
            }))
            .await
            .unwrap();

        // Filtering by project should return both global + project-scoped
        let Json(list) = server
            .get_do_not_repeat(Parameters(GetDoNotRepeatInput {
                project: Some("myproj".to_string()),
                file_path: None,
            }))
            .await
            .unwrap();
        assert_eq!(list.results.len(), 2);

        // project_summary should also include global rules
        let Json(summary) = server
            .get_project_summary(Parameters(GetProjectSummaryInput {
                project: Some("myproj".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(summary.do_not_repeat.len(), 2);

        // Different project should only see global rule
        let Json(list) = server
            .get_do_not_repeat(Parameters(GetDoNotRepeatInput {
                project: Some("other".to_string()),
                file_path: None,
            }))
            .await
            .unwrap();
        assert_eq!(list.results.len(), 1);
        assert_eq!(list.results[0].rule, "global rule");
    }

    #[tokio::test]
    async fn test_add_do_not_repeat_scope_message() {
        let server = test_server();
        // Global
        let Json(result) = server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "test".to_string(),
                reason: None,
                project: None,
                file_path: None,
            }))
            .await
            .unwrap();
        assert!(result.message.contains("GLOBAL"));

        // Project-scoped
        let Json(result) = server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "test".to_string(),
                reason: None,
                project: Some("myproj".to_string()),
                file_path: None,
            }))
            .await
            .unwrap();
        assert!(result.message.contains("project=myproj"));
        assert!(!result.message.contains("GLOBAL"));
    }

    #[tokio::test]
    async fn test_get_project_summary_empty() {
        let server = test_server();
        let Json(summary) = server
            .get_project_summary(Parameters(GetProjectSummaryInput {
                project: Some("nonexistent".to_string()),
            }))
            .await
            .unwrap();
        assert!(summary.context_items.is_empty());
        assert!(summary.recent_bugs.is_empty());
        assert!(summary.do_not_repeat.is_empty());
        assert_eq!(summary.total_sessions, 0);
    }

    #[tokio::test]
    async fn test_get_project_summary_with_data() {
        let server = test_server();
        server
            .save_context(Parameters(SaveContextInput {
                content: "test context".to_string(),
                category: "arch".to_string(),
                project: Some("proj".to_string()),
                ..Default::default()
            }))
            .await
            .unwrap();
        server
            .log_bug(Parameters(LogBugInput {
                error_message: "test bug".to_string(),
                fix_description: "test fix".to_string(),
                project: Some("proj".to_string()),
                ..Default::default()
            }))
            .await
            .unwrap();
        server
            .add_do_not_repeat(Parameters(AddDoNotRepeatInput {
                rule: "test rule".to_string(),
                reason: None,
                project: Some("proj".to_string()),
                file_path: None,
            }))
            .await
            .unwrap();

        let Json(summary) = server
            .get_project_summary(Parameters(GetProjectSummaryInput {
                project: Some("proj".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(summary.context_items.len(), 1);
        assert_eq!(summary.recent_bugs.len(), 1);
        assert_eq!(summary.do_not_repeat.len(), 1);
    }

    #[tokio::test]
    async fn test_get_recent_sessions() {
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
            .await
            .unwrap();
        assert_eq!(list.results.len(), 2);
        // Most recent first
        assert_eq!(list.results[0].session_id, "s2");
    }

    #[tokio::test]
    async fn test_get_session_detail() {
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
            .await
            .unwrap();
        assert_eq!(detail.session_id, "sd1");
        assert_eq!(detail.first_user_message, Some("first message".to_string()));
        assert_eq!(detail.last_user_message, Some("last message".to_string()));
        assert_eq!(detail.tool_summary.len(), 1);
        assert_eq!(detail.tool_summary[0].tool_name, "Read");
        assert_eq!(detail.tool_summary[0].count, 2);
    }

    #[tokio::test]
    async fn test_search_sessions_fts() {
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
            .await
            .unwrap();
        assert_eq!(list.results.len(), 1);
        assert_eq!(list.results[0].session_id, "fts1");
    }

    #[tokio::test]
    async fn test_get_file_history() {
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
            .await
            .unwrap();
        assert_eq!(list.results.len(), 1);
        assert_eq!(list.results[0].tool_name, "Edit");
    }

    // --- Analytics tool tests ---

    #[tokio::test]
    async fn test_get_analytics_tokens_section_empty() {
        let server = test_server();
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(30),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(report.section, "tokens");
        assert_eq!(report.total_sessions, 0);
        assert_eq!(report.total_input_tokens, 0);
        assert_eq!(report.total_output_tokens, 0);
        assert!(report.top_sessions_by_tokens.is_empty());
        // tokens section skips productivity + memory health
        assert!(report.tool_call_breakdown.is_empty());
        assert!(report.context_items_by_category.is_empty());
    }

    #[tokio::test]
    async fn test_get_analytics_tokens_section_with_data() {
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
            .get_analytics(Parameters(GetAnalyticsInput {
                project: Some("proj".to_string()),
                days: Some(7),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(report.total_sessions, 1);
        assert_eq!(report.total_input_tokens, 5000);
        assert_eq!(report.total_output_tokens, 3000);
        assert_eq!(report.total_cache_read_tokens, 1000);
        assert_eq!(report.total_cache_creation_tokens, 500);
        assert_eq!(report.avg_input_per_session, 5000);
        assert_eq!(report.avg_output_per_session, 3000);
        assert_eq!(report.files_with_anatomy, 1);
        assert_eq!(report.total_file_reads, 2);
        assert_eq!(report.repeated_reads_detected, 1);
        assert_eq!(report.estimated_tokens_saveable, 200);
        assert_eq!(report.top_sessions_by_tokens.len(), 1);
        assert_eq!(report.top_sessions_by_tokens[0].total_tokens, 8000);
        // tokens section still skips productivity + memory health
        assert!(report.tool_call_breakdown.is_empty());
        assert!(report.context_items_by_category.is_empty());
    }

    #[tokio::test]
    async fn test_get_analytics_empty() {
        let server = test_server();
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(30),
                section: None,
            }))
            .await
            .unwrap();
        assert_eq!(report.total_sessions, 0);
        assert!(report.tool_call_breakdown.is_empty());
        assert!(report.top_read_files.is_empty());
        assert!(report.context_items_by_category.is_empty());
        assert!(report.projects_with_context.is_empty());
    }

    #[tokio::test]
    async fn test_get_analytics_with_data() {
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
                section: None,
            }))
            .await
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

    // --- Regression guards for the filter-bug fix on session_reads-derived metrics.
    //     Before the fix, total_file_reads / repeated_reads_detected / estimated_tokens_saveable
    //     all ignored the `days` and `project` inputs.

    #[tokio::test]
    async fn test_get_analytics_tokens_filter_excludes_reads_outside_window() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions (session_id, project, start_time, message_count) \
                 VALUES ('old', 'proj', datetime('now', '-60 days'), 0)",
                [],
            )
            .unwrap();
            // Two reads of the same file 60 days ago — a repeat pair, but outside any
            // 7d/30d window. They must not count in any of the filtered metrics.
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('old', 'src/old.rs', datetime('now', '-60 days'), 500)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('old', 'src/old.rs', datetime('now', '-60 days'), 500)",
                [],
            )
            .unwrap();
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(30),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(report.total_file_reads, 0, "60d-old reads leaked into 30d");
        assert_eq!(report.repeated_reads_detected, 0);
        assert_eq!(report.estimated_tokens_saveable, 0);
    }

    #[tokio::test]
    async fn test_get_analytics_tokens_filter_respects_project() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            // Two sessions in-window, different projects. Each has a repeat pair.
            for (sid, proj) in [("s_a", "alpha"), ("s_b", "beta")] {
                conn.execute(
                    "INSERT INTO sessions (session_id, project, start_time, message_count) \
                     VALUES (?1, ?2, datetime('now'), 0)",
                    rusqlite::params![sid, proj],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                     VALUES (?1, 'src/f.rs', datetime('now', '-2 minutes'), 300)",
                    [sid],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                     VALUES (?1, 'src/f.rs', datetime('now'), 300)",
                    [sid],
                )
                .unwrap();
            }
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: Some("alpha".to_string()),
                days: Some(7),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(
            report.total_file_reads, 2,
            "beta's reads leaked through project filter"
        );
        assert_eq!(report.repeated_reads_detected, 1);
        assert_eq!(report.estimated_tokens_saveable, 300);
    }

    #[tokio::test]
    async fn test_get_analytics_includes_overhead_and_net_savings() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions (session_id, project, start_time, message_count) \
                 VALUES ('s1', 'proj', datetime('now'), 0)",
                [],
            )
            .unwrap();
            // Saveable: 400 tokens (second read of same file)
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('s1', 'a.rs', datetime('now', '-2 minutes'), 400)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('s1', 'a.rs', datetime('now'), 400)",
                [],
            )
            .unwrap();
            // Overhead: three hook invocations
            conn.execute(
                "INSERT INTO mnemosyne_overhead (session_id, project, hook_name, output_bytes, estimated_tokens, emitted_at) \
                 VALUES ('s1', 'proj', 'session_start', 350, 100, datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO mnemosyne_overhead (session_id, project, hook_name, output_bytes, estimated_tokens, emitted_at) \
                 VALUES ('s1', 'proj', 'pre_read', 70, 20, datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO mnemosyne_overhead (session_id, project, hook_name, output_bytes, estimated_tokens, emitted_at) \
                 VALUES ('s1', 'proj', 'pre_read', 70, 20, datetime('now'))",
                [],
            ).unwrap();
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: Some("proj".to_string()),
                days: Some(7),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(report.estimated_tokens_saveable, 400);
        assert_eq!(report.overhead_tokens, 140);
        assert_eq!(report.net_savings_tokens, 260, "net = saveable - overhead");

        // Breakdown is sorted by SUM(estimated_tokens) DESC
        assert_eq!(report.overhead_by_hook.len(), 2);
        let ss = &report.overhead_by_hook[0];
        assert_eq!(ss.hook_name, "session_start");
        assert_eq!(ss.invocations, 1);
        assert_eq!(ss.estimated_tokens, 100);
        // Single invocation → avg=min=max=value, stddev=0
        assert_eq!(ss.avg_tokens, 100.0);
        assert_eq!(ss.min_tokens, 100);
        assert_eq!(ss.max_tokens, 100);
        assert_eq!(ss.stddev_tokens, 0.0);

        let pr = &report.overhead_by_hook[1];
        assert_eq!(pr.hook_name, "pre_read");
        assert_eq!(pr.invocations, 2);
        assert_eq!(pr.estimated_tokens, 40);
        // Two identical values → stddev=0
        assert_eq!(pr.avg_tokens, 20.0);
        assert_eq!(pr.min_tokens, 20);
        assert_eq!(pr.max_tokens, 20);
        assert_eq!(pr.stddev_tokens, 0.0);
    }

    /// Three invocations with spread values → avg, min, max, and stddev all
    /// take meaningful values. Lets a caller judge how much the average session
    /// deviates from the typical briefing cost.
    #[tokio::test]
    async fn test_get_analytics_overhead_distribution_stats() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            for tokens in [10i64, 20, 30] {
                conn.execute(
                    "INSERT INTO mnemosyne_overhead (session_id, project, hook_name, output_bytes, estimated_tokens, emitted_at) \
                     VALUES ('s1', 'proj', 'session_start', ?1, ?1, datetime('now'))",
                    [tokens],
                ).unwrap();
            }
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: Some("proj".to_string()),
                days: Some(7),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(report.overhead_by_hook.len(), 1);
        let ss = &report.overhead_by_hook[0];
        assert_eq!(ss.invocations, 3);
        assert_eq!(ss.estimated_tokens, 60);
        assert_eq!(ss.avg_tokens, 20.0);
        assert_eq!(ss.min_tokens, 10);
        assert_eq!(ss.max_tokens, 30);
        // Population variance of {10,20,30} = ((10-20)^2 + 0 + (30-20)^2)/3 = 200/3
        // stddev = sqrt(66.666...) ≈ 8.1650
        let expected = (200.0_f64 / 3.0).sqrt();
        assert!(
            (ss.stddev_tokens - expected).abs() < 1e-9,
            "stddev {} != expected {}",
            ss.stddev_tokens,
            expected
        );
    }

    #[tokio::test]
    async fn test_get_analytics_excludes_overhead_outside_window() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute(
                "INSERT INTO mnemosyne_overhead (session_id, project, hook_name, output_bytes, estimated_tokens, emitted_at) \
                 VALUES ('s1', 'proj', 'session_start', 350, 100, datetime('now', '-60 days'))",
                [],
            ).unwrap();
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(30),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(report.overhead_tokens, 0);
        assert!(report.overhead_by_hook.is_empty());
    }

    #[tokio::test]
    async fn test_get_analytics_filter_excludes_reads_outside_window() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions (session_id, project, start_time, message_count) \
                 VALUES ('old', 'proj', datetime('now', '-60 days'), 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('old', 'src/old.rs', datetime('now', '-60 days'), 500)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('old', 'src/old.rs', datetime('now', '-60 days'), 500)",
                [],
            )
            .unwrap();
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(30),
                section: None,
            }))
            .await
            .unwrap();
        assert_eq!(report.total_file_reads, 0);
        assert_eq!(report.repeated_reads_detected, 0);
        assert_eq!(report.estimated_tokens_saveable, 0);
    }

    #[tokio::test]
    async fn test_get_analytics_filter_respects_project() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            for (sid, proj) in [("s_a", "alpha"), ("s_b", "beta")] {
                conn.execute(
                    "INSERT INTO sessions (session_id, project, start_time, message_count) \
                     VALUES (?1, ?2, datetime('now'), 0)",
                    rusqlite::params![sid, proj],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                     VALUES (?1, 'src/f.rs', datetime('now', '-2 minutes'), 300)",
                    [sid],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                     VALUES (?1, 'src/f.rs', datetime('now'), 300)",
                    [sid],
                )
                .unwrap();
            }
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: Some("alpha".to_string()),
                days: Some(7),
                section: None,
            }))
            .await
            .unwrap();
        assert_eq!(report.total_file_reads, 2);
        assert_eq!(report.repeated_reads_detected, 1);
        assert_eq!(report.estimated_tokens_saveable, 300);
    }

    /// Saveable counts a repeat even when the FIRST read is outside the window —
    /// anatomy is still "saving" the in-window re-read. Guards against accidentally
    /// filtering the MIN(id) subquery by date.
    #[tokio::test]
    async fn test_get_analytics_saveable_counts_repeat_when_first_read_predates_window() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions (session_id, project, start_time, message_count) \
                 VALUES ('s1', 'proj', datetime('now', '-60 days'), 0)",
                [],
            )
            .unwrap();
            // First read 60 days ago (outside any practical window), second today.
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('s1', 'src/f.rs', datetime('now', '-60 days'), 800)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('s1', 'src/f.rs', datetime('now'), 800)",
                [],
            )
            .unwrap();
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(7),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        // In-window reads = 1 (the re-read). Repeat pair count = 0 because only one
        // row falls inside the window so the HAVING COUNT(*)>1 check doesn't trip.
        // But saveable SHOULD count the in-window re-read (800) because MIN(id) is
        // the first read, which happens to be outside the window.
        assert_eq!(report.total_file_reads, 1);
        assert_eq!(report.estimated_tokens_saveable, 800);
    }

    /// Three (session, file) pairs in window:
    ///   - (s1, a.rs) read once,  500 tokens   → counts toward "anatomy used"
    ///   - (s1, b.rs) read twice, 1000 tokens  → repeat, NOT counted
    ///   - (s2, c.rs) read once,  100 tokens   → counts toward "anatomy used"
    /// Total reads = 4 (1 + 2 + 1). Unique pairs = 3.
    /// Used token weight = 500 + 100 = 600. Total token weight = 500 + 1000 + 100 = 1600.
    #[tokio::test]
    async fn test_get_analytics_anatomy_utilization_metrics() {
        let server = test_server();
        {
            let conn = server.db.lock().unwrap();
            for sid in ["s1", "s2"] {
                conn.execute(
                    "INSERT INTO sessions (session_id, project, start_time, message_count) \
                     VALUES (?1, 'proj', datetime('now'), 0)",
                    [sid],
                )
                .unwrap();
            }
            // (s1, a.rs) — single read
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('s1', 'src/a.rs', datetime('now'), 500)",
                [],
            )
            .unwrap();
            // (s1, b.rs) — two reads
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('s1', 'src/b.rs', datetime('now'), 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('s1', 'src/b.rs', datetime('now'), 1000)",
                [],
            )
            .unwrap();
            // (s2, c.rs) — single read in a different session
            conn.execute(
                "INSERT INTO session_reads (session_id, file_path, read_at, token_estimate) \
                 VALUES ('s2', 'src/c.rs', datetime('now'), 100)",
                [],
            )
            .unwrap();
        }
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(30),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(report.total_file_reads, 4);
        assert_eq!(report.unique_session_file_reads, 3);
        assert_eq!(report.repeated_reads_detected, 1);
        assert!(matches!(report.anatomy_used_rate, Some(r) if (r - 0.75).abs() < 1e-9));
        assert_eq!(report.anatomy_token_weight_used, 600);
        assert_eq!(report.anatomy_token_weight_total, 1600);
        assert!(matches!(report.anatomy_token_coverage_rate, Some(r) if (r - 0.375).abs() < 1e-9));
    }

    /// With no reads in the window, both rates are `None` (no data, not 0/0).
    #[tokio::test]
    async fn test_get_analytics_anatomy_utilization_none_when_no_reads() {
        let server = test_server();
        let Json(report) = server
            .get_analytics(Parameters(GetAnalyticsInput {
                project: None,
                days: Some(30),
                section: Some("tokens".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(report.total_file_reads, 0);
        assert_eq!(report.unique_session_file_reads, 0);
        assert!(report.anatomy_used_rate.is_none());
        assert_eq!(report.anatomy_token_weight_total, 0);
        assert!(report.anatomy_token_coverage_rate.is_none());
    }

    // --- Defensive shutdown / timeout tests ---

    /// A handler whose blocking work exceeds the timeout returns an error
    /// instead of hanging the stdio service.
    #[tokio::test]
    async fn test_handler_timeout_returns_error() {
        let server = test_server();
        let res: Result<i32, _> = server
            .run_db_with_timeout("test", Duration::from_millis(50), |_conn| {
                // Simulate a wedged SQL call by sleeping past the deadline.
                std::thread::sleep(Duration::from_millis(500));
                Ok(42)
            })
            .await;
        assert!(res.is_err(), "expected timeout error, got Ok");
        let err_str = format!("{:?}", res.unwrap_err());
        assert!(
            err_str.contains("timed out"),
            "unexpected error message: {err_str}"
        );
    }
}
