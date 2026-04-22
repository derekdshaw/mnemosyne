//! File anatomy extraction for the pre-read hook.
//!
//! Extracts meaningful descriptions from file content based on language:
//! doc comments, public signatures, exports, and key declarations. Also
//! tracks symbol line numbers so the pre-read hook can let Claude jump
//! straight to a symbol instead of re-reading the whole file.
//!
//! Stored in the `file_anatomy` table: `description` holds the one-liner
//! summary and `top_symbols_json` holds a JSON-encoded `Vec<Symbol>`.

use serde::{Deserialize, Serialize};

use crate::db::truncate_utf8;

/// A code symbol surfaced by the anatomy scan, with its line number so the
/// pre-read hook can render `name@line` hints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    /// Short kind tag: `fn`, `struct`, `enum`, `trait`, `type`, `const`,
    /// `mod`, `class`, `interface`, `def`, `method`, `record`.
    pub kind: String,
    pub name: String,
    pub line: u32,
}

/// Anatomy for a single file: a one-line summary plus (for code files) a
/// symbol-line index.
#[derive(Debug, Clone, Default)]
pub struct AnatomyData {
    pub description: String,
    pub symbols: Vec<Symbol>,
}

impl AnatomyData {
    fn just(description: String) -> Self {
        Self {
            description,
            symbols: Vec::new(),
        }
    }
}

/// Extract a meaningful description plus a symbol-line index from file content.
/// Dispatches on filename first (for manifests like `Cargo.toml` / `package.json`)
/// then falls back to extension-based extraction.
pub fn extract_anatomy(content: &str, file_path: &str) -> AnatomyData {
    if content.trim().is_empty() {
        return AnatomyData::just("Empty file".to_string());
    }

    let filename = file_path.rsplit('/').next().unwrap_or(file_path);

    // Manifest-aware dispatch. These return richer descriptions than the
    // generic TOML/JSON extractors and never carry symbols.
    let manifest = match filename {
        "Cargo.toml" => Some(extract_cargo_toml(content)),
        "package.json" => Some(extract_package_json(content)),
        "pyproject.toml" => Some(extract_pyproject_toml(content)),
        "go.mod" => Some(extract_go_mod(content)),
        "pom.xml" => Some(extract_pom_xml(content)),
        "build.gradle" | "build.gradle.kts" => Some(extract_gradle(content)),
        "requirements.txt" => Some(extract_requirements_txt(content)),
        "Gemfile" => Some(extract_gemfile(content)),
        "composer.json" => Some(extract_composer_json(content)),
        _ => None,
    };
    if let Some(desc) = manifest {
        return AnatomyData::just(truncate_utf8(&desc, 500));
    }

    let ext = file_path.rsplit('.').next().unwrap_or("").to_lowercase();
    let data = match ext.as_str() {
        "rs" => extract_rust(content),
        "py" => extract_python(content),
        "ts" | "tsx" | "js" | "jsx" => extract_js_ts(content),
        "java" => extract_java(content),
        "go" => extract_go(content),
        "md" | "markdown" => AnatomyData::just(extract_markdown(content)),
        "toml" => AnatomyData::just(extract_toml(content)),
        "json" => AnatomyData::just(extract_json(content)),
        "yaml" | "yml" => AnatomyData::just(extract_yaml(content)),
        _ => AnatomyData::just(extract_fallback(content)),
    };

    let description = if data.description.is_empty() {
        extract_fallback(content)
    } else {
        data.description
    };

    AnatomyData {
        description: truncate_utf8(&description, 500),
        symbols: data.symbols,
    }
}

/// Backwards-compatible wrapper for callers that only need the description.
pub fn extract_description(content: &str, file_path: &str) -> String {
    extract_anatomy(content, file_path).description
}

// ---------------------------------------------------------------------------
// Per-language code extractors (emit symbols + description)
// ---------------------------------------------------------------------------

