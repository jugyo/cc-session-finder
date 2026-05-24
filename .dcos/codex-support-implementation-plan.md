# Codex Support Implementation Plan

## Goal

Index local Codex sessions alongside Claude Code sessions, search them through
the existing TUI/CLI, and resume the selected session with the correct native
CLI command.

This plan assumes the small source-layer refactor in
`codex-support-refactor-plan.md` is done first.

## Local Codex Storage Observations

Observed on 2026-05-24 with `codex-cli 0.133.0`:

- Metadata lives in `~/.codex/state_5.sqlite`, table `threads`.
- `threads` includes useful fields such as `id`, `rollout_path`, `created_at`,
  `updated_at`, `created_at_ms`, `updated_at_ms`, `source`, `model_provider`,
  `cwd`, `title`, `first_user_message`, `preview`, `tokens_used`, `archived`,
  `git_branch`, `model`, and `reasoning_effort`.
- Conversation JSONL files live under
  `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
- Some older/archived files can also exist under
  `~/.codex/archived_sessions/rollout-*.jsonl`.
- `~/.codex/session_index.jsonl` exists, but it only has `id`, `thread_name`,
  and `updated_at`, so it is a weaker source than `state_5.sqlite`.
- JSONL records use top-level `type` values such as `session_meta`,
  `turn_context`, `response_item`, and `event_msg`.
- Visible message text can be read from either:
  - `event_msg` records with `payload.type` of `user_message` or
    `agent_message`.
  - `response_item` records with `payload.type == "message"`, role
    `user`/`assistant`, and content parts like `input_text`/`output_text` with
    a `text` field.
- Tool calls, tool outputs, and reasoning records are present but should not be
  indexed in the first cut.
- `codex resume [SESSION_ID]` resumes by UUID or thread name.

Treat these as observed implementation details, not a public stable API. The
parser should be tolerant and tests should pin the behavior we rely on.

## Scope

Implement:

- Read-only scanning of local Codex metadata and rollout JSONL.
- Mixed Claude/Codex search in the same SQLite index.
- Agent-aware `show`, `list`, `search`, `ids`, and `resume`.
- Clear JSON output fields for `agent` and `native_session_id`.
- TUI display that makes Codex results identifiable.

Defer:

- Codex Cloud task indexing.
- Fork support.
- Full indexing of tool call arguments/output.
- Configurable source enable/disable files.
- Plugin-style third-party agent support.

## Data Source Strategy

Use `~/.codex/state_5.sqlite` as the primary Codex session index.

1. Open it read-only with rusqlite. The app may have WAL files next to it, so
   use normal SQLite read-only access rather than copying or using immutable
   mode.

2. Query `threads` newest-first. Only include rows with a non-empty `id` and a
   usable `cwd`. Include archived rows if `rollout_path` points to a readable
   transcript; otherwise keep the metadata row searchable by title/prompt only.

3. Resolve transcript path:
   - Prefer `threads.rollout_path`.
   - If missing or unreadable, search the known Codex roots for a file whose
     name contains the thread ID.
   - If still missing, create a metadata-only session with zero messages.

4. Use `updated_at_ms / 1000` when present. Fall back to `updated_at`, then file
   mtime.

5. Use `tokens_used` for total token display if detailed token counts are not
   available. If parsing `token_count` records later is cheap, map the latest
   `total_token_usage.input_tokens`, `output_tokens`, and
   `cached_input_tokens` into the existing token columns.

## Codex Parser Plan

Create `src/agent/codex.rs`.

Metadata extraction:

- `native_session_id`: `threads.id`.
- `session_id`: stable key `codex:<id>`.
- `agent`: `codex`.
- `cwd`: `threads.cwd`.
- `title`: `threads.title`, falling back to `preview`.
- `first_prompt`: `threads.first_user_message`, falling back to the first
  visible user message in the rollout.
- `mtime`: `updated_at_ms / 1000`, `updated_at`, or file mtime.
- `source_path`: `rollout_path` when present.
- `source_group`: `threads.source` or `model_provider`.
- `git_branch`: `threads.git_branch`.
- PR fields: `None` for now.

Message extraction:

1. First pass: collect visible UI messages from `event_msg`.
   - `payload.type == "user_message"` -> role `user`.
   - `payload.type == "agent_message"` -> role `assistant`.
   - Prefer `payload.message` when it is a non-empty string.
   - Apply the shared human-visible-text filter.

2. Fallback pass: if no visible UI messages were found, collect
   `response_item.payload.type == "message"` records.
   - Include only role `user` or `assistant`.
   - Skip `developer`, `system`, `tool`, function calls, function outputs, and
     reasoning.
   - Join `content[]` parts whose `type` is `input_text`, `output_text`, or
     `text`, reading their `text` field.

3. Keep `turn_index` as extraction order.

This avoids indexing large tool output and internal prompts while still making
the conversation searchable.

## CLI/TUI Behavior

- Default search/list should include all indexed agents.
- Do not add an `--agent` filter in this implementation. Mixed search is the
  product behavior for the first Codex-capable release.
- `--format ids` should output the stable `session_id`, so Codex rows are
  unambiguous (`codex:<uuid>`).
- `show` and `resume` should accept:
  - existing Claude native IDs.
  - stable IDs such as `codex:<uuid>`.
  - bare Codex UUIDs only if there is no ambiguity.
- TUI result rows should show an agent marker for Codex. JSON output should
  always include `agent` and `native_session_id`.

## Resume Behavior

Dispatch by `Hit.agent`:

- Claude: `claude --resume <native_session_id>`
- Codex: `codex resume <native_session_id>`

Before exec, keep the current behavior of changing into the session `cwd` when
it exists. Do not pass dangerous bypass flags or alter sandbox/approval
settings.

## Implementation Phases

### Phase 1: Source Identity and Schema

- Add `agent` and `native_session_id` to `Hit`.
- Add schema columns and bump `SCHEMA_VERSION`.
- Make `sessions.session_id` a stable key while preserving current Claude IDs.
- Update `messages` to reference the stable key.
- Update all search/list/show SQL and tests.

### Phase 2: Source Modules

- Move Claude parsing/scanning behind the new static source interface.
- Keep existing Claude fixture tests passing.
- Update ingest to scan per source and delete vanished rows per successful
  source only.

### Phase 3: Codex Metadata Scan

- Add read-only `state_5.sqlite` query support.
- Convert `threads` rows into `SourceSession`.
- Index metadata-only Codex sessions first.
- Verify `list --format json` returns Codex rows with useful titles/cwd/mtime.

### Phase 4: Codex Transcript Parsing

- Parse rollout JSONL visible messages.
- Populate `messages` and `messages_fts`.
- Add fixtures for `event_msg`, `response_item` fallback, token count, missing
  transcript, and skipped function/reasoning records.
- Verify text search finds Codex message content and snippets look right.

### Phase 5: Resume and Presentation

- Dispatch `launch::resume` by agent.
- Add agent marker/fields to TUI and JSON.
- Update README file layout, usage examples, and safety notes.

## Testing Plan

Use fixtures, not the user's real `~/.codex`.

- Unit tests for Codex parser:
  - `event_msg` user/assistant extraction.
  - `response_item` fallback extraction.
  - skip developer/system/tool/reasoning/function records.
  - metadata fallback from `threads`.
  - missing rollout path still indexes metadata.

- Ingest tests:
  - Claude and Codex rows coexist.
  - Codex scan failure does not delete Claude rows.
  - Deletions are scoped to the completed source.
  - stable IDs prevent collision.

- Search tests:
  - metadata search matches Codex title/first prompt/cwd.
  - message search matches Codex visible messages.
  - snippets attach to Codex hits.

- Resume tests:
  - Claude builds `claude --resume <id>`.
  - Codex builds `codex resume <id>`.
  - `show`/`resume` reject ambiguous bare IDs with a clear error.

- Final verification:
  - `cargo fmt --all`
  - `cargo test`
  - `cargo clippy --all-targets -- -D warnings`

## Risks and Mitigations

- Codex storage format is not guaranteed stable.
  Mitigation: isolate parser in `agent/codex.rs`, keep fixtures small, and make
  parse failures warn and skip rather than fail the whole index.

- Codex SQLite may be open by the app.
  Mitigation: read-only SQLite access with WAL support; never write to Codex DB.

- Duplicate text from `event_msg` and `response_item`.
  Mitigation: prefer `event_msg`; use `response_item` only as fallback.

- ID ambiguity across agents.
  Mitigation: stable prefixed keys for non-Claude rows and explicit `agent` in
  the DB/output.

- Mixed results could surprise existing Claude-only users.
  Mitigation: show source identity clearly and keep JSON output explicit about
  each result's `agent`.

## Done Definition

- Running `cc-session-finder` shows Claude and Codex sessions in one result
  list.
- Searching for text from a Codex conversation returns that Codex thread.
- Pressing Enter on a Codex result execs `codex resume <id>`.
- Existing Claude workflows continue to behave as before.
- No source session files or Codex/Claude databases are modified.
- README documents both source layouts and the mixed-agent behavior.
