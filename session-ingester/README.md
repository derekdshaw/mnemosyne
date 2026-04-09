# session-ingester

CLI binary that scans Claude Code JSONL transcript files and ingests them into the Mnemosyne SQLite database.

## Purpose

Claude Code writes a JSONL transcript for every session at `~/.claude/projects/<project-slug>/<session-id>.jsonl`. This tool parses those files and populates the `sessions`, `messages`, `tool_calls`, `token_usage`, and `messages_fts` tables so they can be queried by the MCP server and hooks.

Designed to run on every session start via a `SessionStart` hook, with incremental ingestion so only new or changed files are processed.

## Architecture

```
main.rs
├── CLI parsing (clap): --claude-dir, --verbose
├── Scans ~/.claude/projects/*/*.jsonl
├── For each file:
│   ├── Checks ingestion_log (skip if file_size + file_mtime unchanged)
│   ├── Skips files modified in last 60s (active session)
│   ├── Parses each line via memory_common::jsonl::parse_line()
│   ├── Within a single transaction:
│   │   ├── INSERT OR IGNORE into messages + messages_fts
│   │   ├── INSERT into tool_calls (with extracted file_path)
│   │   ├── INSERT OR IGNORE into token_usage
│   │   ├── UPSERT into sessions (aggregates message count + tokens)
│   │   └── UPDATE ingestion_log
│   └── Supports incremental re-ingestion (seek to file_size offset)
└── Prints summary to stderr
```

### Key Behaviors

- **Idempotent** — Running twice on unchanged files produces no new inserts
- **Active session detection** — Files with mtime < 60 seconds are skipped to avoid ingesting incomplete transcripts
- **Incremental** — Tracks `file_size` and `file_mtime` in `ingestion_log`; if a file has grown, ingests only new lines from the previous offset
- **Deferred foreign keys** — Uses `PRAGMA defer_foreign_keys = ON` so messages can be inserted before their parent session record

## Build

```bash
cargo build -p session-ingester
cargo build -p session-ingester --release  # optimized
```

## Test

```bash
# Run against your real Claude Code transcripts
cargo run -p session-ingester -- --verbose

# Verify data
sqlite3 ~/.claude/memory/memory.db "SELECT count(*) FROM messages;"
sqlite3 ~/.claude/memory/memory.db "SELECT session_id, project, message_count FROM sessions;"
sqlite3 ~/.claude/memory/memory.db "SELECT * FROM messages_fts WHERE messages_fts MATCH 'your search' LIMIT 5;"
```

## Usage

```
session-ingester [OPTIONS]

Options:
  --claude-dir <PATH>  Path to the .claude directory [default: ~/.claude]
  --verbose            Print verbose output
  -h, --help           Print help
```
