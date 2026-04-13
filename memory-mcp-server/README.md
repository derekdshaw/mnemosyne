# memory-mcp-server

MCP (Model Context Protocol) server that gives Claude Code queryable access to the Mnemosyne session memory database.

## Purpose

This server runs over stdio and exposes 13 tools that Claude can call during a session to search past conversations, save project context, log bugs, manage do-not-repeat rules, and view analytics. It connects to the same SQLite database populated by `session-ingester` and `memory-hooks`.

## Architecture

```
main.rs
├── MnemosyneServer struct
│   ├── Mutex<Connection>  — SQLite connection (thread-safe)
│   └── ToolRouter<Self>   — rmcp tool dispatch
├── #[tool_router] impl    — 13 tool methods with SQL queries
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
| `save_context` | content, category, project? | Save a knowledge item (architecture, conventions, etc.) |
| `search_context` | query, category?, project?, limit? | FTS5 search on saved context |
| `get_project_summary` | project? | All context + bugs + do-not-repeat + token stats |
| `log_bug` | error_message, fix_description, root_cause?, tags?, file_path?, project? | Record a bug fix |
| `search_bugs` | query, tags?, project? | FTS5 search on bug records |
| `add_do_not_repeat` | rule, reason?, project?, file_path? | Add a do-not-repeat rule |
| `get_do_not_repeat` | project?, file_path? | List active do-not-repeat rules |

**Analytics:**

| Tool | Input | Description |
|------|-------|-------------|
| `get_token_stats` | project?, days? | Token usage, cache stats, savings estimates, top sessions |
| `get_analytics` | project?, days? | Comprehensive report: usage, productivity, savings, memory health |

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
cargo test -p memory-mcp-server   # 22 tests covering all 13 tools + helper functions
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
