//! Caveman compression for the `--compress-existing` migration.
//!
//! Calls `claude --print` to compress natural language text while preserving
//! code blocks, URLs, headings, file paths, and commands. Validates output
//! integrity and retries on failure. Falls back to original text on any error.

use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashSet;
use std::process::Command;
use std::sync::LazyLock;

/// Minimum text length worth compressing (bytes).
const MIN_COMPRESS_LEN: usize = 500;

/// Maximum retry attempts after validation failure.
const MAX_RETRIES: usize = 2;

/// Number of items per batched `claude --print` call.
pub const BATCH_SIZE: usize = 15;

// --- Regex patterns (compiled once) ---

static HEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^(#{1,6})\s+(.*)$").unwrap());
static URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://[^\s)\]>]+").unwrap());
static PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\./|\.\./|/|[A-Za-z]:\\)[\w\-/\\\.]+|[\w\-\.]+[/\\][\w\-/\\\.]+").unwrap()
});
static BULLET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*[-*+]\s+").unwrap());
static FENCE_OPEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s{0,3})(`{3,}|~{3,})(.*)$").unwrap());
// OUTER_FENCE_RE removed — strip_llm_wrapper uses manual parsing instead
// because Rust regex doesn't support backreferences.
static BATCH_MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\[(\d+)\]\s*$").unwrap());

/// Result of a compression attempt.
pub struct CompressResult {
    pub text: String,
    pub original_length: usize,
    pub was_compressed: bool,
}

/// Validation result with hard errors and soft warnings.
pub struct ValidationResult {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationResult {
    fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

// --- Public API ---

/// Compress a single text via `claude --print`. Falls back to original on failure.
pub fn compress(text: &str) -> CompressResult {
    let original_length = text.len();
    if original_length < MIN_COMPRESS_LEN {
        return CompressResult {
            text: text.to_string(),
            original_length,
            was_compressed: false,
        };
    }
    match compress_with_retry(text) {
        Ok(compressed) => CompressResult {
            text: compressed,
            original_length,
            was_compressed: true,
        },
        Err(e) => {
            tracing::warn!("compression failed, using original: {e}");
            CompressResult {
                text: text.to_string(),
                original_length,
                was_compressed: false,
            }
        }
    }
}

/// Batch compress multiple texts in a single `claude --print` call.
/// Falls back per-item on parse or validation failure.
pub fn compress_batch(texts: &[&str]) -> Vec<CompressResult> {
    // Filter out short texts, track indices
    let mut results: Vec<CompressResult> = texts
        .iter()
        .map(|t| CompressResult {
            text: t.to_string(),
            original_length: t.len(),
            was_compressed: false,
        })
        .collect();

    let compressible: Vec<(usize, &str)> = texts
        .iter()
        .enumerate()
        .filter(|(_, t)| t.len() >= MIN_COMPRESS_LEN)
        .map(|(i, t)| (i, *t))
        .collect();

    if compressible.is_empty() {
        return results;
    }

    let prompt = build_batch_prompt(&compressible.iter().map(|(_, t)| *t).collect::<Vec<_>>());
    let response = match call_claude(&prompt) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("batch compression call failed: {e}");
            return results;
        }
    };

    let parsed = parse_batch_response(&response, compressible.len());

    for (batch_idx, (orig_idx, original)) in compressible.iter().enumerate() {
        if let Some(compressed) = parsed.get(batch_idx).and_then(|o| o.as_ref()) {
            let vr = validate(original, compressed);
            if vr.is_valid() {
                results[*orig_idx] = CompressResult {
                    text: compressed.clone(),
                    original_length: original.len(),
                    was_compressed: true,
                };
            } else {
                for w in &vr.warnings {
                    tracing::debug!("batch item {}: warning: {w}", batch_idx + 1);
                }
                for e in &vr.errors {
                    tracing::warn!("batch item {}: validation error: {e}", batch_idx + 1);
                }
            }
        }
    }

    results
}

// --- Internal Functions ---

fn compress_with_retry(text: &str) -> Result<String> {
    let prompt = build_compress_prompt(text);
    let mut compressed = call_claude(&prompt)?;

    for attempt in 0..MAX_RETRIES {
        let vr = validate(text, &compressed);
        for w in &vr.warnings {
            tracing::debug!("attempt {}: warning: {w}", attempt + 1);
        }
        if vr.is_valid() {
            return Ok(compressed);
        }
        for e in &vr.errors {
            tracing::warn!("attempt {}: error: {e}", attempt + 1);
        }
        if attempt == MAX_RETRIES - 1 {
            anyhow::bail!("validation failed after {MAX_RETRIES} retries");
        }
        let fix_prompt = build_fix_prompt(text, &compressed, &vr.errors);
        compressed = call_claude(&fix_prompt)?;
    }

    Ok(compressed)
}

fn call_claude(prompt: &str) -> Result<String> {
    let output = Command::new("claude")
        .args(["--print"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(prompt.as_bytes())?;
            }
            // Drop stdin so the child sees EOF
            child.stdin.take();
            child.wait_with_output()
        })
        .context("failed to run 'claude --print'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude --print failed ({}): {}", output.status, stderr);
    }

    let text = String::from_utf8(output.stdout)
        .context("claude output is not valid UTF-8")?
        .trim()
        .to_string();

    Ok(strip_llm_wrapper(&text))
}

/// Strip outer ```markdown ... ``` fence when it wraps the entire output.
fn strip_llm_wrapper(text: &str) -> String {
    let trimmed = text.trim();
    // Check if text starts with a fence (``` or ~~~)
    let fence_char = match trimmed.chars().next() {
        Some('`') => '`',
        Some('~') => '~',
        _ => return text.to_string(),
    };

    // Count fence length
    let fence_len = trimmed.chars().take_while(|&c| c == fence_char).count();
    if fence_len < 3 {
        return text.to_string();
    }
    let fence = &trimmed[..fence_len];

    // Check first line is a fence (possibly with info string like "markdown")
    let first_newline = match trimmed.find('\n') {
        Some(pos) => pos,
        None => return text.to_string(),
    };

    // Check if text ends with the same fence
    let last_newline = match trimmed.rfind('\n') {
        Some(pos) if pos > first_newline => pos,
        _ => return text.to_string(),
    };
    let closing = trimmed[last_newline + 1..].trim();
    if closing != fence {
        return text.to_string();
    }

    // Extract inner content
    trimmed[first_newline + 1..last_newline].to_string()
}

