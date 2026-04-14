//! File anatomy extraction for the pre-read hook.
//!
//! Extracts meaningful descriptions from file content based on language:
//! doc comments, public signatures, exports, and key declarations.
//! Stored in the `file_anatomy` table so the pre-read hook can show
//! Claude a useful summary instead of a generic filename.

use crate::db::truncate_utf8;

/// Extract a meaningful description from file content based on file type.
/// Returns a summary of doc comments, exports, and key declarations.
/// Used by the post-read hook to populate `file_anatomy.description` so
/// the pre-read hook can show useful context — helping Claude decide
/// whether a file needs to be re-read or if the summary suffices.
pub fn extract_description(content: &str, file_path: &str) -> String {
    if content.trim().is_empty() {
        return "Empty file".to_string();
    }

    let ext = file_path.rsplit('.').next().unwrap_or("").to_lowercase();

    let raw = match ext.as_str() {
        "rs" => extract_rust(content),
        "py" => extract_python(content),
        "ts" | "tsx" | "js" | "jsx" => extract_js_ts(content),
        "java" => extract_java(content),
        "go" => extract_go(content),
        "md" | "markdown" => extract_markdown(content),
        "toml" => extract_toml(content),
        "json" => extract_json(content),
        "yaml" | "yml" => extract_yaml(content),
        _ => extract_fallback(content),
    };

    if raw.is_empty() {
        return extract_fallback(content);
    }

    truncate_utf8(&raw, 500)
}

fn extract_rust(content: &str) -> String {
    let mut doc_lines: Vec<&str> = Vec::new();
    let mut signatures: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Module-level doc comments (//! ...)
        if trimmed.starts_with("//!") {
            let text = trimmed.trim_start_matches("//!").trim();
            if !text.is_empty() {
                doc_lines.push(text);
            }
            continue;
        }

        // Item doc comments (/// ...) — take the first line only for context
        if trimmed.starts_with("///") {
            continue; // skip, we care about the signatures below
        }

        // Public signatures
        if trimmed.starts_with("pub fn ")
            || trimmed.starts_with("pub struct ")
            || trimmed.starts_with("pub enum ")
            || trimmed.starts_with("pub trait ")
            || trimmed.starts_with("pub type ")
            || trimmed.starts_with("pub const ")
            || trimmed.starts_with("pub mod ")
        {
            // Extract just the name (up to first '(' or '{' or '<' or ':')
            let sig = trimmed
                .split(['(', '{', '<', ':'])
                .next()
                .unwrap_or(trimmed)
                .trim();
            signatures.push(sig.to_string());
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if !doc_lines.is_empty() {
        parts.push(doc_lines.join(" "));
    }
    if !signatures.is_empty() {
        parts.push(format!("Exports: {}", signatures.join(", ")));
    }
    parts.join(". ")
}

fn extract_python(content: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Module docstring: first triple-quoted string
    let trimmed = content.trim_start();
    if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
        let quote = &trimmed[..3];
        if let Some(end) = trimmed[3..].find(quote) {
            let docstring = trimmed[3..3 + end].trim().to_string();
            if !docstring.is_empty() {
                parts.push(docstring);
            }
        }
    }

    // Collect def/class names
    let mut names: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("def ") || trimmed.starts_with("class ") {
            let name = trimmed
                .split(['(', ':'])
                .next()
                .unwrap_or(trimmed)
                .trim();
            names.push(name.to_string());
        }
    }
    if !names.is_empty() {
        parts.push(format!("Defines: {}", names.join(", ")));
    }

    parts.join(". ")
}

