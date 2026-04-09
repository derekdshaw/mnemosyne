use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// --- Tool Input Structs ---

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct SearchSessionsInput {
    /// Full-text search query
    pub query: String,
    /// Maximum results to return
    pub limit: Option<i64>,
    /// Filter by project name
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct GetRecentSessionsInput {
    /// Number of days to look back (default 7)
    pub days: Option<i64>,
    /// Filter by project name
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct GetSessionDetailInput {
    /// Session ID to get details for
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct GetFileHistoryInput {
    /// File path to search for (partial match)
    pub file_path: Option<String>,
    /// Filter by project name
    pub project: Option<String>,
    /// Number of days to look back
    pub days: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct SaveContextInput {
    /// The context content to save
    pub content: String,
    /// Category: architecture, performance, conventions, etc.
    pub category: String,
    /// Project name (derived from cwd if not provided)
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct SearchContextInput {
    /// Full-text search query
    pub query: String,
    /// Filter by category
    pub category: Option<String>,
    /// Filter by project
    pub project: Option<String>,
    /// Maximum results
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct GetProjectSummaryInput {
    /// Project name (uses all projects if not specified)
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct LogBugInput {
    /// The error message
    pub error_message: String,
    /// How the bug was fixed
    pub fix_description: String,
    /// Root cause analysis
    pub root_cause: Option<String>,
    /// Comma-separated tags
    pub tags: Option<String>,
    /// File path where the bug was found
    pub file_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct SearchBugsInput {
    /// Full-text search query
    pub query: String,
    /// Filter by tags (comma-separated)
    pub tags: Option<String>,
    /// Filter by project
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct AddDoNotRepeatInput {
    /// The rule describing what not to do
    pub rule: String,
    /// Why this should not be repeated
    pub reason: Option<String>,
    /// Project scope
    pub project: Option<String>,
    /// File path scope (optional)
    pub file_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct GetDoNotRepeatInput {
    /// Filter by project
    pub project: Option<String>,
    /// Filter by file path
    pub file_path: Option<String>,
}

// --- Output Structs ---

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionResult {
    pub session_id: String,
    pub project: Option<String>,
    pub start_time: Option<String>,
    pub message_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub matching_excerpt: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionDetail {
    pub session_id: String,
    pub project: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub message_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub first_user_message: Option<String>,
    pub last_user_message: Option<String>,
    pub tool_summary: Vec<ToolSummaryEntry>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ToolSummaryEntry {
    pub tool_name: String,
    pub count: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FileHistoryEntry {
    pub session_id: String,
    pub project: Option<String>,
    pub tool_name: String,
    pub tool_input_summary: Option<String>,
    pub file_path: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextItemResult {
    pub id: i64,
    pub project: Option<String>,
    pub category: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProjectSummary {
    pub project: Option<String>,
    pub context_items: Vec<ContextItemResult>,
    pub recent_bugs: Vec<BugResult>,
    pub do_not_repeat: Vec<DoNotRepeatResult>,
    pub total_sessions: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BugResult {
    pub id: i64,
    pub error_message: String,
    pub root_cause: Option<String>,
    pub fix_description: String,
    pub tags: Option<String>,
    pub file_path: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DoNotRepeatResult {
    pub id: i64,
    pub rule: String,
    pub reason: Option<String>,
    pub file_path: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SimpleResult {
    pub success: bool,
    pub message: String,
}

// Wrapper types for Vec results (MCP requires root type 'object', not 'array')
#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionResultList {
    pub results: Vec<SessionResult>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FileHistoryList {
    pub results: Vec<FileHistoryEntry>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextItemList {
    pub results: Vec<ContextItemResult>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BugResultList {
    pub results: Vec<BugResult>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DoNotRepeatList {
    pub results: Vec<DoNotRepeatResult>,
}
