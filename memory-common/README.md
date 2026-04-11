# memory-common

Shared library crate for the Mnemosyne workspace. Provides the SQLite schema, JSONL transcript parser, data models, and utility functions used by all other crates.

## Purpose

This crate is the foundation layer. It owns:

- **Database setup** — Opening the SQLite database, configuring WAL mode and PRAGMAs, running schema migrations
- **Schema definitions** — All 12 tables and 3 FTS5 virtual tables as SQL constants
- **JSONL parser** — Streaming parser for Claude Code's transcript format (`~/.claude/projects/*/*.jsonl`)
- **File anatomy extraction** — Extracts content-aware descriptions from source files (doc comments, public signatures, exports) for 9 languages
- **Data models** — Rust structs for all database entities
- **Path utilities** — Normalizing file paths to forward slashes for consistent cross-platform storage, deriving project names from working directories

## Architecture

```
lib.rs
├── anatomy.rs  — extract_description(): content-aware file summaries for 9 languages
├── db.rs       — open_db(), run_migrations(), normalize_path(), project_from_cwd(), truncate_utf8()
├── schema.rs   — SQL DDL constants (CREATE TABLE, CREATE INDEX, FTS5)
├── models.rs   — Serde-enabled structs: Session, Message, ToolCall, Bug, etc.
└── jsonl.rs    — parse_line() → Record enum, extract_file_path(), extract_tool_input_summary()
```

### JSONL Parser

The parser handles Claude Code's transcript format where each line is a JSON object with a `type` field:

- `"user"` → `Record::UserMessage` (handles both string and array content, including tool results)
- `"assistant"` → `Record::AssistantMessage` (extracts text, thinking blocks truncated to 500 chars, tool_use blocks)
- `"system"`, `"permission-mode"`, `"file-history-snapshot"`, `"queue-operation"`, etc. → `Record::Skip`
- Malformed lines → parse error (caller decides whether to skip or abort)

### Database

`open_db()` resolves `~/.claude/memory/memory.db` using the `dirs` crate (portable across Windows/Linux/macOS), creates the directory if needed, then:

1. Sets `PRAGMA journal_mode=WAL` (concurrent reads)
2. Sets `PRAGMA busy_timeout=3000` (retry on SQLITE_BUSY)
3. Sets `PRAGMA synchronous=NORMAL` (reduces fsync for hooks)
4. Runs all schema migrations idempotently

## Build

```bash
cargo build -p memory-common
```

## Test

```bash
cargo test -p memory-common
```

34 tests covering database creation, migration idempotency, schema version skip, JSONL parsing (user messages, assistant messages, tool results, array content, missing usage, thinking block truncation, skip types, malformed lines), file path extraction, tool input summaries, path normalization, UTF-8 truncation (ASCII, emoji, CJK, empty, boundary), and anatomy extraction (Rust, Python, TypeScript, Java, Go, Markdown, TOML, empty, fallback).
