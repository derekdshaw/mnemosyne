#!/usr/bin/env bash
set -euo pipefail

# Mnemosyne installer for macOS and Linux
# Installs binaries, configures Claude Code hooks and MCP server

BINARIES=("session-ingester" "memory-mcp-server" "memory-hooks")
CLAUDE_DIR="$HOME/.claude"
SETTINGS_FILE="$CLAUDE_DIR/settings.json"
MCP_FILE="$CLAUDE_DIR/.mcp.json"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RESET='\033[0m'

info()  { printf "${CYAN}${BOLD}==> ${RESET}${BOLD}%s${RESET}\n" "$*"; }
ok()    { printf "${GREEN}${BOLD} OK ${RESET}%s\n" "$*"; }
warn()  { printf "${YELLOW}${BOLD}WARN${RESET} %s\n" "$*"; }
err()   { printf "${RED}${BOLD}ERR ${RESET}%s\n" "$*"; exit 1; }

# --- Locate binaries ---

find_binaries() {
    local search_dir="$1"
    local found=0
    for bin in "${BINARIES[@]}"; do
        if [ -f "$search_dir/$bin" ] && [ -x "$search_dir/$bin" ]; then
            found=$((found + 1))
        fi
    done
    [ "$found" -eq "${#BINARIES[@]}" ]
}

info "Mnemosyne installer"
echo ""

BIN_SOURCE=""

# Check current directory
if find_binaries "."; then
    BIN_SOURCE="."
# Check ./target/release
elif find_binaries "./target/release"; then
    BIN_SOURCE="./target/release"
else
    echo "Could not find mnemosyne binaries in the current directory."
    echo "Expected: ${BINARIES[*]}"
    echo ""
    read -rp "Enter the path to the directory containing the binaries: " user_path
    user_path="${user_path/#\~/$HOME}"
    if find_binaries "$user_path"; then
        BIN_SOURCE="$user_path"
    else
        err "Binaries not found in '$user_path'. Build first with: cargo build --release"
    fi
fi

ok "Found binaries in $BIN_SOURCE"

# --- Determine install location ---

if [ "$(uname)" = "Darwin" ]; then
    INSTALL_DIR="/usr/local/bin"
    if [ ! -w "$INSTALL_DIR" ]; then
        INSTALL_DIR="$HOME/.local/bin"
    fi
else
    INSTALL_DIR="$HOME/.local/bin"
fi

echo ""
read -rp "Install binaries to [$INSTALL_DIR]: " custom_dir
if [ -n "$custom_dir" ]; then
    INSTALL_DIR="${custom_dir/#\~/$HOME}"
fi

mkdir -p "$INSTALL_DIR"

# --- Copy binaries ---

info "Installing binaries to $INSTALL_DIR"

for bin in "${BINARIES[@]}"; do
    cp "$BIN_SOURCE/$bin" "$INSTALL_DIR/$bin"
    chmod +x "$INSTALL_DIR/$bin"
    ok "$bin"
done

# Check if install dir is on PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
    warn "$INSTALL_DIR is not on your PATH"
    echo "  Add to your shell profile:"
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
    echo ""
fi

# --- Build absolute paths for config ---

INGESTER="$INSTALL_DIR/session-ingester"
MCP_SERVER="$INSTALL_DIR/memory-mcp-server"
HOOKS="$INSTALL_DIR/memory-hooks"

# --- Configure Claude Code settings.json ---

info "Configuring Claude Code hooks"

mkdir -p "$CLAUDE_DIR"

# Use python to merge JSON (available on macOS and most Linux)
if ! command -v python3 &>/dev/null && ! command -v python &>/dev/null; then
    err "python3 or python is required for JSON merging. Install Python and retry."
fi

PYTHON=$(command -v python3 2>/dev/null || command -v python 2>/dev/null)

$PYTHON - "$SETTINGS_FILE" "$INGESTER" "$HOOKS" <<'PYEOF'
import json, sys, os

settings_path = sys.argv[1]
ingester = sys.argv[2]
hooks = sys.argv[3]

settings = {}
if os.path.exists(settings_path):
    with open(settings_path) as f:
        try:
            settings = json.load(f)
        except json.JSONDecodeError:
            settings = {}