// --- Prompts ---

fn build_compress_prompt(original: &str) -> String {
    format!(
        "Compress this markdown into caveman format.\n\
         \n\
         STRICT RULES:\n\
         - Do NOT modify anything inside ``` code blocks\n\
         - Do NOT modify anything inside inline backticks\n\
         - Preserve ALL URLs exactly\n\
         - Preserve ALL headings exactly\n\
         - Preserve file paths and commands\n\
         - Return ONLY the compressed markdown body — do NOT wrap the entire output \
           in a ```markdown fence or any other fence. Inner code blocks from the \
           original stay as-is; do not add a new outer fence around the whole file.\n\
         \n\
         Only compress natural language.\n\
         \n\
         TEXT:\n\
         {original}"
    )
}

fn build_fix_prompt(original: &str, compressed: &str, errors: &[String]) -> String {
    let errors_str: String = errors.iter().map(|e| format!("- {e}\n")).collect();
    format!(
        "You are fixing a caveman-compressed markdown file. Specific validation errors were found.\n\
         \n\
         CRITICAL RULES:\n\
         - DO NOT recompress or rephrase the file\n\
         - ONLY fix the listed errors — leave everything else exactly as-is\n\
         - The ORIGINAL is provided as reference only (to restore missing content)\n\
         - Preserve caveman style in all untouched sections\n\
         \n\
         ERRORS TO FIX:\n\
         {errors_str}\n\
         HOW TO FIX:\n\
         - Missing URL: find it in ORIGINAL, restore it exactly where it belongs in COMPRESSED\n\
         - Code block mismatch: find the exact code block in ORIGINAL, restore it in COMPRESSED\n\
         - Heading mismatch: restore the exact heading text from ORIGINAL into COMPRESSED\n\
         - Do not touch any section not mentioned in the errors\n\
         \n\
         ORIGINAL (reference only):\n\
         {original}\n\
         \n\
         COMPRESSED (fix this):\n\
         {compressed}\n\
         \n\
         Return ONLY the fixed compressed file. No explanation."
    )
}

