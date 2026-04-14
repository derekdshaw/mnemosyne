# Release Notes

## v1.0.1

### Session Start Briefing

Added a `session-start` subcommand to `memory-hooks` that automatically injects a project briefing into the conversation at the beginning of each session. The briefing includes do-not-repeat rules, saved context items, recent bugs, and session stats — eliminating the need for Claude to manually call `get_project_summary`.

This is registered as a `SessionStart` hook alongside the existing `session-ingester` hook. Install scripts and manual installation docs have been updated accordingly.

### Global Rule Visibility Fix

Fixed a bug where global (unscoped) do-not-repeat rules, context items, and bugs were excluded from query results when filtering by project. The SQL queries in `get_project_summary`, `get_do_not_repeat`, and related functions now use `project IS NULL OR ...` so global entries always appear regardless of project filter.

### Scoped Rule Feedback

`add_do_not_repeat` now reports the scope of the created rule in its response message (e.g., `scope: GLOBAL (applies to all projects)` or `scope: project=myproj`). The `project` field description in the tool schema has been updated to clarify that omitting it creates a global rule.

### Token Estimate Fix

Fixed the `post-read` hook not extracting file content from `tool_response`. Claude Code nests Read content at `tool_response.file.content`, not `tool_response.content`. This caused all token estimates in `session_reads` and `file_anatomy` to be NULL, making the `estimated_tokens_saveable` metric in analytics and token stats always report 0. Token estimates will now populate correctly as files are read in new sessions.

### CI

- Merged release job into the CI workflow and added manual dispatch support.

### Commits

- `af912c8` Merge release into CI workflow and add manual dispatch
- `56cb077` Add session-start hook and fix global rule visibility
- `f7c74af` Fix post-read hook not extracting file content from tool_response
