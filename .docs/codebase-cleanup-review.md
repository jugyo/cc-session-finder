# Codebase Cleanup Review

Date: 2026-05-24

## Scope

Reviewed:

- `AGENTS.md`, `README.md`, `.docs/plan.md`
- `Cargo.toml` / `Cargo.lock`
- `src/` all modules
- `.docs/` markdown/html artifacts
- `.github/workflows/*.yml`

There is no top-level `tests/` directory. Tests currently live as unit tests in
`src/main.rs`'s module tree.

Commands run:

```sh
cargo metadata --format-version 1
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -- --help
cargo run -- search --help
cargo run -- list --help
cargo run -- show --help
cargo run -- index --help
```

Baseline before cleanup:

- `cargo test`: 54 passed
- `cargo clippy --all-targets -- -D warnings`: passed

Final verification after the follow-up cleanup:

- `cargo fmt --all`: passed
- `cargo test`: 56 passed
- `cargo clippy --all-targets -- -D warnings`: passed
- `cargo run -- --help`: passed; `--snippet-lines` is no longer accepted
- `cargo run -- show --help`: passed; `--with-preview` is no longer accepted

Follow-up implementation on 2026-05-24:

- Removed `sessions.preview`, `sessions.embedded_at`, and
  `session::build_preview`.
- Removed `show --with-preview`.
- Removed hidden `--snippet-lines`.
- Simplified TUI DB access from `Arc<Mutex<Connection>>` to direct
  `Connection`.
- Restored visible TUI labels and added a current-cwd text-search ordering
  characterization.

## Implemented Safe Removals

### Removed unused root dependencies

Files changed:

- `Cargo.toml`
- `Cargo.lock`

Removed:

- direct `thiserror = "1"`
- direct `tokio = { ... }`
- unused `rusqlite` `trace` feature

Evidence:

- `rg -n "tokio|thiserror|trace_v2|rusqlite::trace" src Cargo.toml` found no
  source references.
- Current TUI/indexing code uses `std::thread` / `std::sync::mpsc`, not tokio.
- `rusqlite::functions::FunctionFlags` and `create_scalar_function` require
  the `functions` feature, which remains enabled.
- `thiserror` still appears in `Cargo.lock` only as a target-specific transitive
  dependency through `dirs -> dirs-sys -> redox_users`, verified with:

```sh
cargo tree -i thiserror --target all
```

Impact:

- No runtime behavior change.
- Reduces the direct dependency surface and removes unused tokio packages from
  the lockfile.

Why safe:

- Build, tests, and clippy pass after removal.
- No public CLI/TUI output depends on these crates or features.

### Removed obsolete "mark as used" const

File changed:

- `src/session.rs`

Removed:

```rust
const _: fn(&Path) -> Result<Vec<IndexableMessage>> = extract_indexable_messages_from_file;
```

Evidence:

- `extract_indexable_messages_from_file` is now called directly from
  `src/index/ingest.rs` during stale session ingestion.
- The const was only useful before that call site existed.

Impact:

- No behavior change.
- Removes a small AI-ish leftover that explains nothing to readers.

Why safe:

- The function remains used by production code.
- Build, tests, and clippy pass after removal.

## Safe To Remove

No additional production-code deletion is recommended without a small follow-up
task. The remaining high-confidence dead pieces touch schema, public CLI help,
or current JSON/TUI output.

## Implemented Follow-Up Removals

### `sessions.preview`, `embedded_at`, and `session::build_preview`

Evidence:

- Current `sessions_fts` indexes `ai_title`, `first_prompt`, and `cwd`
  separately in `src/index/schema.rs`.
- `sessions.preview` is still populated in `src/index/ingest.rs`, but no search
  query reads it.
- `embedded_at` is still reset when `preview` changes, but semantic/vector
  indexing has been removed.
- `.docs/plan.md` explicitly says the semantic/vector design is historical.

Implementation:

- Bumped `SCHEMA_VERSION` from 6 to 7, which rebuilds only the cache DB under
  `~/.cache/cc-session-finder/`.
- Removed the two columns from schema creation and ingest upsert SQL.
- Removed `session::build_preview`.
- Updated search test fixtures and added a schema test proving the columns are
  absent.

### Hidden `--snippet-lines` plumbing

Evidence:

