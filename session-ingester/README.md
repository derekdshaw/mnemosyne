# session-ingester

CLI binary that scans Claude Code JSONL transcript files and ingests them into the Mnemosyne SQLite database.

## Purpose

Claude Code writes a JSONL transcript for every session at `~/.claude/projects/<project-slug>/<session-id>.jsonl`. This tool parses those files and populates the `sessions`, `messages`, `tool_calls`, `token_usage`, and `messages_fts` tables so they can be queried by the MCP server and hooks.

Designed to run automatically via hooks:
- **SessionStart** — ingests transcripts from any completed sessions since last run
- **SessionEnd** — immediately ingests the just-finished session via `--from-stdin`, so concurrent agents see the data without delay

## Architecture

```
main.rs
├── CLI parsing (clap): --claude-dir, --verbose, --session-id, --from-stdin, --compress-existing
├── Scans ~/.claude/projects/*/*.jsonl
├── For each file:
│   ├── Checks ingestion_log (skip if file_size + file_mtime unchanged)
│   ├── Skips files modified in last 60s (active session), unless --session-id/--from-stdin
│   ├── Parses each line via memory_common::jsonl::parse_line()
│   ├── Within a single transaction (deferred foreign keys):
│   │   ├── DELETE + INSERT into messages_fts (FTS5 dedup)
│   │   ├── INSERT OR IGNORE into messages (via prepare_cached)
│   │   ├── INSERT into tool_calls with extracted file_path
│   │   ├── INSERT OR IGNORE into token_usage
│   │   ├── UPSERT into sessions (aggregates message count + tokens)
│   │   └── UPDATE ingestion_log with line_count + file_size + mtime
│   └── Supports incremental ingestion (skip first N lines if file has grown)
└── Prints summary to stderr
```

### Key Behaviors

- **Idempotent** — Running twice on unchanged files produces no new inserts
- **Active session detection** — Files with mtime < 60 seconds are skipped to avoid ingesting incomplete transcripts
- **Force ingestion** — `--session-id` or `--from-stdin` bypasses the mtime guard for a specific session (used by SessionEnd hook)
- **Incremental** — Tracks `line_count` and `file_mtime` in `ingestion_log`; if a file has grown, skips already-ingested lines
- **FTS dedup** — Uses DELETE before INSERT on FTS5 tables to prevent duplicate rows on re-ingestion
- **Prepared statements** — All INSERT statements use `prepare_cached()` for performance on large transcripts
- **Deferred foreign keys** — Uses `PRAGMA defer_foreign_keys = ON` so messages can be inserted before their parent session record
- **Bounded lines** — Lines exceeding 10MB are skipped to prevent OOM

## Build

```bash
cargo build -p session-ingester
cargo build -p session-ingester --release
```

## Test

```bash
# Run unit tests (6 tests: ingest, idempotency, active skip, force, token usage, tool calls)
cargo test -p session-ingester

# Run against your real Claude Code transcripts
cargo run -p session-ingester -- --verbose

# Optionally verify data (requires sqlite3 CLI: https://www.sqlite.org/download.html)
sqlite3 ~/.claude/memory/memory.db "SELECT count(*) FROM messages;"
sqlite3 ~/.claude/memory/memory.db "SELECT session_id, project, message_count FROM sessions;"
```

## Usage

```
session-ingester [OPTIONS]

Options:
  --claude-dir <PATH>      Path to the .claude directory [default: ~/.claude]
  --verbose                Print verbose output
  --session-id <UUID>      Ingest a specific session immediately (bypasses active-session guard)
  --from-stdin             Read session_id from stdin JSON (for SessionEnd hook)
  --compress-existing      Compress existing uncompressed data using caveman compression
                           via `claude --print`. Idempotent — skips already-compressed rows.
  -h, --help               Print help
```
