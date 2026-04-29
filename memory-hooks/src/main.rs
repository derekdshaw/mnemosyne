//! Real-time hook handlers for Claude Code's PreToolUse, PostToolUse, and
//! SessionStart events.
//!
//! A single binary with subcommands for each hook type. Spawned by Claude Code
//! on every file read/write and at session start. All hooks are advisory only —
//! they write warnings to stderr and always exit 0, never blocking tool execution.

mod post_read;
mod post_write;
mod pre_read;
mod pre_write;
mod session_start;

use clap::{Parser, Subcommand};
use memory_common::db;
use serde::Deserialize;
use std::io::Read as _;

#[derive(Parser)]
#[command(
    name = "memory-hooks",
    about = "Mnemosyne real-time hooks for Claude Code"
)]
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
    /// Print project summary at session start (do-not-repeat rules, context, bugs)
    SessionStart,
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
            .map(db::normalize_path)
    }

    /// Derive project name from cwd.
    pub fn project(&self) -> Option<String> {
        self.cwd.as_deref().map(db::project_from_cwd)
    }
}

fn main() {
    memory_common::logging::init("memory-hooks", "info");

    let cli = Cli::parse();
    let hook_name: &'static str = match cli.command {
        Command::PreRead => "pre_read",
        Command::PostRead => "post_read",
        Command::PreWrite => "pre_write",
        Command::PostWrite => "post_write",
        Command::SessionStart => "session_start",
    };
    let span = tracing::info_span!("hook", name = hook_name, pid = std::process::id());
    let _enter = span.enter();
    tracing::debug!("hook invoked");

    // S10: Read stdin JSON with 1MB limit to prevent OOM from malicious input
    let mut input_str = String::new();
    if let Err(e) = std::io::stdin()
        .take(1_048_576)
        .read_to_string(&mut input_str)
    {
        tracing::error!(error = %e, "failed to read stdin");
        std::process::exit(0); // Never block — exit 0
    }

    let hook_input: HookInput = match serde_json::from_str(&input_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, input_len = input_str.len(), "failed to parse hook input");
            std::process::exit(0);
        }
    };

    // Open DB — if it fails, log and exit silently (don't block Claude)
    let conn = match db::open_db() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = ?e, "failed to open DB; hook is advisory, exiting cleanly");
            std::process::exit(0);
        }
    };

    let result = match cli.command {
        Command::PreRead => pre_read::run(&conn, &hook_input),
        Command::PostRead => post_read::run(&conn, &hook_input),
        Command::PreWrite => pre_write::run(&conn, &hook_input),
        Command::PostWrite => post_write::run(&conn, &hook_input),
        Command::SessionStart => session_start::run(&conn, &hook_input),
    };

    match result {
        Ok(bytes) => {
            tracing::debug!(bytes, "hook ok");
            if let Err(e) = db::record_overhead(
                &conn,
                hook_input.session_id.as_deref(),
                hook_input.project().as_deref(),
                hook_name,
                bytes,
            ) {
                tracing::warn!(error = %e, "failed to record hook overhead");
            }
        }
        Err(e) => {
            tracing::error!(error = ?e, "hook handler returned error");
        }
    }

    // Always exit 0 — hooks are advisory only
    std::process::exit(0);
}