fn extract_js_ts(content: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // First JSDoc comment /** ... */
    if let Some(start) = content.find("/**") {
        if let Some(end) = content[start..].find("*/") {
            let comment = &content[start + 3..start + end];
            let text: String = comment
                .lines()
                .map(|l| l.trim().trim_start_matches('*').trim())
                .filter(|l| !l.is_empty() && !l.starts_with('@'))
                .collect::<Vec<_>>()
                .join(" ");
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }

    // Export declarations
    let mut exports: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("export function ")
            || trimmed.starts_with("export class ")
            || trimmed.starts_with("export interface ")
            || trimmed.starts_with("export type ")
            || trimmed.starts_with("export const ")
            || trimmed.starts_with("export default ")
        {
            let name = trimmed
                .split(['(', '{', '<', '=', ':'])
                .next()
                .unwrap_or(trimmed)
                .trim();
            exports.push(name.to_string());
        }
    }
    if !exports.is_empty() {
        parts.push(format!("Exports: {}", exports.join(", ")));
    }

    parts.join(". ")
}

fn extract_java(content: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Javadoc on the class: first /** ... */ block
    if let Some(start) = content.find("/**") {
        if let Some(end) = content[start..].find("*/") {
            let comment = &content[start + 3..start + end];
            let text: String = comment
                .lines()
                .map(|l| l.trim().trim_start_matches('*').trim())
                .filter(|l| !l.is_empty() && !l.starts_with('@'))
                .collect::<Vec<_>>()
                .join(" ");
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }

    // Package declaration
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("package ") {
            let pkg = trimmed.trim_end_matches(';').trim();
            parts.push(pkg.to_string());
            break;
        }
    }

    // Public class/interface/enum/record declarations and public method signatures
    let mut declarations: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("public class ")
            || trimmed.starts_with("public interface ")
            || trimmed.starts_with("public enum ")
            || trimmed.starts_with("public record ")
            || trimmed.starts_with("public abstract class ")
        {
            let name = trimmed
                .split(['{', '<'])
                .next()
                .unwrap_or(trimmed)
                .trim();
            declarations.push(name.to_string());
        } else if trimmed.starts_with("public ")
            && trimmed.contains('(')
            && !trimmed.contains("class ")
        {
            // Public method signature
            let sig = trimmed
                .split('{')
                .next()
                .unwrap_or(trimmed)
                .trim()
                .trim_end_matches(';')
                .trim();
            declarations.push(sig.to_string());
        }
    }
    if !declarations.is_empty() {
        parts.push(format!("Declares: {}", declarations.join(", ")));
    }

    parts.join(". ")
}

fn extract_go(content: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Package-level doc comment: // lines immediately before `package`
    let mut doc_lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("// ") {
            doc_lines.push(trimmed.trim_start_matches("//").trim());
        } else if trimmed.starts_with("package ") {
            let pkg = trimmed.trim();
            parts.push(pkg.to_string());
            break;
        } else {
            // Non-comment, non-blank line before package — reset doc lines
            if !trimmed.is_empty() {
                doc_lines.clear();
            }
        }
    }
    if !doc_lines.is_empty() {
        parts.insert(0, doc_lines.join(" "));
    }

    // Exported types and functions (capitalized names)
    let mut exports: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(after_func) = trimmed.strip_prefix("func ") {
            // func FuncName( or func (receiver) FuncName(
            let name = if after_func.starts_with('(') {
                // Method with receiver: func (r *Recv) Name(
                after_func
                    .find(')')
                    .and_then(|i| {
                        let rest = after_func[i + 1..].trim();
                        rest.split(['(', ' ']).next()
                    })
                    .unwrap_or("")
            } else {
                after_func
                    .split(['(', ' ', '['])
                    .next()
                    .unwrap_or("")
            };
            if !name.is_empty() && name.starts_with(|c: char| c.is_uppercase()) {
                exports.push(format!("func {name}"));
            }
        } else if let Some(after_type) = trimmed.strip_prefix("type ") {
            let name = after_type
                .split([' ', '['])
                .next()
                .unwrap_or("");
            if !name.is_empty() && name.starts_with(|c: char| c.is_uppercase()) {
                let kind = after_type.split_whitespace().nth(1).unwrap_or("type");
                exports.push(format!("type {name} {kind}"));
            }
        }
    }
    if !exports.is_empty() {
        parts.push(format!("Exports: {}", exports.join(", ")));
    }

    parts.join(". ")
}