fn extract_rust(content: &str) -> AnatomyData {
    let mut doc_lines: Vec<&str> = Vec::new();
    let mut signatures: Vec<String> = Vec::new();
    let mut symbols: Vec<Symbol> = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let line_no = (idx + 1) as u32;

        if trimmed.starts_with("//!") {
            let text = trimmed.trim_start_matches("//!").trim();
            if !text.is_empty() {
                doc_lines.push(text);
            }
            continue;
        }
        if trimmed.starts_with("///") {
            continue;
        }

        let (kind, after) = if let Some(rest) = trimmed.strip_prefix("pub fn ") {
            ("fn", rest)
        } else if let Some(rest) = trimmed.strip_prefix("pub async fn ") {
            ("fn", rest)
        } else if let Some(rest) = trimmed.strip_prefix("pub struct ") {
            ("struct", rest)
        } else if let Some(rest) = trimmed.strip_prefix("pub enum ") {
            ("enum", rest)
        } else if let Some(rest) = trimmed.strip_prefix("pub trait ") {
            ("trait", rest)
        } else if let Some(rest) = trimmed.strip_prefix("pub type ") {
            ("type", rest)
        } else if let Some(rest) = trimmed.strip_prefix("pub const ") {
            ("const", rest)
        } else if let Some(rest) = trimmed.strip_prefix("pub mod ") {
            ("mod", rest)
        } else {
            continue;
        };

        let name = after
            .split(['(', '{', '<', ':', ' ', ';'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }

        // Preserve the existing description format exactly.
        let sig_prefix = match kind {
            "fn" => "pub fn",
            "struct" => "pub struct",
            "enum" => "pub enum",
            "trait" => "pub trait",
            "type" => "pub type",
            "const" => "pub const",
            "mod" => "pub mod",
            _ => "pub",
        };
        signatures.push(format!("{sig_prefix} {name}"));
        symbols.push(Symbol {
            kind: kind.to_string(),
            name,
            line: line_no,
        });
    }

    let mut parts: Vec<String> = Vec::new();
    if !doc_lines.is_empty() {
        parts.push(doc_lines.join(" "));
    }
    if !signatures.is_empty() {
        parts.push(format!("Exports: {}", signatures.join(", ")));
    }

    AnatomyData {
        description: parts.join(". "),
        symbols,
    }
}

fn extract_python(content: &str) -> AnatomyData {
    let mut parts: Vec<String> = Vec::new();
    let mut symbols: Vec<Symbol> = Vec::new();

    let trimmed_start = content.trim_start();
    if trimmed_start.starts_with("\"\"\"") || trimmed_start.starts_with("'''") {
        let quote = &trimmed_start[..3];
        if let Some(end) = trimmed_start[3..].find(quote) {
            let docstring = trimmed_start[3..3 + end].trim().to_string();
            if !docstring.is_empty() {
                parts.push(docstring);
            }
        }
    }

    let mut names: Vec<String> = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let line_no = (idx + 1) as u32;

        let (kind, after) = if let Some(rest) = trimmed.strip_prefix("def ") {
            ("def", rest)
        } else if let Some(rest) = trimmed.strip_prefix("async def ") {
            ("def", rest)
        } else if let Some(rest) = trimmed.strip_prefix("class ") {
            ("class", rest)
        } else {
            continue;
        };

        let name = after
            .split(['(', ':', ' '])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }

        // Skip dunder/private for cleaner output but still include in symbols
        let is_top_level = !line.starts_with(' ') && !line.starts_with('\t');
        if is_top_level {
            names.push(format!("{kind} {name}"));
        }
        symbols.push(Symbol {
            kind: kind.to_string(),
            name,
            line: line_no,
        });
    }
    if !names.is_empty() {
        parts.push(format!("Defines: {}", names.join(", ")));
    }

    AnatomyData {
        description: parts.join(". "),
        symbols,
    }
}

fn extract_js_ts(content: &str) -> AnatomyData {
    let mut parts: Vec<String> = Vec::new();
    let mut symbols: Vec<Symbol> = Vec::new();

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

    let mut exports: Vec<String> = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let line_no = (idx + 1) as u32;

        let (kind, sig_prefix, after) = if let Some(r) = trimmed.strip_prefix("export function ") {
            ("fn", "export function", r)
        } else if let Some(r) = trimmed.strip_prefix("export async function ") {
            ("fn", "export async function", r)
        } else if let Some(r) = trimmed.strip_prefix("export class ") {
            ("class", "export class", r)
        } else if let Some(r) = trimmed.strip_prefix("export interface ") {
            ("interface", "export interface", r)
        } else if let Some(r) = trimmed.strip_prefix("export type ") {
            ("type", "export type", r)
        } else if let Some(r) = trimmed.strip_prefix("export const ") {
            ("const", "export const", r)
        } else if let Some(r) = trimmed.strip_prefix("export default function ") {
            ("fn", "export default function", r)
        } else if let Some(r) = trimmed.strip_prefix("export default class ") {
            ("class", "export default class", r)
        } else {
            continue;
        };

        let name = after
            .split(['(', '{', '<', '=', ':', ' ', ';'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }

        exports.push(format!("{sig_prefix} {name}"));
        symbols.push(Symbol {
            kind: kind.to_string(),
            name,
            line: line_no,
        });
    }
    if !exports.is_empty() {
        parts.push(format!("Exports: {}", exports.join(", ")));
    }

    AnatomyData {
        description: parts.join(". "),
        symbols,
    }
}