fn build_batch_prompt(texts: &[&str]) -> String {
    let mut prompt = String::from(
        "Compress each numbered section below into caveman format.\n\
         \n\
         STRICT RULES:\n\
         - Do NOT modify anything inside ``` code blocks\n\
         - Do NOT modify anything inside inline backticks\n\
         - Preserve ALL URLs exactly\n\
         - Preserve ALL headings exactly\n\
         - Preserve file paths and commands\n\
         - Return each section prefixed with its number marker [1], [2], etc.\n\
         - Do NOT wrap output in a ```markdown fence.\n\
         \n\
         Only compress natural language.\n\n",
    );

    for (i, text) in texts.iter().enumerate() {
        prompt.push_str(&format!("[{}]\n{}\n\n", i + 1, text));
    }

    prompt
}

fn parse_batch_response(response: &str, expected_count: usize) -> Vec<Option<String>> {
    let mut results: Vec<Option<String>> = vec![None; expected_count];

    // Find all [N] markers and their positions
    let markers: Vec<(usize, usize)> = BATCH_MARKER_RE
        .captures_iter(response)
        .filter_map(|cap| {
            let num: usize = cap[1].parse().ok()?;
            if num >= 1 && num <= expected_count {
                Some((num - 1, cap.get(0)?.end()))
            } else {
                None
            }
        })
        .collect();

    for (i, &(idx, start)) in markers.iter().enumerate() {
        let end = markers
            .get(i + 1)
            .map(|&(_, _)| {
                // Find the start of the next marker line
                BATCH_MARKER_RE
                    .find_at(response, start)
                    .map(|m| m.start())
                    .unwrap_or(response.len())
            })
            .unwrap_or(response.len());

        // Actually get the text between this marker and the next
        let end = if i + 1 < markers.len() {
            // Find position of next [N] marker
            let next_marker_start = response[start..]
                .find(&format!("\n[{}]", markers[i + 1].0 + 1))
                .map(|pos| start + pos)
                .unwrap_or(end);
            next_marker_start
        } else {
            response.len()
        };

        let section = response[start..end].trim().to_string();
        if !section.is_empty() {
            results[idx] = Some(section);
        }
    }

    results
}

// --- Validation ---

/// Validate compression output against original.
pub fn validate(original: &str, compressed: &str) -> ValidationResult {
    let mut result = ValidationResult {
        errors: Vec::new(),
        warnings: Vec::new(),
    };

    validate_headings(original, compressed, &mut result);
    validate_code_blocks(original, compressed, &mut result);
    validate_urls(original, compressed, &mut result);
    validate_paths(original, compressed, &mut result);
    validate_bullets(original, compressed, &mut result);

    result
}

fn extract_headings(text: &str) -> Vec<(String, String)> {
    HEADING_RE
        .captures_iter(text)
        .map(|cap| (cap[1].to_string(), cap[2].trim().to_string()))
        .collect()
}

