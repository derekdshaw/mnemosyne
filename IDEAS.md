# Ideas

Captured for later. Not on the current roadmap.

## Process-level smoke test for hook I/O contract

The unit tests in `memory-hooks/src/*.rs` only check the `Result<Option<String>>`
returned by each subcommand's `run()` — they never observe the actual bytes
that leave the binary on stdout/stderr. That's exactly the gap that let the
`eprint!` bug ship: the function returned the right buffer, but the I/O at the
edge was wrong.

A subprocess integration test would close that gap. Sketch:

- Add `memory-hooks/tests/io_contract.rs`.
- Build the binary via `env!("CARGO_BIN_EXE_memory-hooks")`.
- For each subcommand, spawn it with a realistic stdin payload (cwd,
  session_id, tool_input/tool_response) and a temp DB pointed to via env var
  (will require threading a `MNEMOSYNE_DB_PATH` override through
  `memory-common::db::open_db` — currently it always uses the user dir).
- Assert:
  - `PreRead`/`PreWrite` with content → stdout parses as
    `{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":...}}`,
    stderr contains only tracing lines (no payload bytes).
  - `SessionStart` with data → stdout starts with the briefing header,
    stderr is tracing-only.
  - `PostRead`/`PostWrite` → stdout is empty regardless of DB state.
  - All commands → exit code 0.
- Bonus: assert the `additionalContext` value round-trips intact (UTF-8,
  multi-line, emoji) so any future escaping change gets caught.

Why we punted: requires plumbing a DB-path override through `open_db`, which
isn't currently parameterized. Worth doing alongside any other DB-path work
rather than as a one-off.

## Schema-drift fail-loud at hook startup

On 2026-04-28 the hooks log filled with
`table file_anatomy has no column named top_symbols_json` errors for hours
before the schema was updated. The hooks degrade silently in that window —
exit 0, no anatomy emitted, user notices nothing until they go looking.

Cheap mitigation: at the end of `db::run_migrations`, capture the expected
version as a compile-time constant. In each hook's `main()`, after `open_db`
succeeds, read the current `schema_version` from the migrations table and
`tracing::warn!` if it doesn't match the embedded constant. Doesn't block
the hook (still advisory, still exit 0) but makes the drift visible in the
hooks log immediately, instead of as a downstream symptom.

Why we punted: low frequency (schema drift only happens on local upgrade
mismatches), and the existing post-fix migrations already self-heal on next
startup. Worth adding the next time schema work is open anyway.

## Hook context-injection canary via analytics

`db::record_overhead` already stores a `bytes` count per hook invocation. A
small analytics query — "for sessions with N reads of file X, how many had a
preceding pre_read emit non-zero bytes?" — would surface protocol regressions
empirically: if the hook starts producing zero bytes for files we know have
anatomy rows, that's a signal something changed in the Claude Code hook
contract or in our project-resolution logic.

Why we punted: needs a UX surface (CLI subcommand or a row in the existing
analytics output) and a clear definition of "expected hit rate" before the
number is actionable. The INFO log added in this change covers the immediate
diagnostic need.
