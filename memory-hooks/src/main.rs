//! Real-time hook handlers for Claude Code's PreToolUse and PostToolUse events.
//!
//! A single binary with subcommands for each hook type. Spawned by Claude Code
//! on every file read/write. All hooks are advisory only — they write warnings
//! to stderr and always exit 0, never blocking tool execution.

mod pre_read;
mod post_read;
mod pre_write;
mod post_write;

use clap::{Parser, Subcommand};
use memory_common::db;
use serde::Deserialize;
use std::io::Read as _;

#[derive(Parser)]
#[command(name = "memory-hooks", about = "Mnemosyne real-time hooks for Claude Code")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check file anatomy and warn on repeated reads
    PreRead,
    /// Track file read in session, update anatomy
    PostRead,
    /// Check bugs and do-not-repeat rules before write
    PreWrite,
    /// Update anatomy after file write
    PostWrite,
}

/// Common fields from Claude Code hook stdin JSON.
/// Claude Code uses snake_case for hook input fields.
#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
}

impl HookInput {
    /// Extract file_path from tool_input, normalized to forward slashes.
    pub fn file_path(&self) -> Option<String> {
        self.tool_input
            .as_ref()
            .and_then(|input| input.get("file_path"))
            .and_then(|v| v.as_str())
            .map(|s| db::normalize_path(s))
    }

    /// Derive project name from cwd.
    pub fn project(&self) -> Option<String> {
        self.cwd.as_deref().map(db::project_from_cwd)
    }
}

fn main() {
    let cli = Cli::parse();

    // S10: Read stdin JSON with 1MB limit to prevent OOM from malicious input
    let mut input_str = String::new();
    if let Err(e) = std::io::stdin().take(1_048_576).read_to_string(&mut input_str) {
        eprintln!("mnemosyne: failed to read stdin: {e}");
        std::process::exit(0); // Never block — exit 0
    }

    let hook_input: HookInput = match serde_json::from_str(&input_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mnemosyne: failed to parse hook input: {e}");
            std::process::exit(0);
        }
    };

    // Open DB — if it fails, exit silently (don't block Claude)
    let conn = match db::open_db() {
        Ok(c) => c,
        Err(_) => std::process::exit(0),
    };

    let result = match cli.command {
        Command::PreRead => pre_read::run(&conn, &hook_input),
        Command::PostRead => post_read::run(&conn, &hook_input),
        Command::PreWrite => pre_write::run(&conn, &hook_input),
        Command::PostWrite => post_write::run(&conn, &hook_input),
    };

    if let Err(e) = result {
        eprintln!("mnemosyne: hook error: {e}");
    }

    // Always exit 0 — hooks are advisory only
    std::process::exit(0);
}