fn extract_markdown(content: &str) -> String {
    let mut heading = String::new();
    let mut first_para = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if heading.is_empty() && trimmed.starts_with('#') {
            heading = trimmed.trim_start_matches('#').trim().to_string();
            continue;
        }
        if !heading.is_empty() && first_para.is_empty() && !trimmed.is_empty() {
            first_para = trimmed.to_string();
            break;
        }
    }

    if !heading.is_empty() && !first_para.is_empty() {
        format!("{heading}. {first_para}")
    } else if !heading.is_empty() {
        heading
    } else {
        extract_fallback(content)
    }
}

fn extract_toml(content: &str) -> String {
    let mut comments: Vec<&str> = Vec::new();
    let mut sections: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') && comments.len() < 3 {
            comments.push(trimmed.trim_start_matches('#').trim());
        } else if trimmed.starts_with('[') && trimmed.ends_with(']') {
            sections.push(trimmed.to_string());
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if !comments.is_empty() {
        parts.push(comments.join(" "));
    }
    if !sections.is_empty() {
        parts.push(format!("Sections: {}", sections.join(", ")));
    }
    parts.join(". ")
}

fn extract_json(content: &str) -> String {
    // Just extract top-level keys
    let mut keys: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(stripped) = trimmed.strip_prefix('"') {
            if let Some(end) = stripped.find('"') {
                let key = &stripped[..end];
                if !keys.contains(&key.to_string()) && keys.len() < 10 {
                    keys.push(key.to_string());
                }
            }
        }
    }
    if keys.is_empty() {
        return String::new();
    }
    format!("Keys: {}", keys.join(", "))
}

fn extract_yaml(content: &str) -> String {
    let mut comments: Vec<&str> = Vec::new();
    let mut top_keys: Vec<&str> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') && comments.len() < 3 {
            comments.push(trimmed.trim_start_matches('#').trim());
        } else if !line.starts_with(' ') && !line.starts_with('\t') && trimmed.contains(':') {
            if let Some(key) = trimmed.split(':').next() {
                if !key.is_empty() && top_keys.len() < 10 {
                    top_keys.push(key);
                }
            }
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if !comments.is_empty() {
        parts.push(comments.join(" "));
    }
    if !top_keys.is_empty() {
        parts.push(format!("Keys: {}", top_keys.join(", ")));
    }
    parts.join(". ")
}

