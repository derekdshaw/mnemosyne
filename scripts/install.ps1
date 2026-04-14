#Requires -Version 5.1
# Mnemosyne installer for Windows
# Installs binaries, configures Claude Code hooks and MCP server

$ErrorActionPreference = "Stop"

$Binaries = @("session-ingester.exe", "memory-mcp-server.exe", "memory-hooks.exe")
$ClaudeDir = "$env:USERPROFILE\.claude"
$SettingsFile = "$ClaudeDir\settings.json"

function Write-Info($msg)  { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)    { Write-Host " OK $msg" -ForegroundColor Green }
function Write-Warn($msg)  { Write-Host "WARN $msg" -ForegroundColor Yellow }
function Write-Err($msg)   { Write-Host "ERR $msg" -ForegroundColor Red; exit 1 }

# --- Locate binaries ---

function Test-BinariesExist($dir) {
    $found = 0
    foreach ($bin in $Binaries) {
        if (Test-Path (Join-Path $dir $bin)) { $found++ }
    }
    return $found -eq $Binaries.Count
}

Write-Info "Mnemosyne installer"
Write-Host ""

$BinSource = $null

if (Test-BinariesExist ".") {
    $BinSource = (Resolve-Path ".").Path
}
elseif (Test-BinariesExist ".\target\release") {
    $BinSource = (Resolve-Path ".\target\release").Path
}
else {
    Write-Host "Could not find mnemosyne binaries in the current directory."
    Write-Host "Expected: $($Binaries -join ', ')"
    Write-Host ""
    $userPath = Read-Host "Enter the path to the directory containing the binaries"
    if (Test-BinariesExist $userPath) {
        $BinSource = (Resolve-Path $userPath).Path
    }
    else {
        Write-Err "Binaries not found in '$userPath'. Build first with: cargo build --release"
    }
}

Write-Ok "Found binaries in $BinSource"

# --- Determine install location ---

$DefaultDir = "$env:LOCALAPPDATA\Programs\mnemosyne"
Write-Host ""
$customDir = Read-Host "Install binaries to [$DefaultDir]"
if ($customDir) {
    $InstallDir = $customDir
}
else {
    $InstallDir = $DefaultDir
}

if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# --- Copy binaries ---

Write-Info "Installing binaries to $InstallDir"

foreach ($bin in $Binaries) {
    Copy-Item (Join-Path $BinSource $bin) (Join-Path $InstallDir $bin) -Force
    Write-Ok $bin
}

# Check if install dir is on PATH
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$InstallDir*") {
    Write-Warn "$InstallDir is not on your PATH"
    $addToPath = Read-Host "Add it to your user PATH? [Y/n]"
    if ($addToPath -ne "n" -and $addToPath -ne "N") {
        [Environment]::SetEnvironmentVariable(
            "Path",
            "$userPath;$InstallDir",
            "User"
        )
        $env:Path = "$env:Path;$InstallDir"
        Write-Ok "Added to user PATH (restart your terminal for it to take effect)"
    }
}

# --- Build paths for config (use forward slashes for Claude Code) ---

$Ingester  = (Join-Path $InstallDir "session-ingester.exe") -replace '\\', '/'
$McpServer = (Join-Path $InstallDir "memory-mcp-server.exe") -replace '\\', '/'
$Hooks     = (Join-Path $InstallDir "memory-hooks.exe") -replace '\\', '/'

# --- Configure Claude Code settings.json ---

Write-Info "Configuring Claude Code hooks"

if (-not (Test-Path $ClaudeDir)) {
    New-Item -ItemType Directory -Path $ClaudeDir -Force | Out-Null
}

$settings = @{}
if (Test-Path $SettingsFile) {
    try {
        $settings = Get-Content $SettingsFile -Raw | ConvertFrom-Json -AsHashtable
    }
    catch {
        $settings = @{}
    }
}