fn extract_code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        if let Some(caps) = FENCE_OPEN_RE.captures(lines[i]) {
            let fence_char = caps[2].chars().next().unwrap();
            let fence_len = caps[2].len();
            let mut block_lines = vec![lines[i]];
            i += 1;
            let mut closed = false;

            while i < lines.len() {
                if let Some(close_caps) = FENCE_OPEN_RE.captures(lines[i]) {
                    let close_char = close_caps[2].chars().next().unwrap();
                    let close_len = close_caps[2].len();
                    if close_char == fence_char
                        && close_len >= fence_len
                        && close_caps[3].trim().is_empty()
                    {
                        block_lines.push(lines[i]);
                        closed = true;
                        i += 1;
                        break;
                    }
                }
                block_lines.push(lines[i]);
                i += 1;
            }

            if closed {
                blocks.push(block_lines.join("\n"));
            }
        } else {
            i += 1;
        }
    }

    blocks
}

fn extract_urls(text: &str) -> HashSet<String> {
    URL_RE
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn extract_paths(text: &str) -> HashSet<String> {
    PATH_RE
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn count_bullets(text: &str) -> usize {
    BULLET_RE.find_iter(text).count()
}

fn validate_headings(original: &str, compressed: &str, result: &mut ValidationResult) {
    let h1 = extract_headings(original);
    let h2 = extract_headings(compressed);

    if h1.len() != h2.len() {
        result.errors.push(format!(
            "Heading count mismatch: {} vs {}",
            h1.len(),
            h2.len()
        ));
    }
    if h1 != h2 {
        result
            .warnings
            .push("Heading text/order changed".to_string());
    }
}

fn validate_code_blocks(original: &str, compressed: &str, result: &mut ValidationResult) {
    let c1 = extract_code_blocks(original);
    let c2 = extract_code_blocks(compressed);

    if c1 != c2 {
        result
            .errors
            .push("Code blocks not preserved exactly".to_string());
    }
}

fn validate_urls(original: &str, compressed: &str, result: &mut ValidationResult) {
    let u1 = extract_urls(original);
    let u2 = extract_urls(compressed);

    if u1 != u2 {
        let lost: Vec<_> = u1.difference(&u2).collect();
        let added: Vec<_> = u2.difference(&u1).collect();
        result
            .errors
            .push(format!("URL mismatch: lost={lost:?}, added={added:?}"));
    }
}

fn validate_paths(original: &str, compressed: &str, result: &mut ValidationResult) {
    let p1 = extract_paths(original);
    let p2 = extract_paths(compressed);

    if p1 != p2 {
        let lost: Vec<_> = p1.difference(&p2).collect();
        let added: Vec<_> = p2.difference(&p1).collect();
        result
            .warnings
            .push(format!("Path mismatch: lost={lost:?}, added={added:?}"));
    }
}

fn validate_bullets(original: &str, compressed: &str, result: &mut ValidationResult) {
    let b1 = count_bullets(original);
    let b2 = count_bullets(compressed);

    if b1 == 0 {
        return;
    }

    let diff = (b1 as f64 - b2 as f64).abs() / b1 as f64;
    if diff > 0.15 {
        result
            .warnings
            .push(format!("Bullet count changed too much: {b1} -> {b2}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_short_text_skipped() {
        let result = compress("short text");
        assert!(!result.was_compressed);
        assert_eq!(result.text, "short text");
        assert_eq!(result.original_length, 10);
    }

    #[test]
    fn test_validate_headings_preserved() {
        let orig = "# Title\n\nSome text\n\n## Section\n\nMore text";
        let comp = "# Title\n\nText\n\n## Section\n\nMore";
        let vr = validate(orig, comp);
        assert!(vr.is_valid());
    }

    #[test]
    fn test_validate_heading_count_mismatch() {
        let orig = "# Title\n\n## Section 1\n\n## Section 2";
        let comp = "# Title\n\n## Section 1";
        let vr = validate(orig, comp);
        assert!(!vr.is_valid());
        assert!(vr.errors[0].contains("Heading count mismatch"));
    }

    #[test]
    fn test_validate_code_blocks_preserved() {
        let orig = "Text\n\n```rust\nfn main() {}\n```\n\nMore";
        let comp = "Text\n\n```rust\nfn main() {}\n```\n\nMore";
        let vr = validate(orig, comp);
        assert!(vr.is_valid());
    }

    #[test]
    fn test_validate_code_block_modified() {
        let orig = "Text\n\n```rust\nfn main() {}\n```";
        let comp = "Text\n\n```rust\nfn main() { println!(); }\n```";
        let vr = validate(orig, comp);
        assert!(!vr.is_valid());
        assert!(vr.errors[0].contains("Code blocks"));
    }

    #[test]
    fn test_validate_urls_preserved() {
        let orig = "Visit https://example.com and https://other.com";
        let comp = "Visit https://example.com and https://other.com";
        let vr = validate(orig, comp);
        assert!(vr.is_valid());
    }

    #[test]
    fn test_validate_url_missing() {
        let orig = "Visit https://example.com and https://other.com";
        let comp = "Visit https://example.com";
        let vr = validate(orig, comp);
        assert!(!vr.is_valid());
        assert!(vr.errors[0].contains("URL mismatch"));
    }

    #[test]
    fn test_validate_paths_warning() {
        let orig = "File at ./src/main.rs and /usr/bin/test";
        let comp = "File at ./src/main.rs";
        let vr = validate(orig, comp);
        assert!(vr.is_valid()); // paths are warnings, not errors
        assert!(!vr.warnings.is_empty());
    }

    #[test]
    fn test_validate_bullet_count_ok() {
        let orig = "- item 1\n- item 2\n- item 3\n- item 4\n- item 5";
        let comp = "- item 1\n- item 2\n- item 3\n- item 4\n- combined";
        let vr = validate(orig, comp);
        assert!(vr.is_valid());
        assert!(vr.warnings.is_empty()); // 5 vs 5, no change
    }

    #[test]
    fn test_validate_bullet_count_warning() {
        let orig = "- a\n- b\n- c\n- d\n- e\n- f\n- g\n- h\n- i\n- j";
        let comp = "- a\n- b\n- c";
        let vr = validate(orig, comp);
        assert!(vr.is_valid()); // warnings don't fail
        assert!(vr.warnings.iter().any(|w| w.contains("Bullet count")));
    }

    #[test]
    fn test_strip_llm_wrapper() {
        let wrapped = "```markdown\n# Title\n\nContent\n```";
        assert_eq!(strip_llm_wrapper(wrapped), "# Title\n\nContent");

        let unwrapped = "# Title\n\nContent";
        assert_eq!(strip_llm_wrapper(unwrapped), unwrapped);
    }

    #[test]
    fn test_extract_code_blocks_nested() {
        let text = "````\n```\ninner\n```\n````";
        let blocks = extract_code_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], "````\n```\ninner\n```\n````");
    }

    #[test]
    fn test_build_batch_prompt() {
        let texts = vec!["Hello world", "Goodbye world"];
        let prompt = build_batch_prompt(&texts);
        assert!(prompt.contains("[1]\nHello world"));
        assert!(prompt.contains("[2]\nGoodbye world"));
    }

    #[test]
    fn test_parse_batch_response() {
        let response = "[1]\nCompressed first\n\n[2]\nCompressed second";
        let parsed = parse_batch_response(response, 2);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].as_deref(), Some("Compressed first"));
        assert_eq!(parsed[1].as_deref(), Some("Compressed second"));
    }

    #[test]
    fn test_parse_batch_response_missing_item() {
        let response = "[1]\nOnly first item";
        let parsed = parse_batch_response(response, 3);
        assert_eq!(parsed.len(), 3);
        assert!(parsed[0].is_some());
        assert!(parsed[1].is_none());
        assert!(parsed[2].is_none());
    }
}
