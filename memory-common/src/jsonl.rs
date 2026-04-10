use anyhow::Result;
use serde_json::Value;

use crate::db::truncate_utf8;

/// A parsed JSONL transcript record.
#[derive(Debug, Clone)]
pub enum Record {
    UserMessage {
        uuid: String,
        session_id: String,
        parent_uuid: Option<String>,
        cwd: Option<String>,
        git_branch: Option<String>,
        timestamp: Option<String>,
        content: String,
    },
    /// A user message that contains tool results (response to assistant tool_use).
    ToolResult {
        uuid: String,
        session_id: String,
        parent_uuid: Option<String>,
        timestamp: Option<String>,
        results: Vec<ToolResultEntry>,
    },
    AssistantMessage {
        uuid: String,
        session_id: String,
        parent_uuid: Option<String>,
        timestamp: Option<String>,
        model: Option<String>,
        content_blocks: Vec<ContentBlock>,
        usage: Option<UsageInfo>,
    },
    /// Record types we recognize but skip.
    Skip,
}

#[derive(Debug, Clone)]
pub struct ToolResultEntry {
    pub tool_use_id: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Thinking(String),
    ToolUse {
        name: String,
        id: String,
        input: Value,
    },
}

#[derive(Debug, Clone)]
pub struct UsageInfo {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

/// Parse a single JSONL line into a Record.
/// Returns Ok(None) for empty lines, Ok(Some(Record::Skip)) for recognized-but-skipped types.
pub fn parse_line(line: &str) -> Result<Option<Record>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }

    let mut v: Value = serde_json::from_str(line)?;

    let record_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();

    match record_type.as_str() {
        "user" => parse_user_message(&mut v),
        "assistant" => parse_assistant_message(&mut v),
        "system" | "permission-mode" | "file-history-snapshot" | "attachment"
        | "queue-operation" | "custom-title" | "agent-name" => Ok(Some(Record::Skip)),
        _ => Ok(Some(Record::Skip)),
    }
}

fn parse_user_message(v: &mut Value) -> Result<Option<Record>> {
    let uuid = take_str(v, "uuid");
    let session_id = take_str(v, "sessionId");
    let parent_uuid = take_opt_str(v, "parentUuid");
    let cwd = take_opt_str(v, "cwd");
    let git_branch = take_opt_str(v, "gitBranch");
    let timestamp = take_opt_str(v, "timestamp");

    let content = v.get_mut("message").and_then(|m| m.get_mut("content")).map(Value::take);

    match content {
        Some(Value::String(s)) => Ok(Some(Record::UserMessage {
            uuid,
            session_id,
            parent_uuid,
            cwd,
            git_branch,
            timestamp,
            content: s,
        })),
        Some(Value::Array(blocks)) => {
            // Check if this is a tool_result message
            let has_tool_result = blocks.iter().any(|b| {
                b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
            });

            if has_tool_result {
                let results = blocks
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                            return None;
                        }
                        let tool_use_id = b
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let content = extract_tool_result_content(b);
                        Some(ToolResultEntry {
                            tool_use_id,
                            content,
                        })
                    })
                    .collect();
                Ok(Some(Record::ToolResult {
                    uuid,
                    session_id,
                    parent_uuid,
                    timestamp,
                    results,
                }))
            } else {
                // Array of text blocks from user
                let text: String = blocks
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                            b.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Some(Record::UserMessage {
                    uuid,
                    session_id,
                    parent_uuid,
                    cwd,
                    git_branch,
                    timestamp,
                    content: text,
                }))
            }
        }
        _ => Ok(Some(Record::Skip)),
    }
}

