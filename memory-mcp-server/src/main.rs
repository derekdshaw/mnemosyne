mod tools;

use anyhow::Result;
use memory_common::db;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ServerInfo;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use rusqlite::Connection;
use std::sync::Mutex;
use tools::*;

struct MnemosyneServer {
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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let limit = input.limit.unwrap_or(10);

        let mut sql = String::from(
            "SELECT m.session_id, s.project, s.start_time, s.message_count, \
             s.total_input_tokens, s.total_output_tokens, \
             snippet(messages_fts, 2, '>>>', '<<<', '...', 64) as excerpt \
             FROM messages_fts \
             JOIN messages m ON messages_fts.uuid = m.uuid \
             JOIN sessions s ON m.session_id = s.session_id \
             WHERE messages_fts MATCH ?1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(input.query)];

        if let Some(ref project) = input.project {
            sql.push_str(" AND s.project = ?2");
            params.push(Box::new(project.clone()));
        }

        sql.push_str(" GROUP BY m.session_id ORDER BY s.start_time DESC");
        sql.push_str(&format!(" LIMIT {limit}"));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(param_refs.as_slice(), |row| {
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

        Ok(Json(SessionResultList { results }))
    }

    /// List recent sessions, optionally filtered by project.
    #[tool(name = "get_recent_sessions")]
    fn get_recent_sessions(
        &self,
        Parameters(input): Parameters<GetRecentSessionsInput>,
    ) -> Result<Json<SessionResultList>, rmcp::ErrorData> {
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let days = input.days.unwrap_or(7);

        let mut sql = String::from(
            "SELECT session_id, project, start_time, message_count, \
             total_input_tokens, total_output_tokens \
             FROM sessions WHERE start_time >= datetime('now', ?1)"
        );
        let days_param = format!("-{days} days");
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(days_param)];

        if let Some(ref project) = input.project {
            sql.push_str(" AND project = ?2");
            params.push(Box::new(project.clone()));
        }
        sql.push_str(" ORDER BY start_time DESC LIMIT 50");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(param_refs.as_slice(), |row| {
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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let mut sql = String::from(
            "SELECT tc.session_id, s.project, tc.tool_name, tc.tool_input_summary, \
             tc.file_path, tc.timestamp \
             FROM tool_calls tc \
             JOIN sessions s ON tc.session_id = s.session_id \
             WHERE 1=1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(ref fp) = input.file_path {
            sql.push_str(&format!(" AND tc.file_path LIKE ?{idx}"));
            params.push(Box::new(format!("%{fp}%")));
            idx += 1;
        }
        if let Some(ref project) = input.project {
            sql.push_str(&format!(" AND s.project = ?{idx}"));
            params.push(Box::new(project.clone()));
            idx += 1;
        }
        if let Some(days) = input.days {
            sql.push_str(&format!(" AND tc.timestamp >= datetime('now', ?{idx})"));
            params.push(Box::new(format!("-{days} days")));
        }
        sql.push_str(" ORDER BY tc.timestamp DESC LIMIT 50");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(param_refs.as_slice(), |row| {
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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        conn.execute(
            "INSERT INTO context_items (project, category, content, created_at) \
             VALUES (?1, ?2, ?3, datetime('now'))",
            rusqlite::params![input.project, input.category, input.content],
        )
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO context_fts (item_id, project, category, content) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id.to_string(), input.project, input.category, input.content],
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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let limit = input.limit.unwrap_or(10);

        let mut sql = String::from(
            "SELECT c.id, c.project, c.category, c.content, c.created_at \
             FROM context_fts f \
             JOIN context_items c ON f.item_id = CAST(c.id AS TEXT) \
             WHERE context_fts MATCH ?1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(input.query)];

        if let Some(ref category) = input.category {
            sql.push_str(" AND c.category = ?2");
            params.push(Box::new(category.clone()));
        }
        if let Some(ref project) = input.project {
            sql.push_str(&format!(" AND c.project = ?{}", params.len() + 1));
            params.push(Box::new(project.clone()));
        }
        sql.push_str(&format!(" LIMIT {limit}"));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(param_refs.as_slice(), |row| {
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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let file_path = input.file_path.as_deref().map(db::normalize_path);

        conn.execute(
            "INSERT INTO bugs (project, error_message, root_cause, fix_description, tags, file_path, created_at) \
             VALUES (NULL, ?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params![
                input.error_message,
                input.root_cause,
                input.fix_description,
                input.tags,
                file_path,
            ],
        )
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO bugs_fts (bug_id, project, file_path, error_message, root_cause, fix_description) \
             VALUES (?1, NULL, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                id.to_string(),
                file_path,
                input.error_message,
                input.root_cause,
                input.fix_description,
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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let mut sql = String::from(
            "SELECT b.id, b.error_message, b.root_cause, b.fix_description, b.tags, b.file_path, b.created_at \
             FROM bugs_fts f \
             JOIN bugs b ON f.bug_id = CAST(b.id AS TEXT) \
             WHERE bugs_fts MATCH ?1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(input.query)];

        if let Some(ref project) = input.project {
            sql.push_str(&format!(" AND b.project = ?{}", params.len() + 1));
            params.push(Box::new(project.clone()));
        }
        sql.push_str(" ORDER BY b.created_at DESC LIMIT 20");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(param_refs.as_slice(), |row| {
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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let file_path = input.file_path.as_deref().map(db::normalize_path);

        conn.execute(
            "INSERT INTO do_not_repeat (project, rule, reason, file_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            rusqlite::params![input.project, input.rule, input.reason, file_path],
        )
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO do_not_repeat_fts (dnr_id, project, file_path, rule, reason) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                id.to_string(),
                input.project,
                file_path,
                input.rule,
                input.reason,
            ],
        )
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

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
        let conn = self.db.lock().map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let mut sql = String::from(
            "SELECT id, rule, reason, file_path, created_at FROM do_not_repeat WHERE 1=1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(ref project) = input.project {
            sql.push_str(&format!(" AND project = ?{idx}"));
            params.push(Box::new(project.clone()));
            idx += 1;
        }
        if let Some(ref fp) = input.file_path {
            sql.push_str(&format!(" AND (file_path = ?{idx} OR file_path IS NULL)"));
            params.push(Box::new(db::normalize_path(fp)));
        }
        sql.push_str(" ORDER BY created_at DESC");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        let results = stmt
            .query_map(param_refs.as_slice(), |row| {
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
}

#[tool_handler]
impl ServerHandler for MnemosyneServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.server_info.name = "mnemosyne".into();
        info.server_info.version = "0.1.0".into();
        info.instructions = Some("Mnemosyne: Claude Code session memory system. Search past sessions, \
            save context, log bugs, and manage do-not-repeat rules.".into());
        info
    }
}

impl MnemosyneServer {
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