fn extract_java(content: &str) -> AnatomyData {
    let mut parts: Vec<String> = Vec::new();
    let mut symbols: Vec<Symbol> = Vec::new();

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

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("package ") {
            let pkg = trimmed.trim_end_matches(';').trim();
            parts.push(pkg.to_string());
            break;
        }
    }

    let mut declarations: Vec<String> = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let line_no = (idx + 1) as u32;

        let (kind, sig_prefix, after) = if let Some(r) = trimmed.strip_prefix("public class ") {
            ("class", "public class", r)
        } else if let Some(r) = trimmed.strip_prefix("public interface ") {
            ("interface", "public interface", r)
        } else if let Some(r) = trimmed.strip_prefix("public enum ") {
            ("enum", "public enum", r)
        } else if let Some(r) = trimmed.strip_prefix("public record ") {
            ("record", "public record", r)
        } else if let Some(r) = trimmed.strip_prefix("public abstract class ") {
            ("class", "public abstract class", r)
        } else if trimmed.starts_with("public ")
            && trimmed.contains('(')
            && !trimmed.contains("class ")
        {
            // Public method signature, e.g. "public void login(String u, String p) {"
            let sig = trimmed
                .split('{')
                .next()
                .unwrap_or(trimmed)
                .trim()
                .trim_end_matches(';')
                .trim();
            declarations.push(sig.to_string());
            if let Some(paren) = sig.find('(') {
                let before = &sig[..paren];
                if let Some(name) = before.split_whitespace().last() {
                    symbols.push(Symbol {
                        kind: "method".to_string(),
                        name: name.to_string(),
                        line: line_no,
                    });
                }
            }
            continue;
        } else {
            continue;
        };

        let name = after
            .split(['{', '<', '(', ' '])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        declarations.push(format!("{sig_prefix} {name}"));
        symbols.push(Symbol {
            kind: kind.to_string(),
            name,
            line: line_no,
        });
    }
    if !declarations.is_empty() {
        parts.push(format!("Declares: {}", declarations.join(", ")));
    }

    AnatomyData {
        description: parts.join(". "),
        symbols,
    }
}

fn extract_go(content: &str) -> AnatomyData {
    let mut parts: Vec<String> = Vec::new();
    let mut symbols: Vec<Symbol> = Vec::new();

    // Package-level doc: leading // lines before `package`
    let mut doc_lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("// ") {
            doc_lines.push(rest);
        } else if trimmed.starts_with("package ") {
            let pkg = trimmed.trim();
            parts.push(pkg.to_string());
            break;
        } else if !trimmed.is_empty() {
            doc_lines.clear();
        }
    }
    if !doc_lines.is_empty() {
        parts.insert(0, doc_lines.join(" "));
    }

    let mut exports: Vec<String> = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let line_no = (idx + 1) as u32;

        if let Some(after_func) = trimmed.strip_prefix("func ") {
            let name = if after_func.starts_with('(') {
                after_func
                    .find(')')
                    .and_then(|i| {
                        let rest = after_func[i + 1..].trim();
                        rest.split(['(', ' ']).next()
                    })
                    .unwrap_or("")
            } else {
                after_func.split(['(', ' ', '[']).next().unwrap_or("")
            };
            if !name.is_empty() && name.starts_with(|c: char| c.is_uppercase()) {
                exports.push(format!("func {name}"));
                symbols.push(Symbol {
                    kind: "fn".to_string(),
                    name: name.to_string(),
                    line: line_no,
                });
            }
        } else if let Some(after_type) = trimmed.strip_prefix("type ") {
            let name = after_type.split([' ', '[']).next().unwrap_or("");
            if !name.is_empty() && name.starts_with(|c: char| c.is_uppercase()) {
                let go_kind = after_type.split_whitespace().nth(1).unwrap_or("type");
                exports.push(format!("type {name} {go_kind}"));
                symbols.push(Symbol {
                    kind: "type".to_string(),
                    name: name.to_string(),
                    line: line_no,
                });
            }
        }
    }
    if !exports.is_empty() {
        parts.push(format!("Exports: {}", exports.join(", ")));
    }

    AnatomyData {
        description: parts.join(". "),
        symbols,
    }
}

// ---------------------------------------------------------------------------
// Prose / generic text extractors (no symbols)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Manifest-aware extractors — richer output for dependency config files
// ---------------------------------------------------------------------------

/// Format a sorted list of (name, version) pairs into a description fragment
/// like `"Deps: tokio 1.42, serde 1 (+12 more)"`. `limit` caps the shown entries.
fn fmt_deps(label: &str, entries: &[(String, String)], limit: usize) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = entries
        .iter()
        .take(limit)
        .map(|(n, v)| {
            if v.is_empty() {
                n.clone()
            } else {
                format!("{n} {v}")
            }
        })
        .collect();
    let extra = entries.len().saturating_sub(limit);
    let suffix = if extra > 0 {
        format!(" (+{extra} more)")
    } else {
        String::new()
    };
    format!("{label}: {}{suffix}", shown.join(", "))
}

/// Strip common semver operator prefixes so `"^1.2.3"` becomes `"1.2.3"`.
fn clean_version(v: &str) -> String {
    v.trim_start_matches(['^', '~', '>', '=', '<', ' '])
        .to_string()
}

