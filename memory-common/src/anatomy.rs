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

    let ext = file_path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase();

    let raw = match ext.as_str() {
        "rs" => extract_rust(content),
        "py" => extract_python(content),
        "ts" | "tsx" | "js" | "jsx" => extract_js_ts(content),
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
                .splitn(2, |c: char| c == '(' || c == '{' || c == '<' || c == ':')
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
                .splitn(2, |c: char| c == '(' || c == ':')
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
                .splitn(2, |c: char| c == '(' || c == '{' || c == '<' || c == '=' || c == ':')
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
        if trimmed.starts_with('"') {
            if let Some(end) = trimmed[1..].find('"') {
                let key = &trimmed[1..1 + end];
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
        let content = "# Architecture\n\nThis document describes the system architecture.\n\n## Components\n";
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
        let content = "# Workspace configuration\n[workspace]\nmembers = [\"a\", \"b\"]\n\n[dependencies]\n";
        let desc = extract_description(content, "Cargo.toml");
        assert!(desc.contains("Workspace configuration"));
        assert!(desc.contains("[workspace]"));
        assert!(desc.contains("[dependencies]"));
    }
}