fn parse_assistant_message(v: &mut Value) -> Result<Option<Record>> {
    let uuid = take_str(v, "uuid");
    let session_id = take_str(v, "sessionId");
    let parent_uuid = take_opt_str(v, "parentUuid");
    let timestamp = take_opt_str(v, "timestamp");

    let mut message = v.get_mut("message");
    let model = message.as_ref()
        .and_then(|m| m.get("model"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let content_blocks = message.as_mut()
        .and_then(|m| m.get_mut("content"))
        .and_then(|c| c.as_array_mut())
        .map(|blocks| {
            blocks
                .iter_mut()
                .filter_map(|b| parse_content_block(b))
                .collect()
        })
        .unwrap_or_default();

    let usage = v.get("message")
        .and_then(|m| m.get("usage"))
        .map(|u| UsageInfo {
            input_tokens: u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            output_tokens: u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            cache_read_tokens: u
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            cache_creation_tokens: u
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        });

    Ok(Some(Record::AssistantMessage {
        uuid,
        session_id,
        parent_uuid,
        timestamp,
        model,
        content_blocks,
        usage,
    }))
}

fn parse_content_block(b: &mut Value) -> Option<ContentBlock> {
    let block_type = b.get("type").and_then(|t| t.as_str())?.to_string();
    match block_type.as_str() {
        "text" => {
            let text = match b.get_mut("text").map(Value::take) {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            Some(ContentBlock::Text(text))
        }
        "thinking" => {
            let thinking = b.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
            let truncated = truncate_utf8(thinking, 500);
            Some(ContentBlock::Thinking(truncated))
        }
        "tool_use" => {
            let name = match b.get_mut("name").map(Value::take) {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            let id = match b.get_mut("id").map(Value::take) {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            let input = b.get_mut("input").map(Value::take).unwrap_or(Value::Null);
            Some(ContentBlock::ToolUse { name, id, input })
        }
        _ => None,
    }
}

fn extract_tool_result_content(b: &Value) -> String {
    // Tool result content can be a string or array of {type: "text", text: "..."}
    match b.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Extract file_path from a tool's input JSON for Read/Write/Edit/Glob/Grep tools.
pub fn extract_file_path(tool_name: &str, input: &Value) -> Option<String> {
    match tool_name {
        "Read" | "Write" | "Edit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "Glob" | "Grep" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "Bash" => None, // Could extract from command but not reliable
        _ => None,
    }
}

/// Extract a summary of tool input for storage (first 200 chars).
pub fn extract_tool_input_summary(tool_name: &str, input: &Value) -> Option<String> {
    match tool_name {
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, 200)),
        "Read" | "Write" | "Edit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "Grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| format!("pattern: {}", truncate_str(s, 180))),
        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| format!("pattern: {}", truncate_str(s, 180))),
        "Agent" => input
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, 200)),
        _ => Some(truncate_str(&input.to_string(), 200)),
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    truncate_utf8(s, max)
}

/// Take a string field from a Value, consuming it (zero-copy for owned strings).
fn take_str(v: &mut Value, field: &str) -> String {
    match v.get_mut(field).map(Value::take) {
        Some(Value::String(s)) => s,
        _ => String::new(),
    }
}

/// Take an optional string field from a Value, consuming it.
fn take_opt_str(v: &mut Value, field: &str) -> Option<String> {
    match v.get_mut(field).map(Value::take) {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_user_message_string_content() {
        let line = r#"{"type":"user","uuid":"abc","sessionId":"sess1","parentUuid":"p1","cwd":"D:\\r\\test","gitBranch":"main","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello world"}}"#;
        let record = parse_line(line).unwrap().unwrap();
        match record {
            Record::UserMessage {
                uuid,
                content,
                cwd,
                ..
            } => {
                assert_eq!(uuid, "abc");
                assert_eq!(content, "hello world");
                assert_eq!(cwd, Some("D:\\r\\test".to_string()));
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn test_parse_assistant_with_tool_use() {
        let line = r#"{"type":"assistant","uuid":"def","sessionId":"sess1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant","model":"claude-opus-4-6","content":[{"type":"text","text":"Let me read that."},{"type":"tool_use","name":"Read","id":"tool1","input":{"file_path":"/foo/bar.rs"}}],"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":10,"cache_creation_input_tokens":5}}}"#;
        let record = parse_line(line).unwrap().unwrap();
        match record {
            Record::AssistantMessage {
                model,
                content_blocks,
                usage,
                ..
            } => {
                assert_eq!(model, Some("claude-opus-4-6".to_string()));
                assert_eq!(content_blocks.len(), 2);
                let usage = usage.unwrap();
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.output_tokens, 50);
            }
            _ => panic!("expected AssistantMessage"),
        }
    }

    #[test]
    fn test_parse_tool_result() {
        let line = r#"{"type":"user","uuid":"ghi","sessionId":"sess1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool1","content":[{"type":"text","text":"file contents here"}]}]}}"#;
        let record = parse_line(line).unwrap().unwrap();
        match record {
            Record::ToolResult { results, .. } => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].tool_use_id, "tool1");
                assert_eq!(results[0].content, "file contents here");
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn test_parse_skip_types() {
        for t in &[
            "system",
            "permission-mode",
            "file-history-snapshot",
            "queue-operation",
        ] {
            let line = format!(r#"{{"type":"{}","data":"irrelevant"}}"#, t);
            let record = parse_line(&line).unwrap().unwrap();
            assert!(matches!(record, Record::Skip));
        }
    }

    #[test]
    fn test_extract_file_path() {
        let input: Value =
            serde_json::from_str(r#"{"file_path":"/foo/bar.rs"}"#).unwrap();
        assert_eq!(
            extract_file_path("Read", &input),
            Some("/foo/bar.rs".to_string())
        );
        assert_eq!(extract_file_path("Bash", &input), None);
    }

    #[test]
    fn test_thinking_block_truncation() {
        let long_thinking = "a".repeat(1000);
        let line = format!(
            r#"{{"type":"assistant","uuid":"x","sessionId":"s","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"{}"}}]}}}}"#,
            long_thinking
        );
        let record = parse_line(&line).unwrap().unwrap();
        match record {
            Record::AssistantMessage {
                content_blocks, ..
            } => match &content_blocks[0] {
                ContentBlock::Thinking(t) => {
                    assert!(t.len() <= 504); // 500 + "..."
                    assert!(t.ends_with("..."));
                }
                _ => panic!("expected Thinking block"),
            },
            _ => panic!("expected AssistantMessage"),
        }
    }

    #[test]
    fn test_empty_and_malformed_lines() {
        assert!(parse_line("").unwrap().is_none());
        assert!(parse_line("   ").unwrap().is_none());
        assert!(parse_line("not json").is_err());
    }
}
