# Usage Guide

## Automatic Behavior (via Hooks)

These happen automatically without any action from the user:

**Session start** — The ingester runs and processes any new transcripts from completed sessions. This populates the `sessions`, `messages`, `tool_calls`, and `token_usage` tables — making past conversations, tool invocations (with file paths), and token stats queryable by tools like `search_sessions`, `get_file_history`, and `get_session_detail`.

**Session end** — The ingester runs again with `--from-stdin`, immediately ingesting the just-finished session's transcript so other concurrent agents can access it without waiting for a new session to start.

**Reading a file** — After a read, the post-read hook extracts a content-aware description from the file (doc comments, public signatures, exports) and stores it in the `file_anatomy` table. On subsequent reads, the pre-read hook shows this summary and warns if the file was already read this session:
```
pack.rs: Pack file parsing and delta resolution. Exports: pub fn parse_pack, pub struct DeltaArena (~1,200 tokens)
Already read at 2026-04-09 14:30:00 (1200 tokens). Consider if re-read is needed.
```

Anatomy extraction supports Rust, Python, TypeScript/JavaScript, Java, Go, Markdown, TOML, JSON, and YAML. Descriptions are refreshed on every read so they stay current as files change.

**Writing a file** — The pre-write hook checks for known bugs and do-not-repeat rules on the target file:
```
Known bug on pack.rs: delta cache Vec<u8> causes 43s pause — Fix: Use DeltaArena block allocator
Do not: use individual Vec<u8> for delta cache — Reason: Causes drop-time regression
```

## Configuring CLAUDE.md

Add guidance to your project's `CLAUDE.md` so Claude uses memory tools proactively.

### Minimal Example

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

### Comprehensive Example

```markdown
## Session Memory (Mnemosyne)

### Session Start
Always begin by calling `get_project_summary` to load context from prior sessions.
Review the returned bugs, do-not-repeat rules, and architecture decisions before
starting any work. If the summary mentions relevant prior sessions, call
`get_session_detail` to understand what was done and why.

### During Development
- **Before reading a file**: Check the pre-read hook output. If the anatomy description
  tells you enough about the file's contents, you may not need to read it.
- **Before exploring new areas**: Call `search_sessions` with relevant keywords. Another
  session may have already investigated the same code or problem.
- **After fixing a bug**: Always call `log_bug` with:
  - The exact error message (for future search matching)
  - The root cause (so we understand why it happened)
  - The fix description (so we know how to handle it next time)
  - The file_path (so pre-write hooks can warn about it)
- **When the user corrects you**: Call `add_do_not_repeat` immediately. Include the
  reason — it helps judge edge cases later. Scope to a file_path when the rule only
  applies to specific code.
- **Architecture decisions**: Call `save_context` with category "architecture" whenever
  we choose a library, design pattern, or structural approach. Include the reasoning.
- **Performance decisions**: Call `save_context` with category "performance" for any
  optimization choice, benchmark result, or performance-related constraint.
- **Conventions**: Call `save_context` with category "conventions" for coding style
  decisions, naming patterns, or workflow preferences the user expresses.

### File History
When the user asks about changes to a file or why something is the way it is,
call `get_file_history` first. It shows which sessions modified the file and what
tool calls were made, giving you context before reading git blame.
```

## Examples

### Bootstrapping a Session

```
Claude: calls get_project_summary("my_project")
  -> Returns: 3 architecture decisions, 2 known bugs, 1 do-not-repeat rule, token stats

Claude: "From past sessions I see we're using arena allocators for delta cache
         and the tree-diff approach only walks changed subtrees. There's a known
         bug with merge commits having >2 parents. What would you like to work on?"
```

### Searching Past Sessions

```
User: "Did we discuss tree diffing optimization?"
Claude: calls search_sessions("tree diffing optimization")
  -> Returns: 2 sessions with matching excerpts and timestamps
```

### Saving an Architectural Decision

```
Claude: calls save_context({
  content: "Chose rmcp crate over hand-rolled JSON-RPC for MCP server. 
            rmcp handles protocol negotiation, tool registration, and 
            stdio transport. The crate is young but sufficient for our needs.",
  category: "architecture",
  project: "mnemosyne"
})
```

### Logging a Bug

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

### Adding a Do-Not-Repeat Rule

```
Claude: calls add_do_not_repeat({
  rule: "Don't use GROUP BY with FTS5 snippet() or highlight() functions",
  reason: "GROUP BY breaks the FTS match context. Use Rust-side dedup instead.",
  project: "mnemosyne",
  file_path: "memory-mcp-server/src/main.rs"
})
```

### Checking File History

```
User: "What changes have been made to pack.rs?"
Claude: calls get_file_history({ file_path: "pack.rs" })
  -> Returns: 5 tool calls (3 Edit, 2 Read) across 2 sessions with timestamps
```

### Getting Session Details

```
Claude: calls get_session_detail({ session_id: "74810b85-..." })
  -> Returns: project, timestamps, cwd, git branch, first/last user messages,
             tool call summary (Read: 12, Edit: 5, Bash: 3)
```

### Token Stats

```
Claude: calls get_token_stats({ project: "mnemosyne", days: 7 })
  -> Returns:
    {
      "period_days": 7,
      "project": "mnemosyne",
      "total_sessions": 12,
      "total_input_tokens": 847200,
      "total_output_tokens": 423100,
      "total_cache_read_tokens": 312000,
      "total_cache_creation_tokens": 98400,
      "avg_input_per_session": 70600,
      "avg_output_per_session": 35258,
      "files_with_anatomy": 14,
      "total_file_reads": 187,
      "repeated_reads_warned": 23,
      "estimated_tokens_saveable": 27600,
      "top_sessions_by_tokens": [
        { "session_id": "74810b85-...", "project": "mnemosyne", "total_tokens": 142300, "start_time": "2026-04-09..." }
      ]
    }
```

### Full Analytics Report

```
Claude: calls get_analytics({ days: 30 })
  -> Returns:
    {
      "period_days": 30,
      "total_sessions": 45,
      "total_input_tokens": 3200000,
      "total_output_tokens": 1600000,
      "total_cache_read_tokens": 980000,
      "tool_call_breakdown": [
        { "tool_name": "Read", "count": 412 },
        { "tool_name": "Edit", "count": 187 },
        { "tool_name": "Bash", "count": 156 },
        { "tool_name": "Grep", "count": 98 }
      ],
      "top_read_files": [
        { "file_path": "src/main.rs", "count": 34, "estimated_tokens": 850 },
        { "file_path": "src/parser/pack.rs", "count": 28, "estimated_tokens": 1200 }
      ],
      "top_written_files": [
        { "file_path": "src/main.rs", "count": 22, "estimated_tokens": 850 }
      ],
      "bug_count": 8,
      "bugs_by_file": [
        { "file_path": "src/parser/pack.rs", "bug_count": 3 }
      ],
      "files_with_anatomy": 14,
      "total_file_reads": 412,
      "repeated_reads_detected": 47,
      "estimated_tokens_saveable": 56400,
      "context_items_by_category": [
        { "category": "architecture", "count": 7 },
        { "category": "performance", "count": 4 },
        { "category": "conventions", "count": 3 }
      ],
      "total_do_not_repeat_rules": 5,
      "total_bugs_logged": 12,
      "oldest_context_item": "2026-04-05T19:52:13Z",
      "projects_with_context": ["mnemosyne", "git_dag_analyzer"],
      "projects_without_context": ["alarm-to-speech"]
    }
```