fn extract_cargo_toml(content: &str) -> String {
    let parsed: toml::Value = match toml::from_str(content) {
        Ok(v) => v,
        Err(_) => return extract_toml(content),
    };

    let mut parts: Vec<String> = Vec::new();

    // Prefer [package] name/version, fall back to [workspace.package], then
    // label as a workspace root if neither has a name.
    let package = parsed.get("package").and_then(|v| v.as_table());
    let workspace = parsed.get("workspace").and_then(|v| v.as_table());
    let ws_package = workspace
        .and_then(|t| t.get("package"))
        .and_then(|v| v.as_table());

    let name = package
        .and_then(|t| t.get("name"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            ws_package
                .and_then(|t| t.get("name"))
                .and_then(|v| v.as_str())
        });
    let version = package
        .and_then(|t| t.get("version"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            ws_package
                .and_then(|t| t.get("version"))
                .and_then(|v| v.as_str())
        });

    match (name, version) {
        (Some(n), Some(v)) if workspace.is_some() && package.is_none() => {
            parts.push(format!("{n} v{v} (workspace)"));
        }
        (Some(n), Some(v)) => {
            parts.push(format!("{n} v{v}"));
        }
        (Some(n), None) => {
            parts.push(n.to_string());
        }
        (None, _) if workspace.is_some() => {
            parts.push("Cargo workspace".to_string());
        }
        _ => {}
    }

    if let Some(members) = workspace
        .and_then(|t| t.get("members"))
        .and_then(|v| v.as_array())
    {
        parts.push(format!("{} members", members.len()));
    }

    // Dependencies: prefer [dependencies] then [workspace.dependencies]
    let dep_table = parsed
        .get("dependencies")
        .and_then(|v| v.as_table())
        .or_else(|| {
            parsed
                .get("workspace")
                .and_then(|v| v.get("dependencies"))
                .and_then(|v| v.as_table())
        });

    if let Some(deps) = dep_table {
        let mut entries: Vec<(String, String)> = deps
            .iter()
            .map(|(k, v)| {
                let ver = match v {
                    toml::Value::String(s) => clean_version(s),
                    toml::Value::Table(t) => t
                        .get("version")
                        .and_then(|v| v.as_str())
                        .map(clean_version)
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                (k.clone(), ver)
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let line = fmt_deps("Deps", &entries, 5);
        if !line.is_empty() {
            parts.push(line);
        }
    }

    if parts.is_empty() {
        extract_toml(content)
    } else {
        parts.join(". ")
    }
}

fn extract_package_json(content: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return extract_json(content),
    };

    let mut parts: Vec<String> = Vec::new();

    let name = parsed.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let version = parsed.get("version").and_then(|v| v.as_str()).unwrap_or("");
    if !name.is_empty() {
        if !version.is_empty() {
            parts.push(format!("{name}@{version}"));
        } else {
            parts.push(name.to_string());
        }
    }

    let collect_deps = |key: &str| -> Vec<(String, String)> {
        let Some(obj) = parsed.get(key).and_then(|v| v.as_object()) else {
            return Vec::new();
        };
        let mut entries: Vec<(String, String)> = obj
            .iter()
            .map(|(k, v)| {
                let raw = v.as_str().unwrap_or("");
                let cleaned = clean_version(raw);
                // For npm, showing the major version is usually enough.
                let major = cleaned.split('.').next().unwrap_or(&cleaned).to_string();
                (k.clone(), major)
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    };

    let deps = collect_deps("dependencies");
    if !deps.is_empty() {
        parts.push(fmt_deps("Deps", &deps, 5));
    }
    let dev_deps = collect_deps("devDependencies");
    if !dev_deps.is_empty() {
        parts.push(fmt_deps("DevDeps", &dev_deps, 3));
    }

    if parts.is_empty() {
        extract_json(content)
    } else {
        parts.join(". ")
    }
}

fn extract_pyproject_toml(content: &str) -> String {
    let parsed: toml::Value = match toml::from_str(content) {
        Ok(v) => v,
        Err(_) => return extract_toml(content),
    };

    let mut parts: Vec<String> = Vec::new();

    // PEP 621: [project]
    let project = parsed.get("project").and_then(|v| v.as_table());
    let poetry = parsed
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|v| v.as_table());

    let (name, version) = if let Some(p) = project {
        let n = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let v = p.get("version").and_then(|v| v.as_str()).unwrap_or("");
        (n.to_string(), v.to_string())
    } else if let Some(p) = poetry {
        let n = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let v = p.get("version").and_then(|v| v.as_str()).unwrap_or("");
        (n.to_string(), v.to_string())
    } else {
        (String::new(), String::new())
    };

    if !name.is_empty() {
        if !version.is_empty() {
            parts.push(format!("{name} {version}"));
        } else {
            parts.push(name);
        }
    }

    // PEP 621 deps are a list of strings like "fastapi>=0.104".
    let mut dep_entries: Vec<(String, String)> = Vec::new();
    if let Some(deps) = project
        .and_then(|p| p.get("dependencies"))
        .and_then(|v| v.as_array())
    {
        for d in deps {
            if let Some(s) = d.as_str() {
                let (n, v) = split_pep508(s);
                dep_entries.push((n, v));
            }
        }
    } else if let Some(deps) = poetry
        .and_then(|p| p.get("dependencies"))
        .and_then(|v| v.as_table())
    {
        for (k, v) in deps {
            let ver = match v {
                toml::Value::String(s) => clean_version(s),
                toml::Value::Table(t) => t
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(clean_version)
                    .unwrap_or_default(),
                _ => String::new(),
            };
            if k != "python" {
                dep_entries.push((k.clone(), ver));
            }
        }
    }
    dep_entries.sort_by(|a, b| a.0.cmp(&b.0));
    if !dep_entries.is_empty() {
        parts.push(fmt_deps("Deps", &dep_entries, 5));
    }

    if parts.is_empty() {
        extract_toml(content)
    } else {
        parts.join(". ")
    }
}

/// Split a PEP 508 requirement like `"fastapi>=0.104"` into name + version spec.
fn split_pep508(s: &str) -> (String, String) {
    let bytes = s.as_bytes();
    let mut split_at = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if matches!(b, b'=' | b'<' | b'>' | b'~' | b'!' | b'[' | b';' | b' ') {
            split_at = i;
            break;
        }
    }
    let name = s[..split_at].trim().to_string();
    let version = s[split_at..].trim().to_string();
    (name, version)
}

fn extract_go_mod(content: &str) -> String {
    let mut module = String::new();
    let mut go_version = String::new();
    let mut deps: Vec<(String, String)> = Vec::new();
    let mut in_require_block = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("module ") {
            module = rest.trim().to_string();
        } else if let Some(rest) = trimmed.strip_prefix("go ") {
            go_version = rest.trim().to_string();
        } else if trimmed.starts_with("require (") {
            in_require_block = true;
        } else if trimmed == ")" && in_require_block {
            in_require_block = false;
        } else if in_require_block || trimmed.starts_with("require ") {
            let body = trimmed.trim_start_matches("require").trim();
            let tokens: Vec<&str> = body.split_whitespace().collect();
            if tokens.len() >= 2 && !tokens[0].is_empty() && !tokens[0].starts_with("//") {
                let short = tokens[0]
                    .rsplit('/')
                    .next()
                    .unwrap_or(tokens[0])
                    .to_string();
                deps.push((short, tokens[1].to_string()));
            }
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if !module.is_empty() {
        if !go_version.is_empty() {
            parts.push(format!("{module} (go {go_version})"));
        } else {
            parts.push(module);
        }
    }
    if !deps.is_empty() {
        parts.push(fmt_deps("Deps", &deps, 5));
    }

    if parts.is_empty() {
        extract_fallback(content)
    } else {
        parts.join(". ")
    }
}

fn extract_pom_xml(content: &str) -> String {
    let art_re = regex::Regex::new(r"<artifactId>\s*([^<]+?)\s*</artifactId>").unwrap();
    let ver_re = regex::Regex::new(r"<version>\s*([^<]+?)\s*</version>").unwrap();
    let dep_block_re = regex::Regex::new(r"(?s)<dependency>(.*?)</dependency>").unwrap();

    let deps_idx = content.find("<dependencies>").unwrap_or(content.len());
    let project_section = &content[..deps_idx];

    let project_id = art_re
        .captures(project_section)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default();
    let project_ver = ver_re
        .captures(project_section)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default();

    let deps: Vec<(String, String)> = dep_block_re
        .captures_iter(content)
        .filter_map(|c| {
            let block = c.get(1)?.as_str();
            let name = art_re
                .captures(block)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().trim().to_string())?;
            let ver = ver_re
                .captures(block)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            Some((name, ver))
        })
        .collect();

    let mut parts: Vec<String> = Vec::new();
    if !project_id.is_empty() {
        if !project_ver.is_empty() {
            parts.push(format!("{project_id} {project_ver}"));
        } else {
            parts.push(project_id);
        }
    }
    if !deps.is_empty() {
        parts.push(fmt_deps("Deps", &deps, 5));
    }

    if parts.is_empty() {
        extract_fallback(content)
    } else {
        parts.join(". ")
    }
}

fn extract_gradle(content: &str) -> String {
    // Match: implementation 'group:name:version' or implementation("group:name:version")
    let re = regex::Regex::new(
        r#"(?:implementation|api|compile|testImplementation|runtimeOnly)\s*\(?\s*["']([^"':]+):([^"':]+):([^"']+)["']"#,
    )
    .unwrap();

    let deps: Vec<(String, String)> = re
        .captures_iter(content)
        .map(|c| {
            let name = c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
            let ver = c.get(3).map(|m| m.as_str().to_string()).unwrap_or_default();
            (name, ver)
        })
        .collect();

    if deps.is_empty() {
        return extract_fallback(content);
    }
    fmt_deps("Deps", &deps, 5)
}

fn extract_requirements_txt(content: &str) -> String {
    let deps: Vec<(String, String)> = content
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') || l.starts_with('-') {
                return None;
            }
            let (n, v) = split_pep508(l);
            if n.is_empty() {
                None
            } else {
                Some((n, v))
            }
        })
        .collect();

    if deps.is_empty() {
        return extract_fallback(content);
    }
    fmt_deps("Deps", &deps, 8)
}

