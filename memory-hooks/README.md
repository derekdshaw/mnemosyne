# memory-hooks

Single Rust binary with subcommands for Claude Code's `SessionStart`, `PreToolUse`, and `PostToolUse` hooks. Provides a session briefing at startup, advisory warnings, and session-level file tracking.

## Purpose

Claude Code spawns a new process for each hook event. This binary handles all five hook types via subcommands, keeping it to one compiled binary for fast startup. All hooks are **advisory only** — they always exit 0 and never block tool execution.

## Architecture

```
main.rs
├── CLI parsing (clap subcommands)
├── Reads hook JSON from stdin
├── Opens SQLite DB (shared with MCP server and ingester)
├── Dispatches to subcommand handler
├── Writes briefing to stdout (session-start, injected into conversation)
└── Writes warnings to stderr (pre/post hooks, visible to Claude)

session_start.rs — Print project briefing (do-not-repeat rules, context, bugs) to stdout
pre_read.rs      — Anatomy lookup + repeated-read detection
post_read.rs     — Record read in session_reads, update/create anatomy entry
pre_write.rs     — Query bugs + do-not-repeat rules for the target file
post_write.rs    — Update anatomy write count and modification time
```

### Hook Input Format

Each hook receives JSON on stdin from Claude Code. The format depends on the event type:

**SessionStart** input:
```json
{
  "session_id": "uuid-string",
  "cwd": "/home/user/my_project",
  "hook_event_name": "SessionStart",
  "source": "startup"
}
```

**PreToolUse / PostToolUse** input:
```json
{
  "session_id": "uuid-string",
  "cwd": "/home/user/my_project",
  "tool_name": "Read",
  "tool_input": { "file_path": "/home/user/my_project/src/main.rs" },
  "tool_response": { "content": "..." }
}
```

`tool_response` is only present for `PostToolUse` hooks.

### Subcommand Behavior

**`session-start`** — Runs at the beginning of each session:
1. Derives project name from `cwd`
2. Queries `do_not_repeat` rules (global + project-scoped) and prints them
3. Queries `context_items` (architecture decisions, conventions, etc.) and prints them
4. Queries recent `bugs` and prints them
5. Prints session stats (total sessions, token usage)

Output goes to **stdout**, which Claude Code injects into the conversation automatically. This replaces the need for Claude to manually call `get_project_summary` at session start.

**`pre-read`** — Runs before every file read:
1. Looks up the file in `file_anatomy` → prints content-aware description and token estimate to stderr
2. Checks `session_reads` for the current session → warns if file was already read

**`post-read`** — Runs after every file read:
1. Extracts a content-aware description from the file (doc comments, public signatures, exports) via `memory_common::anatomy::extract_description()`
2. Estimates tokens from response content (chars / 3.5)
3. Inserts into `session_reads` (session tracking)
4. Upserts `file_anatomy` with the description, token estimate, and read count. Description is refreshed on every read so it stays current as files change.

Supported languages for anatomy extraction: Rust, Python, TypeScript/JavaScript, Java, Go, Markdown, TOML, JSON, YAML (with a first-line fallback for other types).

**`pre-write`** — Runs before every file write/edit:
1. Queries `bugs` table for the target file → warns about known bugs
2. Queries `do_not_repeat` table for matching project/file rules → warns about things to avoid

**`post-write`** — Runs after every file write/edit:
1. Increments `file_anatomy.times_written` and updates `last_modified`
2. Creates anatomy entry if file is new

### Error Handling

All errors are caught and printed to stderr. The process always exits 0 to avoid blocking Claude Code. If the database can't be opened (e.g., first run before ingester creates it), the hook exits silently.

## Build

```bash
cargo build -p memory-hooks
cargo build -p memory-hooks --release
```

## Test

```bash
# Test session-start (prints project briefing to stdout)
echo '{"session_id":"test","cwd":"/home/user/myproject"}' | cargo run -p memory-hooks -- session-start

# Test pre-read (should show repeated-read warning if file was read before)
echo '{"session_id":"test","cwd":"/home/user/myproject","tool_name":"Read","tool_input":{"file_path":"/home/user/myproject/src/main.rs"}}' | cargo run -p memory-hooks -- pre-read

# Test post-read (records the read)
echo '{"session_id":"test","cwd":"/home/user/myproject","tool_name":"Read","tool_input":{"file_path":"/home/user/myproject/src/main.rs"}}' | cargo run -p memory-hooks -- post-read

# Test pre-write (shows bugs and do-not-repeat warnings)
echo '{"session_id":"test","cwd":"/home/user/myproject","tool_name":"Edit","tool_input":{"file_path":"/home/user/myproject/src/main.rs"}}' | cargo run -p memory-hooks -- pre-write

# Test post-write (updates anatomy)
echo '{"session_id":"test","cwd":"/home/user/myproject","tool_name":"Edit","tool_input":{"file_path":"/home/user/myproject/src/main.rs"}}' | cargo run -p memory-hooks -- post-write
```

## Configuration

Register in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "/absolute/path/to/memory-hooks session-start" }] }
    ],
    "PreToolUse": [
      { "matcher": "Read", "hooks": [{ "type": "command", "command": "/absolute/path/to/memory-hooks pre-read" }] },
      { "matcher": "Write|Edit", "hooks": [{ "type": "command", "command": "/absolute/path/to/memory-hooks pre-write" }] }
    ],
    "PostToolUse": [
      { "matcher": "Read", "hooks": [{ "type": "command", "command": "/absolute/path/to/memory-hooks post-read" }] },
      { "matcher": "Write|Edit", "hooks": [{ "type": "command", "command": "/absolute/path/to/memory-hooks post-write" }] }
    ]
  }
}
```

Note: The `session-start` hook outputs to **stdout**, which Claude Code injects into the conversation. The pre/post hooks output to **stderr**, which appears as advisory messages.

### Performance

Hooks run in the hot path of every file read/write. Per-hook overhead is typically 50-100ms (process spawn + SQLite open/query/close). See the plan document for pre-designed optimization paths (named-pipe daemon, MCP server reuse) if this becomes a bottleneck.