- `Cli::snippet_lines` is hidden and defaults to `2`.
- TUI rendering currently treats it as a boolean: `0` hides the snippet, any
  positive value renders one one-line snippet.
- Tests cover `--snippet-lines 0` behavior, but README does not document the
  option.

Implementation:

- Removed the hidden global flag and the plumbing through `main.rs`,
  `tui/mod.rs`, `tui/app.rs`, and `tui/view.rs`.
- Removed the test that asserted `--snippet-lines 0` hides snippets.
- Verified `cargo run -- --snippet-lines 0 --help` now fails as an unexpected
  argument.

### `show --with-preview`

Evidence:

- `ShowArgs::with_preview` is exposed in help.
- `cli::run_show` ignores it with `let _ = args.with_preview; // body preview is v2`.
- README documents `show <SESSION_ID>` but does not document `--with-preview`.

Implementation:

- Removed the exposed but ignored `ShowArgs::with_preview` field.
- Removed the no-op `let _ = args.with_preview`.
- Verified `cargo run -- show --help` no longer lists the flag and
  `cargo run -- show dummy --with-preview 1` now fails as an unexpected
  argument.

### `Arc<Mutex<Connection>>` in TUI app loop

Evidence:

- `src/tui/app.rs` wraps the SQLite connection in `Arc<Mutex<_>>`.
- The spawned input thread only sends terminal events and never uses the DB.
- Search refreshes happen on the main TUI loop.
- This looks like leftover async/shared-worker shape after the tokio design was
  removed.

Implementation:

- Stored a direct `Connection` in the TUI loop.
- Changed `refresh_results` to accept `&Connection`.
- This is TUI-internal and does not affect CLI behavior or schema.

## Likely Removable But Needs Confirmation

### Generated/stale `.docs` artifacts

Evidence:

- `.docs/design-guide.html` references removed modules/features such as
  `index/embed.rs`, `sessions_vec`, `template.rs`, and semantic embeddings.
- `.docs/search-ranking-phase1-preview.html` is not referenced by README,
  AGENTS, code, workflows, or other docs.
- `.docs/tasks/*` are all completed; `.docs/TODO.md` has every Phase 2 item
  checked.

Impact:

- Documentation only.
- Deleting or archiving may remove useful historical context for the project.

Safe deletion steps:

1. Decide whether `.docs` should be an active design area or a historical
   archive.
2. Move completed task docs and generated HTML into `.docs/archive/`, or delete
   generated HTML after confirming it is not needed.
3. Update any remaining references.

## Needs Test Before Removal

### Ranking/explain score fields

Evidence:

- Current code exposes `Scores` fields such as `text_search`,
  `metadata_score`, `relevance_score`, `freshness_boost`, and
  `message_weighted_score`.
- Some old docs still describe `cwd_score`, `cwd_boost`, additive
  `keyword + cwd + recency`, or `message_count_bonus`.
- Current tests assert the present multiplicative freshness model and that
  message match count does not affect relevance.

Impact:

- These fields are serialized in CLI JSON and are observable when `--explain`
  is used.
- Removing or renaming them risks breaking AI-agent/shell consumers.

Safe deletion steps:

1. Add CLI-level JSON characterization tests for `search --explain`.
2. Decide the public JSON schema for score fields.
3. Only then remove obsolete fields or docs.

## Implemented Behavior Reconciliations

### TUI label display vs README

Evidence:

- README documents result labels `[cwd]`, `[match]`, and `[recent]`.
- `Hit.labels` is still populated by search/list code and serialized by CLI
  JSON/TSV output.

Implementation:

- Restored TUI label rendering by prefixing labels in the result title line.
- Added `title_line_renders_result_labels` to lock the behavior down.
- Updated README to say labels are shown at the start of TUI result rows.

### Search ordering and cwd boost

Evidence:

- README says sessions from the current `cwd` are boosted to the top.
- `list()` already ordered empty-query results by current-cwd first.
- `text_search()` annotated cwd labels but did not sort current-cwd hits ahead
  of newer non-current-cwd hits.

Implementation:

- Added current-cwd ordering before `mtime` / relevance tie-breakers in
  `text_search()`.
- Added `text_search_boosts_current_cwd_before_newer_results`.
- Kept the existing `Scores` fields unchanged; this is an ordering
  characterization, not a score schema change.

## Keep, Despite Looking Suspicious

