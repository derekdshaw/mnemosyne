//! Tracing setup shared by all Mnemosyne binaries.
//!
//! All logs go to stderr (Claude Code captures it under
//! `~/Library/Caches/claude-cli-nodejs/<cwd>/mcp-logs-mnemosyne/`) and, when
//! `MNEMOSYNE_LOG_FILE` is set or the default log dir is writable, also to a
//! file at `~/.claude/memory/logs/<binary>.log`. The file path is reachable
//! by the user without digging through Claude Code's cache, which matters
//! when the only signal we have is "the MCP server isn't responding".

use std::fs::OpenOptions;
use std::path::PathBuf;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Default log directory: `~/.claude/memory/logs`.
fn default_log_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("memory").join("logs"))
}

/// Resolve the log file path for `binary`. Honors `MNEMOSYNE_LOG_FILE` (treated
/// as an explicit override that disables file logging when set to empty/`-`)
/// and otherwise falls back to `<default_log_dir>/<binary>.log`.
fn resolve_log_file(binary: &str) -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("MNEMOSYNE_LOG_FILE") {
        if override_path.is_empty() || override_path == "-" {
            return None;
        }
        return Some(PathBuf::from(override_path));
    }
    default_log_dir().map(|d| d.join(format!("{binary}.log")))
}

/// Initialize tracing for a Mnemosyne binary. Idempotent within a process —
/// subsequent calls become no-ops via `try_init`.
///
/// `binary` is the short name used for the default log file
/// (`memory-mcp-server`, `memory-hooks`, etc.).
///
/// Filter precedence: `MNEMOSYNE_LOG` > `RUST_LOG` > the supplied
/// `default_filter` (e.g. `"info"`).
///
/// Returns `true` if file logging was enabled, `false` if only stderr was
/// configured. Failures opening the log file are non-fatal — we always keep
/// stderr so the binary still emits diagnostics.
pub fn init(binary: &str, default_filter: &str) -> bool {
    let filter = std::env::var("MNEMOSYNE_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|s| EnvFilter::try_new(&s).ok())
        .unwrap_or_else(|| EnvFilter::new(default_filter));

    let file_path = resolve_log_file(binary);
    let file = file_path.as_ref().and_then(|p| {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        OpenOptions::new().create(true).append(true).open(p).ok()
    });

    // Disable ANSI globally. Per-layer `with_ansi(false)` doesn't fully strip
    // codes from span field rendering because tracing-subscriber caches the
    // formatted span fields under the `FormatFields` type and the first layer
    // to render a span wins. Claude Code's mcp-logs view sanitizes stderr
    // anyway, so colors weren't buying us much in practice.
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::io::stderr);
    let file_layer = file.map(|f| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(f)
    });

    let result = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init();

    let initialized_file_logging = result.is_ok() && file_path.is_some();
    if let (Some(p), true) = (file_path.as_ref(), initialized_file_logging) {
        tracing::debug!(log_file = %p.display(), "tracing initialized");
    }
    initialized_file_logging
}
