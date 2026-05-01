//! Tracing setup shared by all Mnemosyne binaries.
//!
//! All logs go to stderr (Claude Code captures it under
//! `~/Library/Caches/claude-cli-nodejs/<cwd>/mcp-logs-mnemosyne/`) and, when
//! `MNEMOSYNE_LOG_FILE` is set or the default log dir is writable, also to a
//! file at `~/.claude/memory/logs/<binary>.log`. The file path is reachable
//! by the user without digging through Claude Code's cache, which matters
//! when the only signal we have is "the MCP server isn't responding".
//!
//! ## Environment variables
//!
//! - `MNEMOSYNE_LOG_ENABLED` — master kill switch. Set to `0`, `false`,
//!   `off`, or `no` (case-insensitive) to disable both stderr and file
//!   logging entirely. Default: ON.
//! - `MNEMOSYNE_LOG` / `RUST_LOG` — `EnvFilter` directives (e.g. `info`,
//!   `debug,rmcp=warn`). Falls back to each binary's `default_filter`.
//! - `MNEMOSYNE_LOG_FILE` — explicit log file path. Set to empty or `-`
//!   to keep stderr only.
//! - `MNEMOSYNE_LOG_MAX_MB` — log file rotation threshold in megabytes.
//!   Default: 20. When the file exceeds this size at process startup, it
//!   is renamed to `<binary>.log.<UTC timestamp>` and a fresh file is
//!   started. Timestamped names mean rotations never overwrite each
//!   other; pruning old rotations is the operator's responsibility. The
//!   check runs once inside `init()`, never per-write — for the MCP
//!   server and session-ingester that's once at session start; for
//!   memory-hooks it's one `stat(2)` per invocation, which is
//!   microseconds and not on any hot path.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Default rotation threshold in megabytes if `MNEMOSYNE_LOG_MAX_MB` is unset.
const DEFAULT_MAX_MB: u64 = 20;

/// `true` if `MNEMOSYNE_LOG_ENABLED` is set to a recognized falsey value.
/// Anything else (including unset) means logging stays on.
fn logging_disabled() -> bool {
    let Ok(raw) = std::env::var("MNEMOSYNE_LOG_ENABLED") else {
        return false;
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

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

/// Resolve the rotation threshold in bytes from `MNEMOSYNE_LOG_MAX_MB`,
/// defaulting to `DEFAULT_MAX_MB`.
fn resolve_max_bytes() -> u64 {
    std::env::var("MNEMOSYNE_LOG_MAX_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_MB)
        .saturating_mul(1024 * 1024)
}

/// If the file at `path` exists and exceeds `max_bytes`, rename it to
/// `<path>.<UTC timestamp>` so the subsequent open creates a fresh file.
/// Timestamp format `%Y%m%dT%H%M%SZ` is sortable, filesystem-safe on Windows
/// (no colons), and fine-grained enough that two rotations within the same
/// second never collide in practice — but if they do, the second `rename`
/// is a no-op overwrite of an identical file, which is acceptable.
/// Best-effort: any I/O error is ignored — failing to rotate must never
/// prevent logging.
fn maybe_rotate(path: &Path, max_bytes: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= max_bytes {
        return;
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut rotated: OsString = path.as_os_str().to_owned();
    rotated.push(".");
    rotated.push(stamp);
    let _ = std::fs::rename(path, PathBuf::from(rotated));
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
/// configured (or if logging was disabled entirely via
/// `MNEMOSYNE_LOG_ENABLED`). Failures opening the log file are non-fatal —
/// we always keep stderr so the binary still emits diagnostics.
pub fn init(binary: &str, default_filter: &str) -> bool {
    if logging_disabled() {
        return false;
    }

    let filter = std::env::var("MNEMOSYNE_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|s| EnvFilter::try_new(&s).ok())
        .unwrap_or_else(|| EnvFilter::new(default_filter));

    let file_path = resolve_log_file(binary);
    let max_bytes = resolve_max_bytes();
    let file = file_path.as_ref().and_then(|p| {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        maybe_rotate(p, max_bytes);
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