hooks_config = {
    "SessionStart": [{
        "matcher": "",
        "hooks": [{"type": "command", "command": ingester}]
    }],
    "SessionEnd": [{
        "matcher": "",
        "hooks": [{"type": "command", "command": f"{ingester} --from-stdin"}]
    }],
    "PreToolUse": [
        {
            "matcher": "Read",
            "hooks": [{"type": "command", "command": f"{hooks} pre-read"}]
        },
        {
            "matcher": "Write|Edit",
            "hooks": [{"type": "command", "command": f"{hooks} pre-write"}]
        }
    ],
    "PostToolUse": [
        {
            "matcher": "Read",
            "hooks": [{"type": "command", "command": f"{hooks} post-read"}]
        },
        {
            "matcher": "Write|Edit",
            "hooks": [{"type": "command", "command": f"{hooks} post-write"}]
        }
    ]
}

# Merge: overwrite mnemosyne hook entries, preserve other hooks
existing_hooks = settings.get("hooks", {})
for event, entries in hooks_config.items():
    # Remove any existing mnemosyne entries (match by command containing our binaries)
    if event in existing_hooks:
        existing = existing_hooks[event]
        filtered = [e for e in existing if not any(
            ingester in h.get("command", "") or hooks in h.get("command", "")
            for h in e.get("hooks", [])
        )]
        # Also filter entries referencing old mnemosyne paths
        filtered = [e for e in filtered if not any(
            "session-ingester" in h.get("command", "") or "memory-hooks" in h.get("command", "")
            for h in e.get("hooks", [])
        )]
        existing_hooks[event] = filtered + entries
    else:
        existing_hooks[event] = entries

settings["hooks"] = existing_hooks

with open(settings_path, "w") as f:
    json.dump(settings, f, indent=2)
    f.write("\n")
PYEOF

ok "Updated $SETTINGS_FILE"

# --- Configure MCP server ---

info "Configuring MCP server"

$PYTHON - "$MCP_FILE" "$MCP_SERVER" <<'PYEOF'
import json, sys, os

mcp_path = sys.argv[1]
server = sys.argv[2]

mcp = {}
if os.path.exists(mcp_path):
    with open(mcp_path) as f:
        try:
            mcp = json.load(f)
        except json.JSONDecodeError:
            mcp = {}

if "mcpServers" not in mcp:
    mcp["mcpServers"] = {}

mcp["mcpServers"]["mnemosyne"] = {
    "command": server,
    "args": []
}

with open(mcp_path, "w") as f:
    json.dump(mcp, f, indent=2)
    f.write("\n")
PYEOF

ok "Updated $MCP_FILE"

# --- Seed database ---

info "Running initial transcript ingestion"

"$INGESTER" --verbose 2>&1 | tail -5 || warn "Ingestion had issues (non-fatal)"

ok "Database seeded"

# --- Done ---

echo ""
printf "${GREEN}${BOLD}Mnemosyne installed successfully!${RESET}\n"
echo ""
echo "Binaries:  $INSTALL_DIR"
echo "Settings:  $SETTINGS_FILE"
echo "MCP:       $MCP_FILE"
echo "Database:  ~/.claude/memory/memory.db"
echo ""
printf "${CYAN}${BOLD}Next step:${RESET} Add memory guidance to your project's CLAUDE.md.\n"
echo "See the usage guide for CLAUDE.md examples and detailed tool usage:"
echo "  https://github.com/derekdshaw/mnemosyne/blob/main/docs/USAGE.md"
echo ""
echo "Minimal CLAUDE.md snippet:"
echo ""
cat <<'SNIPPET'
  ## Memory (Mnemosyne)

  This project uses Mnemosyne for persistent session memory. At the start of each session:
  1. Call `get_project_summary` to load accumulated knowledge, known bugs, and do-not-repeat rules.

  When working:
  - Before exploring unfamiliar code, call `search_sessions` to check if it was discussed before.
  - When you fix a bug, call `log_bug` with the error message, root cause, and fix description.
  - When the user corrects your approach, call `add_do_not_repeat` to remember the lesson.
  - When we make an architectural decision, call `save_context` with category "architecture".
  - After investigating a file's history, call `get_file_history` to see past changes and context.
SNIPPET
