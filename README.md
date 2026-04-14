# Mnemosyne

A session memory system for Claude Code. Named after the Greek goddess of memory.

Mnemosyne gives Claude Code persistent, queryable memory across sessions by ingesting existing JSONL transcripts into SQLite and exposing them through MCP tools and real-time hooks.

## What It Does

- **Ingests session transcripts** — Parses Claude Code's JSONL transcript files into a structured SQLite database with full-text search
- **Provides MCP tools** — 13 tools for searching past sessions, saving context, logging bugs, and managing do-not-repeat rules
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
│ SessionStart/End Hook │────>│  session-ingester     │
└───────────────────────┘     │  Parses JSONL -> SQLite│
                              └──────────┬────────────┘
                                         │
┌───────────────────────┐                ▼
│ PreToolUse/PostToolUse│     ┌──────────────────────┐
│ Hooks (Read/Write)    │<──>│  SQLite (WAL mode)    │
└───────────────────────┘     │  ~/.claude/memory/    │
  memory-hooks binary         │    memory.db          │
                              └──────────┬────────────┘
                                         ^
                              ┌──────────┴────────────┐
                              │  memory-mcp-server     │
                              │  13 MCP tools over     │
                              │  stdio transport       │
                              └────────────────────────┘
```

## Workspace Crates

| Crate | Binary | Purpose |
|-------|--------|---------|
| [memory-common](memory-common/) | (library) | Shared SQLite schema, JSONL parser, data models, path utilities |
| [session-ingester](session-ingester/) | `session-ingester` | CLI that scans and ingests JSONL transcripts into SQLite |
| [memory-mcp-server](memory-mcp-server/) | `memory-mcp-server` | MCP server exposing 13 query/write tools over stdio |
| [memory-hooks](memory-hooks/) | `memory-hooks` | Real-time hook handlers for pre/post read/write events |

---

## Install

### Option 1: Download Prebuilt Binaries

Grab the latest release for your platform from [GitHub Releases](https://github.com/derekdshaw/mnemosyne/releases). Available archives:

| Platform | Archive |
|----------|---------|
| Linux x86_64 | `mnemosyne-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `mnemosyne-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Intel | `mnemosyne-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `mnemosyne-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `mnemosyne-vX.Y.Z-x86_64-pc-windows-msvc.zip` |

Extract the archive, then run the install script from the extracted directory.

### Option 2: Build from Source

Requires [Rust 1.70+](https://rustup.rs).

```bash
git clone https://github.com/derekdshaw/mnemosyne.git
cd mnemosyne
cargo build --release
```

### Run the Install Script

The install script copies binaries to a standard location, registers the MCP server, configures Claude Code hooks, and seeds the database.

**macOS / Linux:**

```bash
./scripts/install.sh
```

**Windows (PowerShell):**

```powershell
.\scripts\install.ps1
```

For manual configuration without the install script, see [Manual Installation](docs/MANUAL_INSTALL.md).

---

## Configure CLAUDE.md

Add this to your project's `CLAUDE.md` so Claude uses memory tools proactively:

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

See the [Usage Guide](docs/USAGE.md) for a comprehensive CLAUDE.md example and detailed tool usage.

## Run Tests

```bash
cargo test          # 71 tests across all 4 crates
```

## Documentation

| Topic | Location |
|-------|----------|
| Usage examples and CLAUDE.md setup | [docs/USAGE.md](docs/USAGE.md) |
| Manual installation | [docs/MANUAL_INSTALL.md](docs/MANUAL_INSTALL.md) |
| MCP tools (13 tools) | [memory-mcp-server/README.md](memory-mcp-server/README.md) |
| Hook behavior | [memory-hooks/README.md](memory-hooks/README.md) |
| Database schema | [memory-common/README.md](memory-common/README.md#database-schema) |

## License

[Common Clause + MIT](LICENSE)