### `.docs/plan.md`

Why keep:

- It contains semantic/vector search, `--no-vector`, and embedding details that
  are no longer current, but the top note explicitly marks it as historical.
- AGENTS points readers to it for design rationale and decision history.

Cleanup action:

- Do not delete. If touched, keep the historical disclaimer prominent.

### `paths::encode_cwd`

Why keep:

- It is only used by tests and has `#[allow(dead_code)]`, but it documents the
  forward Claude project-dir encoding contract next to `decode_dir_hint`.
- The tests act as cheap guardrails around path encoding assumptions.

Cleanup action:

- Keep unless the repository adopts a stricter "no test-only helpers in
  production modules" rule.

### `Progress` trait in ingest

Why keep:

- It has only two implementations plus `NoopProgress`, but it is the boundary
  that lets CLI and TUI report scan progress differently.
- It keeps ingestion independent of stderr/TUI details.

Cleanup action:

- Keep.

### `thiserror` in `Cargo.lock`

Why keep:

- Root `thiserror` dependency was removed.
- The lockfile package remains because `dirs` pulls it for Redox targets through
  `redox_users`.

Cleanup action:

- Do not manually remove it; Cargo re-adds it.

## Documentation Drift

### AGENTS module layout (resolved)

Evidence:

- AGENTS lists `src/template.rs`, but no such file exists.
- AGENTS says `src/tui/mod.rs` is a `tokio::select` event loop, but current TUI
  uses blocking threads and `std::sync::mpsc`.

Impact:

- Agent onboarding drift only.

Cleanup action:

- Updated AGENTS to remove `template.rs`, remove the stale tokio event-loop
  note, and soften the obsolete `thiserror` guidance.

### Search ranking docs

Evidence:

- Phase 1 docs describe additive `keyword + cwd + recency`.
- Current code uses `relevance_score * freshness_boost` and sorts by
  current-cwd match before `mtime` / relevance tie-breakers.
- Phase 2 verification says `message_count_bonus` is present, but current code
  has no such field and has a test proving message count does not affect
  relevance.

Impact:

- Future agents may "restore" old behavior by mistake.

Suggested cleanup:

- Add a short "current implementation snapshot" doc, or mark old phase docs as
  historical/completed like `.docs/plan.md`.

### Highlight docs

Evidence:

- `.docs/keyword-highlight.md` plans helper/tests in `src/template.rs`.
- Current implementation lives in `src/tui/view.rs`.
- The plan mentions title/prompt highlighting, while current tests explicitly
  assert the title is not highlighted.

Impact:

- Documentation only, but high confusion risk around the recent highlight work.

Suggested cleanup:

- Mark the document as historical or rewrite it to describe the current
  snippet-only highlight behavior.

## Rust Quality Notes

### `scan_and_update` stats naming

Observation:

- `IngestStats.scanned` increments only for stale/upserted files, not every file
  encountered.
- `run_index` prints `indexed {} files` using `stats.scanned`.

Risk:

- Mostly naming/reporting confusion.

Suggested cleanup:

- Rename to `parsed`/`changed`, or add a separate `visited` count in a small
  behavior-preserving task.

### Ingest errors are swallowed in non-index commands

Observation:

- `cli::maybe_update` ignores `scan_and_update` errors.
- TUI indexing worker also logs and proceeds to `Done`.

Risk:

- Search may silently use stale DB data after an indexing failure.
- This may be intentional resilience, so it is not a cleanup deletion.

Suggested cleanup:

- Decide desired CLI/TUI failure semantics before changing it.

### `truncate_to_width` edge case

Observation:

- `truncate_to_width(..., 1)` returns `"..."`, which is wider than one column.

Risk:

- Only affects very narrow terminal areas.

Suggested cleanup:

- Add a focused unit test before changing display behavior.

## Larger Follow-Up Tasks

1. Docs archive pass: mark completed phase docs historical or move them under
   `.docs/archive/`.
2. Ranking/explain schema: characterize the public JSON fields emitted by
   `search --explain` before removing or renaming score fields.
3. Ingest stats naming: clarify whether `scanned` means visited, parsed, or
   changed.
4. Indexing failure semantics: decide whether non-index commands should surface
   ingest failures or keep silently using the stale DB.
5. TUI width edge case: add a focused test before changing
   `truncate_to_width(..., 1)`.
