# Mnemosyne

A session memory system for Claude Code. Named after the Greek goddess of memory.

Mnemosyne gives Claude Code persistent, queryable memory across sessions by ingesting existing JSONL transcripts into SQLite and exposing them through MCP tools and real-time hooks.

## What It Does

- **Ingests session transcripts** — Parses Claude Code's JSONL transcript files into a structured SQLite database with full-text search
- **Provides MCP tools** — 11 tools for searching past sessions, saving context, logging bugs, and managing do-not-repeat rules
- **Real-time hooks** — Warns before re-reading files already read this session, and checks bugs/do-not-repeat rules before writes
- **Cross-session knowledge** — Decisions, bugs, and context persist so Claude doesn't re-learn the same things every session

## Architecture

```
┌─────────────────────────┐
│ Claude Code (sessions)  │
│ writes JSONL transcripts│
└───────────┬─────────────┘
            │
            ▼
┌───────────────────────┐     ┌──────────────────────┐
│ SessionStart/End Hook │────▶│  session-ingester     │
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

---

## Install

### Prerequisites

- Rust toolchain (1.70+): https://rustup.rs
- Claude Code CLI

### Build

```bash
cd D:/r/mnemosyne
cargo build --release
```

Binaries are output to `target/release/`:
- `session-ingester.exe`
- `memory-mcp-server.exe`
- `memory-hooks.exe`

### Register the MCP Server

Create or edit `~/.claude/.mcp.json`:

```json
{
  "mcpServers": {
    "mnemosyne": {
      "command": "D:/r/mnemosyne/target/release/memory-mcp-server.exe",
      "args": []
    }
  }
}
```

### Register the Hooks

Add to `~/.claude/settings.json` (merge with existing settings):

```json
{
  "hooks": {
    "SessionStart": [{
      "matcher": "",
      "hooks": [{
        "type": "command",
        "command": "D:/r/mnemosyne/target/release/session-ingester.exe"
      }]
    }],
    "SessionEnd": [{
      "matcher": "",
      "hooks": [{
        "type": "command",
        "command": "D:/r/mnemosyne/target/release/session-ingester.exe --from-stdin"
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

Adjust all paths to match your system. Use absolute paths with forward slashes on Windows (`D:/r/...` not `D:\r\...`).

### Seed the Database

Run the ingester once to ingest existing transcripts:

```bash
./target/release/session-ingester --verbose
```

After this, the SessionStart and SessionEnd hooks will keep the database up to date automatically.

### Verify

```bash
# Check the database
sqlite3 ~/.claude/memory/memory.db "SELECT count(*) FROM messages; SELECT count(*) FROM sessions;"

# Run the test suite
cargo test
```

---

## Usage Examples

### Automatic Behavior (via Hooks)

These happen automatically without any action from the user:

**Session start** — The ingester runs and processes any new transcripts from completed sessions. Other running agents immediately see data from sessions that have ended.

**Session end** — The ingester runs again with `--from-stdin`, immediately ingesting the just-finished session's transcript so other concurrent agents can access it.

**Reading a file** — The pre-read hook checks the file anatomy index and warns if the file was already read this session:
```
📄 pack.rs: Pack file parsing, delta resolution (~1,200 tokens)
⚠️ Already read at 2026-04-09 14:30:00 (1200 tokens). Consider if re-read is needed.
```

**Writing a file** — The pre-write hook checks for known bugs and do-not-repeat rules on the target file:
```
🐛 Known bug on pack.rs: delta cache Vec<u8> causes 43s pause — Fix: Use DeltaArena block allocator
🚫 Do not: use individual Vec<u8> for delta cache — Reason: Causes drop-time regression
```

### MCP Tools (On-Demand)

Claude can call these tools during any session. To encourage Claude to use them proactively, add guidance to your project's `CLAUDE.md`:

```markdown
## Memory (Mnemosyne)

This project uses Mnemosyne for persistent session memory. At the start of each session:
1. Call `get_project_summary` to load accumulated knowledge, known bugs, and do-not-repeat rules.

When working:
- Before exploring unfamiliar code, call `search_sessions` to check if it was discussed before.
- When you fix a bug, call `log_bug` with the error message, root cause, and fix description.
- When the user corrects your approach, call `add_do_not_repeat` to remember the lesson.
- When we make an architectural decision, call `save_context` with category "architecture".
- After investigating a file's history, call `get_file_history` to see past changes and context.
```

#### Bootstrapping a Session

```
Claude: calls get_project_summary("my_project")
  → Returns: 3 architecture decisions, 2 known bugs, 1 do-not-repeat rule, token stats

Claude: "From past sessions I see we're using arena allocators for delta cache
         and the tree-diff approach only walks changed subtrees. There's a known
         bug with merge commits having >2 parents. What would you like to work on?"
```

#### Searching Past Sessions

```
User: "Did we discuss tree diffing optimization?"
Claude: calls search_sessions("tree diffing optimization")
  → Returns: 2 sessions with matching excerpts and timestamps
```

#### Saving an Architectural Decision

```
Claude: calls save_context({
  content: "Chose rmcp crate over hand-rolled JSON-RPC for MCP server. 
            rmcp handles protocol negotiation, tool registration, and 
            stdio transport. The crate is young but sufficient for our needs.",
  category: "architecture",
  project: "mnemosyne"
})
```

#### Logging a Bug

```
Claude: calls log_bug({
  error_message: "snippet() fails with 'unable to use function in requested context'",
  fix_description: "Remove GROUP BY from FTS query. snippet() requires active 
                    MATCH context which GROUP BY breaks. Dedup by session_id in Rust instead.",
  root_cause: "FTS5 auxiliary functions lose their match context when GROUP BY is applied",
  tags: "sqlite,fts5",
  file_path: "memory-mcp-server/src/main.rs",
  project: "mnemosyne"
})
```

#### Adding a Do-Not-Repeat Rule

```
Claude: calls add_do_not_repeat({
  rule: "Don't use GROUP BY with FTS5 snippet() or highlight() functions",
  reason: "GROUP BY breaks the FTS match context. Use Rust-side dedup instead.",
  project: "mnemosyne",
  file_path: "memory-mcp-server/src/main.rs"
})
```

#### Checking File History

```
User: "What changes have been made to pack.rs?"
Claude: calls get_file_history({ file_path: "pack.rs" })
  → Returns: 5 tool calls (3 Edit, 2 Read) across 2 sessions with timestamps
```

#### Getting Session Details

```
Claude: calls get_session_detail({ session_id: "74810b85-..." })
  → Returns: project, timestamps, cwd, git branch, first/last user messages,
             tool call summary (Read: 12, Edit: 5, Bash: 3)
```

---

## MCP Tools Reference

| Tool | Type | Description |
|------|------|-------------|
| `search_sessions` | read | FTS5 search across all past messages, returns matching sessions with snippet excerpts |
| `get_recent_sessions` | read | List recent sessions by project (default: last 7 days) |
| `get_session_detail` | read | Full session metadata with first/last user messages and tool call summary |
| `get_file_history` | read | Tool calls that touched a specific file, with session context |
| `save_context` | write | Save a knowledge item with category (architecture, performance, conventions, etc.) |
| `search_context` | read | FTS5 search on saved context items |
| `get_project_summary` | read | All context items, recent bugs, do-not-repeat rules, and token stats for a project |
| `log_bug` | write | Log a bug with error message, fix description, optional root cause and tags |
| `search_bugs` | read | FTS5 search on logged bugs, with optional tag filtering |
| `add_do_not_repeat` | write | Add a rule for something to avoid, scoped to project and/or file |
| `get_do_not_repeat` | read | List do-not-repeat rules, filtered by project and/or file |

## Hook Behavior

All hooks are **advisory only** — they always exit 0 and never block tool execution.

| Hook | Event | What It Does |
|------|-------|-------------|
| `pre-read` | Before file read | Shows anatomy info (description + token estimate), warns on repeated reads |
| `post-read` | After file read | Records the read in session tracking, updates file anatomy stats |
| `pre-write` | Before file write/edit | Warns about known bugs and do-not-repeat rules matching the target file |
| `post-write` | After file write/edit | Updates file anatomy write count and modification time |

## Database

SQLite database is stored at `~/.claude/memory/memory.db` with WAL mode enabled for concurrent access from multiple agents.

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

## Run Tests

```bash
cargo test          # 54 tests across all 4 crates
cargo test -- -q    # quiet output, just pass/fail
```