fn extract_fallback(content: &str) -> String {
    content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_rust_doc_comment() {
        let content = "//! Pack file parsing and delta resolution.\n//! Handles OFS and REF deltas.\n\nuse std::io;\n\npub fn parse_pack() {}\n";
        let desc = extract_description(content, "pack.rs");
        assert!(desc.contains("Pack file parsing"));
        assert!(desc.contains("delta resolution"));
        assert!(desc.contains("pub fn parse_pack"));
    }

    #[test]
    fn test_extract_rust_signatures() {
        let content = "pub struct DeltaArena {}\npub fn resolve_ref_delta() {}\npub enum ObjectType {}\nfn private_fn() {}\n";
        let desc = extract_description(content, "lib.rs");
        assert!(desc.contains("pub struct DeltaArena"));
        assert!(desc.contains("pub fn resolve_ref_delta"));
        assert!(desc.contains("pub enum ObjectType"));
        assert!(!desc.contains("private_fn"));
    }

    #[test]
    fn test_extract_python_docstring() {
        let content = "\"\"\"Database connection management.\"\"\"\n\nimport sqlite3\n\ndef connect():\n    pass\n\nclass Pool:\n    pass\n";
        let desc = extract_description(content, "db.py");
        assert!(desc.contains("Database connection management"));
        assert!(desc.contains("def connect"));
        assert!(desc.contains("class Pool"));
    }

    #[test]
    fn test_extract_js_exports() {
        let content = "/** Server configuration module */\nexport function startServer() {}\nexport class Config {}\nexport const PORT = 3000;\n";
        let desc = extract_description(content, "server.ts");
        assert!(desc.contains("Server configuration module"));
        assert!(desc.contains("export function startServer"));
        assert!(desc.contains("export class Config"));
    }

    #[test]
    fn test_extract_markdown_heading() {
        let content =
            "# Architecture\n\nThis document describes the system architecture.\n\n## Components\n";
        let desc = extract_description(content, "ARCHITECTURE.md");
        assert!(desc.contains("Architecture"));
        assert!(desc.contains("system architecture"));
    }

    #[test]
    fn test_extract_fallback() {
        let content = "#!/bin/bash\necho hello\n";
        let desc = extract_description(content, "script.sh");
        assert_eq!(desc, "#!/bin/bash");
    }

    #[test]
    fn test_extract_empty_content() {
        assert_eq!(extract_description("", "foo.rs"), "Empty file");
        assert_eq!(extract_description("   \n  \n", "foo.rs"), "Empty file");
    }

    #[test]
    fn test_extract_toml() {
        let content =
            "# Workspace configuration\n[workspace]\nmembers = [\"a\", \"b\"]\n\n[dependencies]\n";
        let desc = extract_description(content, "Cargo.toml");
        assert!(desc.contains("Workspace configuration"));
        assert!(desc.contains("[workspace]"));
        assert!(desc.contains("[dependencies]"));
    }

    #[test]
    fn test_extract_java_class() {
        let content = "\
package com.example.service;

import java.util.List;

/** Manages user authentication and session tokens. */
public class AuthService {
    public void login(String user, String pass) {}
    public boolean validate(String token) {}
    private void internal() {}
}
";
        let desc = extract_description(content, "AuthService.java");
        assert!(desc.contains("Manages user authentication"), "got: {desc}");
        assert!(desc.contains("package com.example.service"), "got: {desc}");
        assert!(desc.contains("public class AuthService"), "got: {desc}");
        assert!(desc.contains("public void login"), "got: {desc}");
        assert!(desc.contains("public boolean validate"), "got: {desc}");
        assert!(
            !desc.contains("internal"),
            "should not contain private methods: {desc}"
        );
    }

    #[test]
    fn test_extract_java_interface() {
        let content = "\
package com.example.api;

public interface Repository<T> {
    public T findById(long id);
    public List<T> findAll();
}
";
        let desc = extract_description(content, "Repository.java");
        assert!(desc.contains("public interface Repository"), "got: {desc}");
        assert!(desc.contains("public T findById"), "got: {desc}");
    }

    #[test]
    fn test_extract_go_package() {
        let content = "\
// Package handler provides HTTP request handling for the API.
// It implements routing, middleware, and error handling.
package handler

import \"net/http\"

// Server is the main HTTP server.
type Server struct {
    router *http.ServeMux
}

// NewServer creates a configured server instance.
func NewServer(addr string) *Server {
    return &Server{}
}

// Start begins listening on the configured address.
func (s *Server) Start() error {
    return nil
}

func helperFunc() {}
";
        let desc = extract_description(content, "handler.go");
        assert!(desc.contains("HTTP request handling"), "got: {desc}");
        assert!(desc.contains("package handler"), "got: {desc}");
        assert!(desc.contains("func NewServer"), "got: {desc}");
        assert!(desc.contains("func Start"), "got: {desc}");
        assert!(desc.contains("type Server struct"), "got: {desc}");
        assert!(
            !desc.contains("helperFunc"),
            "should not contain unexported func: {desc}"
        );
    }

    #[test]
    fn test_extract_go_exported_types() {
        let content = "\
package models

type User struct {
    Name string
}

type Role interface {
    Permissions() []string
}

type internal struct{}
";
        let desc = extract_description(content, "models.go");
        assert!(desc.contains("type User struct"), "got: {desc}");
        assert!(desc.contains("type Role interface"), "got: {desc}");
        assert!(
            !desc.contains("internal"),
            "should not contain unexported type: {desc}"
        );
    }
}
