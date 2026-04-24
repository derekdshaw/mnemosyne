# memory-mcp-server

MCP (Model Context Protocol) server that gives Claude Code queryable access to the Mnemosyne session memory database.

## Purpose

This server runs over stdio and exposes 12 tools that Claude can call during a session to search past conversations, save project context, log bugs, manage do-not-repeat rules, and view analytics. It connects to the same SQLite database populated by `session-ingester` and `memory-hooks`.

## Architecture

```
main.rs
├── MnemosyneServer struct
│   ├── Mutex<Connection>  — SQLite connection (thread-safe)
│   └── ToolRouter<Self>   — rmcp tool dispatch
├── #[tool_router] impl    — 12 tool methods with SQL queries
├── #[tool_handler] impl   — ServerHandler trait (initialize, list_tools, call_tool)
└── main()                 — Opens DB, starts stdio transport via rmcp

tools.rs
├── Input structs           — Deserialize + JsonSchema for each tool's parameters
└── Output structs          — Serialize + JsonSchema for each tool's return value
```

### Tools

**Read-only (query ingested data):**

| Tool | Input | Description |
|------|-------|-------------|
| `search_sessions` | query, limit?, project? | FTS5 search across messages with snippet excerpts |
| `get_recent_sessions` | days?, project? | List sessions from the last N days |
| `get_session_detail` | session_id | Full metadata + first/last messages + tool call summary |
| `get_file_history` | file_path?, project?, days? | Tool calls that touched a file |

**Read-write (context management):**

| Tool | Input | Description |
|------|-------|-------------|
| `save_context` | content, category, project?, compress? | Save a knowledge item (architecture, conventions, etc.). Set compress=true and write content in caveman format for token savings. |
| `search_context` | query, category?, project?, limit? | FTS5 search on saved context |
| `get_project_summary` | project? | All context + bugs + do-not-repeat + token stats |
| `log_bug` | error_message, fix_description, root_cause?, tags?, file_path?, project?, compress? | Record a bug fix. Set compress=true and write fix_description/root_cause in caveman format. |
| `search_bugs` | query, tags?, project? | FTS5 search on bug records |
| `add_do_not_repeat` | rule, reason?, project?, file_path? | Add a do-not-repeat rule |
| `get_do_not_repeat` | project?, file_path? | List active do-not-repeat rules |

**Analytics:**

| Tool | Input | Description |
|------|-------|-------------|
| `get_analytics` | project?, days?, section? | Usage + tokens + savings + overhead (always), plus productivity and memory-health when `section` is `"full"` or omitted. Pass `section: "tokens"` for a cheaper tokens-only response. |

The `get_analytics` response always includes:
- **Usage:** `total_sessions`, `total_input_tokens`, `total_output_tokens`, `total_cache_read_tokens`, `total_cache_creation_tokens`, `avg_input_per_session`, `avg_output_per_session`
- **Savings:** `files_with_anatomy`, `total_file_reads`, `repeated_reads_detected`, `estimated_tokens_saveable` — the "tokens we skipped re-reading because anatomy was cached"
- **Overhead:** `overhead_tokens` (total tokens mnemosyne's own hooks added to Claude's context in the window), `overhead_by_hook` (per-hook breakdown with `invocations`, `estimated_tokens`, `avg_tokens`, `min_tokens`, `max_tokens`, `stddev_tokens`), and `net_savings_tokens = saveable - overhead`
- **Top consumers:** `top_sessions_by_tokens` (5 biggest sessions by total tokens)

When `section` is `"full"` (or omitted), the response additionally includes productivity data (`tool_call_breakdown`, `top_read_files`, `top_written_files`, `bug_count`, `bugs_by_file`) and memory health (`context_items_by_category`, `total_do_not_repeat_rules`, `total_bugs_logged`, `oldest_context_item`, `projects_with_context`, `projects_without_context`).

Use `section: "tokens"` when you only need token accounting — it skips the heavier joins on `tool_calls`, `file_anatomy`, `bugs`, `context_items`, `do_not_repeat`.

### MCP Protocol

Uses the [rmcp](https://crates.io/crates/rmcp) crate (v1.3) with newline-delimited JSON-RPC over stdio. Claude Code manages the server lifecycle — starting it when a session begins and stopping it when the session ends.

## Build

```bash
cargo build -p memory-mcp-server
cargo build -p memory-mcp-server --release
```

## Test

```bash
# Test with MCP inspector (requires npm)
npx @modelcontextprotocol/inspector ./target/debug/memory-mcp-server

# Manual test: send initialize + tools/list
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' | ./target/release/memory-mcp-server
```

## Test

```bash
cargo test -p memory-mcp-server   # 34 tests covering all 12 tools + helpers + regression guards
```

## Configuration

Register in `~/.claude/.mcp.json` (not settings.json — MCP servers have their own config file):

```json
{
  "mcpServers": {
    "mnemosyne": {
      "command": "/absolute/path/to/memory-mcp-server",
      "args": []
    }
  }
}
```