fn extract_gemfile(content: &str) -> String {
    let re = regex::Regex::new(r#"gem\s+['"]([^'"]+)['"](?:\s*,\s*['"]([^'"]+)['"])?"#).unwrap();
    let deps: Vec<(String, String)> = re
        .captures_iter(content)
        .map(|c| {
            let name = c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let ver = c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
            (name, ver)
        })
        .collect();

    if deps.is_empty() {
        return extract_fallback(content);
    }
    fmt_deps("Deps", &deps, 5)
}

fn extract_composer_json(content: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return extract_json(content),
    };

    let mut parts: Vec<String> = Vec::new();

    let name = parsed.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let version = parsed.get("version").and_then(|v| v.as_str()).unwrap_or("");
    if !name.is_empty() {
        if !version.is_empty() {
            parts.push(format!("{name} {version}"));
        } else {
            parts.push(name.to_string());
        }
    }

    let collect_deps = |key: &str| -> Vec<(String, String)> {
        let Some(obj) = parsed.get(key).and_then(|v| v.as_object()) else {
            return Vec::new();
        };
        let mut entries: Vec<(String, String)> = obj
            .iter()
            .filter(|(k, _)| *k != "php" && !k.starts_with("ext-"))
            .map(|(k, v)| {
                let raw = v.as_str().unwrap_or("");
                (k.clone(), clean_version(raw))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    };

    let deps = collect_deps("require");
    if !deps.is_empty() {
        parts.push(fmt_deps("Deps", &deps, 5));
    }
    let dev_deps = collect_deps("require-dev");
    if !dev_deps.is_empty() {
        parts.push(fmt_deps("DevDeps", &dev_deps, 3));
    }

    if parts.is_empty() {
        extract_json(content)
    } else {
        parts.join(". ")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_rust_doc_comment() {
        let content = "//! Pack file parsing and delta resolution.\n//! Handles OFS and REF deltas.\n\nuse std::io;\n\npub fn parse_pack() {}\n";
        let data = extract_anatomy(content, "pack.rs");
        assert!(data.description.contains("Pack file parsing"));
        assert!(data.description.contains("delta resolution"));
        assert!(data.description.contains("pub fn parse_pack"));
        assert_eq!(data.symbols.len(), 1);
        assert_eq!(data.symbols[0].name, "parse_pack");
        assert_eq!(data.symbols[0].kind, "fn");
        assert_eq!(data.symbols[0].line, 6);
    }

    #[test]
    fn test_extract_rust_signatures() {
        let content = "pub struct DeltaArena {}\npub fn resolve_ref_delta() {}\npub enum ObjectType {}\nfn private_fn() {}\n";
        let data = extract_anatomy(content, "lib.rs");
        assert!(data.description.contains("pub struct DeltaArena"));
        assert!(data.description.contains("pub fn resolve_ref_delta"));
        assert!(data.description.contains("pub enum ObjectType"));
        assert!(!data.description.contains("private_fn"));
        let names: Vec<&str> = data.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["DeltaArena", "resolve_ref_delta", "ObjectType"]);
        assert_eq!(data.symbols[0].line, 1);
        assert_eq!(data.symbols[1].line, 2);
        assert_eq!(data.symbols[2].line, 3);
    }

    #[test]
    fn test_extract_python_docstring() {
        let content = "\"\"\"Database connection management.\"\"\"\n\nimport sqlite3\n\ndef connect():\n    pass\n\nclass Pool:\n    pass\n";
        let data = extract_anatomy(content, "db.py");
        assert!(data.description.contains("Database connection management"));
        assert!(data.description.contains("def connect"));
        assert!(data.description.contains("class Pool"));
        let names: Vec<&str> = data.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["connect", "Pool"]);
        assert_eq!(data.symbols[0].line, 5);
        assert_eq!(data.symbols[1].line, 8);
    }

    #[test]
    fn test_extract_js_exports() {
        let content = "/** Server configuration module */\nexport function startServer() {}\nexport class Config {}\nexport const PORT = 3000;\n";
        let data = extract_anatomy(content, "server.ts");
        assert!(data.description.contains("Server configuration module"));
        assert!(data.description.contains("export function startServer"));
        assert!(data.description.contains("export class Config"));
        let names: Vec<&str> = data.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["startServer", "Config", "PORT"]);
        assert_eq!(data.symbols[0].line, 2);
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
    fn test_extract_generic_toml() {
        // Use a filename that isn't Cargo.toml/pyproject.toml so the generic
        // TOML extractor runs instead of the manifest-aware one.
        let content = "# App configuration\n[server]\nport = 8080\n\n[database]\nurl = \"\"\n";
        let desc = extract_description(content, "config.toml");
        assert!(desc.contains("App configuration"));
        assert!(desc.contains("[server]"));
        assert!(desc.contains("[database]"));
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
        let data = extract_anatomy(content, "AuthService.java");
        assert!(
            data.description.contains("Manages user authentication"),
            "got: {}",
            data.description
        );
        assert!(data.description.contains("package com.example.service"));
        assert!(data.description.contains("public class AuthService"));
        assert!(data.description.contains("public void login"));
        assert!(data.description.contains("public boolean validate"));
        assert!(
            !data.description.contains("internal"),
            "should not contain private methods: {}",
            data.description
        );
        let names: Vec<&str> = data.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"AuthService"));
        assert!(names.contains(&"login"));
        assert!(names.contains(&"validate"));
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
        let data = extract_anatomy(content, "Repository.java");
        assert!(data.description.contains("public interface Repository"));
        assert!(data.description.contains("public T findById"));
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
        let data = extract_anatomy(content, "handler.go");
        assert!(data.description.contains("HTTP request handling"));
        assert!(data.description.contains("package handler"));
        assert!(data.description.contains("func NewServer"));
        assert!(data.description.contains("func Start"));
        assert!(data.description.contains("type Server struct"));
        assert!(!data.description.contains("helperFunc"));
        let names: Vec<&str> = data.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Server"));
        assert!(names.contains(&"NewServer"));
        assert!(names.contains(&"Start"));
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
        let data = extract_anatomy(content, "models.go");
        assert!(data.description.contains("type User struct"));
        assert!(data.description.contains("type Role interface"));
        assert!(!data.description.contains("internal"));
    }

    // ---- Manifest extractor tests ----

    #[test]
    fn test_extract_cargo_toml_package() {
        let content = r#"
[package]
name = "mypkg"
version = "0.4.2"
edition = "2021"

[dependencies]
tokio = "1.42"
serde = { version = "1", features = ["derive"] }
rusqlite = "0.31"
anyhow = "1"
clap = "4"
regex = "1"
"#;
        let desc = extract_description(content, "Cargo.toml");
        assert!(desc.contains("mypkg v0.4.2"), "got: {desc}");
        assert!(desc.contains("Deps:"), "got: {desc}");
        // 6 deps total; after alphabetical sort the top 5 are anyhow, clap,
        // regex, rusqlite, serde. tokio becomes the "(+1 more)" tail.
        assert!(desc.contains("rusqlite 0.31"), "got: {desc}");
        assert!(desc.contains("(+1 more)"), "got: {desc}");
    }

    #[test]
    fn test_extract_cargo_toml_workspace() {
        let content = r#"
[workspace]
members = ["a", "b", "c"]

[workspace.dependencies]
tokio = "1"
serde = "1"
"#;
        let desc = extract_description(content, "Cargo.toml");
        assert!(desc.contains("Cargo workspace"), "got: {desc}");
        assert!(desc.contains("3 members"), "got: {desc}");
        assert!(desc.contains("Deps:"), "got: {desc}");
    }

    #[test]
    fn test_extract_package_json() {
        let content = r#"{
  "name": "my-app",
  "version": "1.2.3",
  "dependencies": {
    "react": "^18.2.0",
    "express": "~4.18.0"
  },
  "devDependencies": {
    "typescript": "^5.6.0",
    "vitest": "^2.0.0"
  }
}"#;
        let desc = extract_description(content, "package.json");
        assert!(desc.contains("my-app@1.2.3"), "got: {desc}");
        assert!(desc.contains("Deps: express 4, react 18"), "got: {desc}");
        assert!(desc.contains("DevDeps:"), "got: {desc}");
        assert!(desc.contains("typescript 5"), "got: {desc}");
    }

    #[test]
    fn test_extract_pyproject_toml_pep621() {
        let content = r#"
[project]
name = "mypkg"
version = "1.2.3"
dependencies = [
    "fastapi>=0.104",
    "pydantic~=2.5",
    "uvicorn[standard]>=0.24",
]
"#;
        let desc = extract_description(content, "pyproject.toml");
        assert!(desc.contains("mypkg 1.2.3"), "got: {desc}");
        assert!(desc.contains("fastapi"), "got: {desc}");
        assert!(desc.contains("pydantic"), "got: {desc}");
    }

    #[test]
    fn test_extract_pyproject_toml_poetry() {
        let content = r#"
[tool.poetry]
name = "poetrypkg"
version = "0.9.0"

[tool.poetry.dependencies]
python = "^3.11"
requests = "^2.31"
httpx = "^0.27"
"#;
        let desc = extract_description(content, "pyproject.toml");
        assert!(desc.contains("poetrypkg 0.9.0"), "got: {desc}");
        assert!(desc.contains("requests"), "got: {desc}");
        assert!(desc.contains("httpx"), "got: {desc}");
        // python itself should be filtered out since it's the interpreter pin
        assert!(!desc.contains("python 3"), "got: {desc}");
    }

    #[test]
    fn test_extract_go_mod() {
        let content = "\
module github.com/foo/bar

go 1.22

require (
    github.com/gorilla/mux v1.8.0
    github.com/jmoiron/sqlx v1.3.5
    github.com/lib/pq v1.10.9
)
";
        let desc = extract_description(content, "go.mod");
        assert!(desc.contains("github.com/foo/bar"), "got: {desc}");
        assert!(desc.contains("go 1.22"), "got: {desc}");
        assert!(desc.contains("mux v1.8.0"), "got: {desc}");
    }

    #[test]
    fn test_extract_pom_xml() {
        let content = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId>
  <artifactId>myapp</artifactId>
  <version>1.2.3</version>
  <dependencies>
    <dependency>
      <groupId>org.springframework</groupId>
      <artifactId>spring-core</artifactId>
      <version>5.3.0</version>
    </dependency>
    <dependency>
      <groupId>com.fasterxml.jackson.core</groupId>
      <artifactId>jackson-databind</artifactId>
      <version>2.15.0</version>
    </dependency>
  </dependencies>
</project>"#;
        let desc = extract_description(content, "pom.xml");
        assert!(desc.contains("myapp 1.2.3"), "got: {desc}");
        assert!(desc.contains("spring-core 5.3.0"), "got: {desc}");
        assert!(desc.contains("jackson-databind 2.15.0"), "got: {desc}");
    }

    #[test]
    fn test_extract_gradle() {
        let content = r#"
dependencies {
    implementation 'org.springframework:spring-core:5.3.0'
    implementation("com.fasterxml.jackson.core:jackson-databind:2.15.0")
    testImplementation 'junit:junit:4.13.2'
}
"#;
        let desc = extract_description(content, "build.gradle");
        assert!(desc.contains("spring-core 5.3.0"), "got: {desc}");
        assert!(desc.contains("jackson-databind 2.15.0"), "got: {desc}");
        assert!(desc.contains("junit 4.13.2"), "got: {desc}");
    }

    #[test]
    fn test_extract_requirements_txt() {
        let content = "\
# Production deps
fastapi>=0.104
pydantic~=2.5
uvicorn==0.24.0
# Dev comment
pytest
";
        let desc = extract_description(content, "requirements.txt");
        assert!(desc.contains("fastapi"), "got: {desc}");
        assert!(desc.contains("pydantic"), "got: {desc}");
    }

    #[test]
    fn test_extract_gemfile() {
        let content = "\
source 'https://rubygems.org'

gem 'rails', '~> 7.0'
gem 'puma'
gem 'pg', '>= 1.1'
";
        let desc = extract_description(content, "Gemfile");
        assert!(desc.contains("rails"), "got: {desc}");
        assert!(desc.contains("puma"), "got: {desc}");
    }

    #[test]
    fn test_extract_composer_json() {
        let content = r#"{
  "name": "vendor/mypkg",
  "version": "1.0.0",
  "require": {
    "php": ">=8.1",
    "symfony/console": "^6.0",
    "monolog/monolog": "^3.0"
  },
  "require-dev": {
    "phpunit/phpunit": "^10.0"
  }
}"#;
        let desc = extract_description(content, "composer.json");
        assert!(desc.contains("vendor/mypkg"), "got: {desc}");
        assert!(desc.contains("symfony/console"), "got: {desc}");
        assert!(!desc.contains(" php "), "got: {desc}");
    }

    #[test]
    fn test_symbol_json_roundtrip() {
        let data = extract_anatomy("pub fn foo() {}\npub struct Bar {}\n", "lib.rs");
        let json = serde_json::to_string(&data.symbols).unwrap();
        let parsed: Vec<Symbol> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, data.symbols);
    }

    #[test]
    fn test_split_pep508() {
        assert_eq!(
            split_pep508("fastapi>=0.104"),
            ("fastapi".into(), ">=0.104".into())
        );
        assert_eq!(split_pep508("requests"), ("requests".into(), "".into()));
        assert_eq!(
            split_pep508("uvicorn[standard]>=0.24"),
            ("uvicorn".into(), "[standard]>=0.24".into())
        );
    }
}
