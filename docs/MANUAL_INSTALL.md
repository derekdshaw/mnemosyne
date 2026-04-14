# Manual Installation

If you prefer not to use the install scripts, you can configure Mnemosyne manually.

## Prerequisites

- [Rust toolchain](https://rustup.rs) (1.70+) — only if building from source
- [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code)

## Build

```bash
cd /path/to/mnemosyne
cargo build --release
```

Binaries are output to `target/release/`:
- `session-ingester`
- `memory-mcp-server`
- `memory-hooks`

Copy these to a directory on your PATH (e.g. `~/.local/bin/`, `/usr/local/bin/`, or `%LOCALAPPDATA%\Programs\mnemosyne\` on Windows).

## Register the MCP Server

Register mnemosyne as a **user-level** MCP server so it's available in all projects:

```bash
claude mcp add --scope user --transport stdio mnemosyne /absolute/path/to/memory-mcp-server
```

This stores the server configuration in `~/.claude.json`. You can verify it was added:

```bash
claude mcp list
```

### Per-Project Setup (Optional)

If you prefer to enable mnemosyne only for a specific project instead of globally, create a `.mcp.json` file in the project root:

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

Claude Code will prompt you to approve the server the first time it's used. To auto-approve it, add to `~/.claude/settings.json`:

```json
{
  "enabledMcpjsonServers": ["mnemosyne"]
}
```

> **Note:** The install script uses user-level registration by default. Use the per-project approach only if you need mnemosyne in some projects but not others.

## Register the Hooks

Add to `~/.claude/settings.json` (merge with existing settings):

```json
{
  "hooks": {
    "SessionStart": [{
      "matcher": "",
      "hooks": [{
        "type": "command",
        "command": "/absolute/path/to/session-ingester"
      }]
    }],
    "SessionEnd": [{
      "matcher": "",
      "hooks": [{
        "type": "command",
        "command": "/absolute/path/to/session-ingester --from-stdin"
      }]
    }],
    "PreToolUse": [
      {
        "matcher": "Read",
        "hooks": [{
          "type": "command",
          "command": "/absolute/path/to/memory-hooks pre-read"
        }]
      },
      {
        "matcher": "Write|Edit",
        "hooks": [{
          "type": "command",
          "command": "/absolute/path/to/memory-hooks pre-write"
        }]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Read",
        "hooks": [{
          "type": "command",
          "command": "/absolute/path/to/memory-hooks post-read"
        }]
      },
      {
        "matcher": "Write|Edit",
        "hooks": [{
          "type": "command",
          "command": "/absolute/path/to/memory-hooks post-write"
        }]
      }
    ]
  }
}
```

Replace `/absolute/path/to/` with the actual path to your binaries. Paths must be absolute. On Windows, use forward slashes (`C:/Users/me/...`).

## Seed the Database

Run the ingester once to process existing transcripts:

```bash
session-ingester --verbose
```

After this, the SessionStart and SessionEnd hooks keep the database up to date automatically.

## Verify

```bash
# Run the test suite
cargo test

# Inspect the database (optional, requires sqlite3 CLI)
sqlite3 ~/.claude/memory/memory.db "SELECT count(*) FROM messages; SELECT count(*) FROM sessions;"
```
