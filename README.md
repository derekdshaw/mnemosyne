# Mnemosyne

A session memory system for Claude Code. Named after the Greek goddess of memory.

Mnemosyne gives Claude Code persistent, queryable memory across sessions by ingesting existing JSONL transcripts into SQLite and exposing them through MCP tools and real-time hooks.

## What It Does

- **Ingests session transcripts** — Parses Claude Code's JSONL transcript files into a structured SQLite database with full-text search
- **Provides MCP tools** — 11 tools for searching past sessions, saving context, logging bugs, and managing do-not-repeat rules
- **Real-time hooks** — Warns before re-reading files already read this session, and checks bugs/do-not-repeat rules before writes

## Architecture

```
┌─────────────────────────┐
│ Claude Code (sessions)  │
│ writes JSONL transcripts│
└───────────┬─────────────┘
            │
            ▼
┌───────────────────────┐     ┌──────────────────────┐
│ SessionStart Hook     │────▶│  session-ingester     │
└───────────────────────┘     │  Parses JSONL → SQLite│
                              └──────────┬────────────┘
                                         │
┌───────────────────────┐                ▼
│ PreToolUse/PostToolUse│     ┌──────────────────────┐
│ Hooks (Read/Write)    │◀──▶│  SQLite (WAL mode)    │
└───────────────────────┘     │  ~/.claude/memory/    │
  memory-hooks binary         │    memory.db          │
                              └──────────┬────────────┘
                                         ▲
                              ┌──────────┴────────────┐
                              │  memory-mcp-server     │
                              │  11 MCP tools over     │
                              │  stdio transport       │
                              └────────────────────────┘
```

## Workspace Crates

| Crate | Binary | Purpose |
|-------|--------|---------|
| [memory-common](memory-common/) | (library) | Shared SQLite schema, JSONL parser, data models, path utilities |
| [session-ingester](session-ingester/) | `session-ingester.exe` | CLI that scans and ingests JSONL transcripts into SQLite |
| [memory-mcp-server](memory-mcp-server/) | `memory-mcp-server.exe` | MCP server exposing 11 query/write tools over stdio |
| [memory-hooks](memory-hooks/) | `memory-hooks.exe` | Real-time hook handlers for pre/post read/write events |

## Quick Start

### Build

```bash
cargo build --release
```

Binaries are output to `target/release/`.

### Run the Ingester

```bash
# Ingest all JSONL transcripts from ~/.claude/projects/
./target/release/session-ingester

# With verbose output
./target/release/session-ingester --verbose

# Custom .claude directory
./target/release/session-ingester --claude-dir /path/to/.claude
```

### Configure Claude Code

Add to `~/.claude/settings.json`:

```json
{
  "mcpServers": {
    "memory": {
      "command": "D:/r/mnemosyne/target/release/memory-mcp-server.exe",
      "args": []
    }
  },
  "hooks": {
    "SessionStart": [{
      "matcher": "",
      "hooks": [{
        "type": "command",
        "command": "D:/r/mnemosyne/target/release/session-ingester.exe"
      }]
    }],
    "PreToolUse": [
      {
        "matcher": "Read",
        "hooks": [{
          "type": "command",
          "command": "D:/r/mnemosyne/target/release/memory-hooks.exe pre-read"
        }]
      },
      {
        "matcher": "Write|Edit",
        "hooks": [{
          "type": "command",
          "command": "D:/r/mnemosyne/target/release/memory-hooks.exe pre-write"
        }]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Read",
        "hooks": [{
          "type": "command",
          "command": "D:/r/mnemosyne/target/release/memory-hooks.exe post-read"
        }]
      },
      {
        "matcher": "Write|Edit",
        "hooks": [{
          "type": "command",
          "command": "D:/r/mnemosyne/target/release/memory-hooks.exe post-write"
        }]
      }
    ]
  }
}
```

Adjust paths to match your system. Use absolute paths with forward slashes on Windows.

### Run Tests

```bash
cargo test
```

## Database

SQLite database is stored at `~/.claude/memory/memory.db` with WAL mode enabled for concurrent access.

### Tables

**Ingestion tables** (populated from JSONL transcripts):
- `sessions` — Session metadata (project, timestamps, token totals)
- `messages` — All user/assistant messages with full-text search via `messages_fts`
- `tool_calls` — Tool invocations with file paths and input summaries
- `token_usage` — Per-message token counts
- `ingestion_log` — Tracks which files have been ingested (with size/mtime for incremental updates)

**Context tables** (read-write via MCP tools):
- `context_items` — Saved knowledge with categories, searchable via `context_fts`
- `bugs` — Logged bugs with error messages and fixes, searchable via `bugs_fts`
- `do_not_repeat` — Rules for mistakes to avoid, queried by exact project/file match

**Hook tables** (populated by real-time hooks):
- `file_anatomy` — Per-project file index with descriptions and token estimates
- `session_reads` — Files read in the current session (for repeated-read detection)

## MCP Tools

| Tool | Type | Description |
|------|------|-------------|
| `search_sessions` | read | FTS5 search across all past messages |
| `get_recent_sessions` | read | List recent sessions by project |
| `get_session_detail` | read | Full session metadata with first/last messages and tool summary |
| `get_file_history` | read | Tool calls that touched a specific file |
| `save_context` | write | Save a context item with category |
| `search_context` | read | FTS5 search on saved context |
| `get_project_summary` | read | All context, bugs, and do-not-repeat rules for a project |
| `log_bug` | write | Log a bug with error message, fix, and optional root cause |
| `search_bugs` | read | FTS5 search on logged bugs |
| `add_do_not_repeat` | write | Add a do-not-repeat rule |
| `get_do_not_repeat` | read | List do-not-repeat rules for a project/file |

## Hook Behavior

All hooks are **advisory only** — they always exit 0 and never block tool execution.

| Hook | Event | What It Does |
|------|-------|-------------|
| `pre-read` | Before file read | Shows anatomy info, warns on repeated reads |
| `post-read` | After file read | Records the read, updates anatomy stats |
| `pre-write` | Before file write/edit | Warns about known bugs and do-not-repeat rules for the file |
| `post-write` | After file write/edit | Updates anatomy write count and modification time |