# Build hooks config
$hooksConfig = @{
    "SessionStart" = @(
        @{
            matcher = ""
            hooks = @(@{ type = "command"; command = $Ingester })
        },
        @{
            matcher = ""
            hooks = @(@{ type = "command"; command = "$Hooks session-start" })
        }
    )
    "SessionEnd" = @(
        @{
            matcher = ""
            hooks = @(@{ type = "command"; command = "$Ingester --from-stdin" })
        }
    )
    "PreToolUse" = @(
        @{
            matcher = "Read"
            hooks = @(@{ type = "command"; command = "$Hooks pre-read" })
        },
        @{
            matcher = "Write|Edit"
            hooks = @(@{ type = "command"; command = "$Hooks pre-write" })
        }
    )
    "PostToolUse" = @(
        @{
            matcher = "Read"
            hooks = @(@{ type = "command"; command = "$Hooks post-read" })
        },
        @{
            matcher = "Write|Edit"
            hooks = @(@{ type = "command"; command = "$Hooks post-write" })
        }
    )
}

# Merge: remove old mnemosyne entries, add new ones
if (-not $settings.ContainsKey("hooks")) {
    $settings["hooks"] = @{}
}

foreach ($event in $hooksConfig.Keys) {
    $newEntries = $hooksConfig[$event]
    if ($settings["hooks"].ContainsKey($event)) {
        # Filter out old mnemosyne entries
        $existing = $settings["hooks"][$event] | Where-Object {
            $dominated = $false
            foreach ($h in $_.hooks) {
                $cmd = $h.command
                if ($cmd -match "session-ingester" -or $cmd -match "memory-hooks") {
                    $dominated = $true
                }
            }
            -not $dominated
        }
        if ($null -eq $existing) { $existing = @() }
        if ($existing -isnot [array]) { $existing = @($existing) }
        $settings["hooks"][$event] = @($existing) + $newEntries
    }
    else {
        $settings["hooks"][$event] = $newEntries
    }
}

$settings | ConvertTo-Json -Depth 10 | Set-Content $SettingsFile -Encoding UTF8
Write-Ok "Updated $SettingsFile"

# --- Register MCP server (user-level, available in all projects) ---

Write-Info "Registering MCP server"

$claudeCmd = Get-Command claude -ErrorAction SilentlyContinue
if (-not $claudeCmd) {
    Write-Err "Claude Code CLI ('claude') not found on PATH. Install it first: https://docs.anthropic.com/en/docs/claude-code"
}

& claude mcp add --scope user --transport stdio mnemosyne $McpServer
Write-Ok "Registered mnemosyne MCP server (user-level)"

# --- Seed database ---

Write-Info "Running initial transcript ingestion"

$ingesterExe = Join-Path $InstallDir "session-ingester.exe"
try {
    & $ingesterExe --verbose 2>&1 | Select-Object -Last 5
    Write-Ok "Database seeded"
}
catch {
    Write-Warn "Ingestion had issues (non-fatal): $_"
}

# --- Done ---

Write-Host ""
Write-Host "Mnemosyne installed successfully!" -ForegroundColor Green
Write-Host ""
Write-Host "Binaries:  $InstallDir"
Write-Host "Settings:  $SettingsFile"
Write-Host "MCP:       ~/.claude.json (user-level)"
Write-Host "Database:  ~/.claude/memory/memory.db"
Write-Host ""
Write-Host "Next step: " -ForegroundColor Cyan -NoNewline
Write-Host "Add memory guidance to your project's CLAUDE.md."
Write-Host "See the usage guide for CLAUDE.md examples and detailed tool usage:"
Write-Host "  https://github.com/derekdshaw/mnemosyne/blob/main/docs/USAGE.md"
Write-Host ""
Write-Host "Minimal CLAUDE.md snippet:"
Write-Host ""
Write-Host @"
  ## Memory (Mnemosyne)

  This project uses Mnemosyne for persistent session memory. A session briefing
  (do-not-repeat rules, saved context, recent bugs) is automatically injected
  at startup via the SessionStart hook — no manual tool call needed.

  When working:
  - Before exploring unfamiliar code, call ``search_sessions`` to check if it was discussed before.
  - When you fix a bug, call ``log_bug`` with the error message, root cause, and fix description.
  - When the user corrects your approach, call ``add_do_not_repeat`` to remember the lesson.
  - When we make an architectural decision, call ``save_context`` with category "architecture".
  - After investigating a file's history, call ``get_file_history`` to see past changes and context.
"@
