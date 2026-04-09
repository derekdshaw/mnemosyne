use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    pub project: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub message_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub uuid: String,
    pub session_id: String,
    pub parent_uuid: Option<String>,
    pub role: String,
    pub content_type: Option<String>,
    pub content: Option<String>,
    pub tool_name: Option<String>,
    pub timestamp: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: Option<i64>,
    pub message_uuid: String,
    pub session_id: String,
    pub tool_name: String,
    pub tool_input_summary: Option<String>,
    pub file_path: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub message_uuid: String,
    pub session_id: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: Option<i64>,
    pub project: Option<String>,
    pub category: String,
    pub content: String,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub source_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bug {
    pub id: Option<i64>,
    pub project: Option<String>,
    pub error_message: String,
    pub root_cause: Option<String>,
    pub fix_description: String,
    pub tags: Option<String>,
    pub file_path: Option<String>,
    pub created_at: String,
    pub source_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionRecord {
    pub file_path: String,
    pub ingested_at: String,
    pub line_count: i64,
    pub file_size: i64,
    pub file_mtime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAnatomy {
    pub project: String,
    pub file_path: String,
    pub description: Option<String>,
    pub estimated_tokens: Option<i64>,
    pub last_modified: Option<String>,
    pub last_scanned: Option<String>,
    pub times_read: i64,
    pub times_written: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRead {
    pub id: Option<i64>,
    pub session_id: String,
    pub file_path: String,
    pub read_at: String,
    pub token_estimate: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoNotRepeat {
    pub id: Option<i64>,
    pub project: Option<String>,
    pub rule: String,
    pub reason: Option<String>,
    pub file_path: Option<String>,
    pub created_at: String,
    pub source_session_id: Option<String>,
}
