# Codex Support Refactor Plan

## Purpose

Codex support should not be added by threading more Claude-specific conditionals
through the current code. The current implementation is small and healthy, so
the refactor should also stay small: separate "where sessions come from" and
"how a session resumes" from the shared SQLite/search/TUI code.

This plan should be completed before the Codex ingestion work if possible.

## Current Claude-Specific Assumptions

- `src/paths.rs` exposes `projects_root()` specifically for
  `~/.claude/projects`.
- `src/session.rs` parses only Claude Code JSONL records.
- `src/index/ingest.rs` scans only `~/.claude/projects/*/*.jsonl`.
- `sessions.session_id` is the global primary key, with no source/agent
  discriminator.
- `project_dir` means Claude's encoded cwd directory name.
- `launch::resume` always execs `claude --resume <session_id>`.
- TUI and CLI labels do not show which agent produced a result.

These are manageable, but Codex support needs source identity in the DB and a
resume dispatch point. Otherwise mixed Claude/Codex results can collide or
resume with the wrong binary.

## Non-Goals

- No plugin runtime.
- No user-authored parser DSL.
- No dynamic loading.
- No writes to source stores such as `~/.claude` or `~/.codex`.
- No remote/cloud session indexing in the first cut.

## Target Shape

Add a small static source layer:

```rust
// src/agent/mod.rs
pub enum AgentKind {
    Claude,
    Codex,
}

pub struct SourceSession {
    pub session_key: String,       // e.g. "claude:<uuid>", "codex:<uuid>"
    pub agent: AgentKind,
    pub native_session_id: String, // id passed to the native resume command
    pub source_path: PathBuf,
    pub source_group: Option<String>,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub mtime: i64,
    pub size: i64,
    pub msg_count: u32,
    pub git_branch: Option<String>,
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
    pub pr_repo: Option<String>,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_create: u64,
}

pub struct SourceMessage {
    pub turn_index: u32,
    pub role: String,
    pub text: String,
}
```

Dispatch should be compile-time and explicit:

```rust
pub fn all_kinds() -> &'static [AgentKind] {
    &[AgentKind::Claude, AgentKind::Codex]
}

pub fn scan(kind: AgentKind) -> Result<Vec<SourceFile>>;
pub fn parse(kind: AgentKind, file: &SourceFile) -> Result<(SourceSession, Vec<SourceMessage>)>;
pub fn resume_command(kind: AgentKind, native_session_id: &str) -> Command;
```

Using a small enum plus `match` keeps future extension easy without making this
a plugin system. Adding another agent should mean one new module, one enum
variant, one match arm, fixtures, and docs.

## Schema Plan

Bump `SCHEMA_VERSION` and rebuild the cache DB, matching the repository's
current migration style.

Recommended table shape:

- `sessions.session_id`: stable user-facing key. For Claude this remains the
  old UUID/string for compatibility. For Codex use `codex:<uuid>`.
- `sessions.agent`: `claude` or `codex`.
- `sessions.native_session_id`: the raw ID accepted by the source agent's CLI.
- `sessions.source_group`: generic replacement for Claude `project_dir`.
- Keep `ai_title` in SQLite for the first implementation to reduce query churn,
  but rename Rust-facing fields to `title` where practical.
- `messages.session_id` continues to reference the stable key.

This keeps existing Claude JSON/TSV/IDs output stable while making Codex IDs
unambiguous.

## Refactor Steps

1. Add characterization tests for the current Claude parser and ingest path.
   Cover metadata extraction, message extraction, skipped tool/internal text,
   message replacement, and `resume` command construction.

2. Introduce `AgentKind`, `SourceSession`, and `SourceMessage`.
   Keep this in `src/agent/mod.rs` or `src/session_source.rs`. Do not add a
   registry abstraction beyond a fixed list of built-in kinds.

3. Move Claude parsing into a Claude source module.
   `src/session.rs` can either become `src/agent/claude.rs` or retain shared
   text helpers while the Claude-specific parser moves out.

4. Change ingest to iterate agent sources.
   Source scans should be isolated: if Codex scanning fails, do not delete old
   Codex rows; if Claude scanning succeeds, still update Claude rows. Delete
   vanished sessions only for a source that completed its scan.

5. Update the DB schema and SQL mapping.
   Add `agent`, `native_session_id`, and `source_group`. Update `HIT_COLS`,
   `Hit`, JSON output, and tests.

6. Split resume command construction from process replacement.
   Add a helper returning `{program, args}` or `Command` so tests can assert:
   Claude uses `claude --resume <native_session_id>` and Codex uses
   `codex resume <native_session_id>`.

7. Add source labels in shared presentation.
   Show `[codex]` for Codex results. Claude can remain unlabeled or show
   `[claude]`; the first implementation should choose the less noisy TUI
   behavior and keep JSON explicit with `agent`.

8. Run verification:
   `cargo fmt --all`, `cargo test`, and
   `cargo clippy --all-targets -- -D warnings`.

## Acceptance Criteria

- Existing Claude-only behavior still works.
- Existing Claude session IDs remain accepted by `show` and `resume`.
- The DB can safely contain rows from more than one agent.
- A failed scan for one source does not wipe another source's cached rows.
- All source transcript roots remain read-only.
- Adding a third built-in agent is a local change, not a new architecture.

