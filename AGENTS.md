# AGENTS.md

Guide for AI coding agents working in this repository.
(`CLAUDE.md` is a symlink to this file.)

For the user-facing overview, installation, and usage, see
[`README.md`](README.md). For the design rationale and decision history, see
[`.docs/plan.md`](.docs/plan.md).

## What this project is

A Rust TUI / CLI that indexes local Claude Code and Codex sessions into
SQLite and runs FTS5 keyword search over them. Pressing Enter execs the
matching native resume command, such as `claude --resume <session-id>` or
`codex resume <session-id>`.

## Key constraints

- **Source session stores are read-only.** Writing under `~/.claude/` or
  `~/.codex/` can corrupt the source agent's own state. Only write under our
  cache (`~/.cache/cc-session-finder/`).
- **TUI and CLI share the same DB.** SQLite is opened in WAL mode for
  concurrent reader / writer access.
- **`--reset` is destructive.** Don't use it without explicit user
  permission; `--reindex` is enough in almost every case.

## Module layout

```
src/
├── main.rs           # CLI entry (clap), subcommand dispatch
├── cli.rs            # Non-interactive subcommands (search/list/show/index/resume)
├── tui/              # ratatui-based TUI
│   ├── mod.rs        # TUI startup after indexing
│   ├── app.rs        # State management and key bindings
│   ├── view.rs       # Rendering
│   ├── input.rs      # IME / multi-byte safe query editor
│   └── indexing.rs   # UI state during index updates
├── index/
│   ├── mod.rs        # Public API (open, db_path)
│   ├── schema.rs     # Migrations
│   ├── ingest.rs     # Incremental scan + UPSERT
│   └── search.rs     # FTS5 keyword queries
├── agent/
│   ├── mod.rs        # Source abstraction and dispatch
│   ├── claude.rs     # Claude Code source scanner
│   └── codex.rs      # Codex source scanner
├── session.rs        # Claude Code JSONL parser
├── paths.rs          # source roots and cache paths
├── relative_time.rs  # "3h ago" style relative timestamps
└── launch.rs         # native resume command dispatch
```

## Development workflow

```sh
cargo build
cargo test
cargo fmt    --all
cargo clippy --all-targets -- -D warnings
```

- To exercise the TUI by hand, just `cargo run` (no args ⇒ TUI). For CLI
  smoke tests, `cargo run -- list --limit 5 | jq` works well.
- For verbose logs, set `CC_SESSION_FINDER_LOG=debug`.
- If you suspect index corruption, try `cargo run -- --reindex` before
  reaching for `--reset`.

## Code style

- `cargo fmt` (default rustfmt config) and `cargo clippy -D warnings` are
  CI blockers. Run both before opening a PR.
- Use `anyhow::Result` at function boundaries. Add an internal error enum only
  when it removes real ambiguity from callers.
- Short doc comments (`///`) on public APIs only. Don't narrate internals.
- No purely descriptive comments (this is enforced repo-wide via
  CLAUDE.md / AGENTS.md).

## CI

`.github/workflows/ci.yml` runs fmt / clippy / test on every PR and on
push to `main`. The test matrix covers `ubuntu-latest` and `macos-latest`.
`RUSTFLAGS: "-D warnings"` means warnings fail the build.

## Pinning GitHub Actions versions (important)

Every `uses:` reference in `.github/workflows/*.yml` **must be pinned to a
concrete version**. Don't use major tags like `@v4` or branch refs like
`@stable`.

- For actions with release tags: pin to a specific patch version
  (e.g. `actions/checkout@v6.0.2`, `Swatinem/rust-cache@v2.9.1`).
- For actions that only publish branch refs (e.g. `dtolnay/rust-toolchain`):
  pin to a commit SHA and leave a comment recording the source branch /
  date.

This matters for both supply-chain safety and build reproducibility. Apply
this rule to both new workflow files and edits to existing ones.

## Release process

Pushing a tag matching `vX.Y.Z` triggers `.github/workflows/release.yml`,
which builds an `aarch64-apple-darwin` binary and attaches it to a GitHub
Release.

1. Bump `version` in `Cargo.toml`.
2. Run `cargo build --release` once so the bump propagates to `Cargo.lock`
   (the `cc-session-finder` entry there must move in lockstep).
3. Commit both files together and push to `main`.
   ```sh
   git add Cargo.toml Cargo.lock
   git commit -m "Release vX.Y.Z"
   git push
   ```
4. Tag and push the tag.
   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
5. After the `Release` workflow finishes, verify the GitHub Releases page:
   - `cc-session-finder-vX.Y.Z-aarch64-apple-darwin.tar.gz` is attached
   - Release notes are auto-generated (`generate_release_notes: true`)
6. Edit the release notes to add highlights or migration notes as needed.

Tag naming convention: `v` prefix + SemVer (`v0.2.0`, `v1.0.0-rc.1`, ...).
Suffixed tags like `v0.2.0-rc.1` are marked as pre-releases on GitHub.

To unpublish a release, remove both the tag and the GitHub Release.

```sh
git push --delete origin vX.Y.Z
git tag -d vX.Y.Z
gh release delete vX.Y.Z
```

Agent-specific reminders:

- **Do not push tags without explicit user permission** — a tag push runs
  the release workflow and produces a public artifact.
- The release workflow only builds for macOS aarch64. Linux and x86_64
  macOS users build from source for now.

## Things to avoid (footguns)

- Writing to or deleting anything under `~/.claude/` or `~/.codex/`
- Running `--reset` without confirming with the user
- Running `cargo install` without confirming (it writes into the user's
  `~/.cargo/bin/`)
- Leaving floating tags (`@v4`, `@stable`) in GitHub Actions workflows
- Pushing `git push origin vX.Y.Z` on your own

## References

- [README.md](README.md) — user-facing docs
- [.docs/plan.md](.docs/plan.md) — architecture and design rationale (historical;
  documents the earlier keyword + semantic design before semantic search was
  removed)
- [ratatui](https://ratatui.rs/)
