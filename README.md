# cc-session-finder

Fast full-text search for [Claude Code](https://claude.com/claude-code) and
Codex sessions.

Native resume pickers are simple chronological lists, which makes finding old
sessions tedious. `cc-session-finder` indexes local Claude Code and Codex
sessions into SQLite and runs **FTS5 text search** from a TUI. Selecting a
result execs the matching native resume command in place.

- Incremental search in a TUI
- Non-interactive CLI mode for AI agents / shells (JSON / TSV / IDs)
- Optional `--cwd-only` filtering for the current project
- Works with non-ASCII queries (FTS5 trigram tokenizer)

## Install

### Build from source

```sh
git clone https://github.com/<owner>/cc-session-finder.git
cd cc-session-finder
cargo install --path .
```

`cargo install` drops the binary at `~/.cargo/bin/cc-session-finder`.

### Pre-built binary (macOS, Apple Silicon)

Grab `cc-session-finder-vX.Y.Z-aarch64-apple-darwin.tar.gz` from the
[Releases](../../releases) page, extract it, and put the binary somewhere on
your `PATH`.

```sh
tar -xzf cc-session-finder-vX.Y.Z-aarch64-apple-darwin.tar.gz
mv cc-session-finder /usr/local/bin/
```

### Build requirements

- Rust stable (`rustup` recommended)
- macOS / Linux (Windows is untested)

## Usage

### TUI mode

Launching without arguments starts the TUI.

```sh
cc-session-finder
```

Start with an initial query:

```sh
cc-session-finder graphql
```

#### Key bindings

| Key | Action |
| -- | -- |
| Text input / editing keys | Edit query at the cursor (IME / multi-byte safe) |
| `↑` / `↓` or `Ctrl-P` / `Ctrl-N` | Move selection |
| `Enter` | `chdir` to the session's `cwd` and exec the native resume command |
| `Esc` / `Ctrl-C` | Cancel and exit |

#### Agent Labels

The TUI shows a colored source label after each title, such as
`title - Claude Code` or `title - Codex`. CLI JSON output also includes the
`agent` field.

### CLI mode (for AI agents / scripts)

CLI mode activates whenever a subcommand is given, or when stdout is not a
TTY. The default output format is JSON.

```sh
# Keyword search
cc-session-finder search "graphql migration" --limit 10
cc-session-finder search "graphql migration" --since 7d --until 1d

# Include ranking details in JSON output
cc-session-finder --explain search "graphql migration" --limit 10

# Newest-first listing
cc-session-finder list --limit 50 --since 7d
cc-session-finder list --from 2026-05-01 --to 2026-05-25

# Just the session IDs (for xargs piping)
cc-session-finder search "auth" --format ids

# Details of a single session
cc-session-finder show 022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx
cc-session-finder show codex:022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx

# Resume directly without going through the TUI
cc-session-finder resume 022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx
cc-session-finder resume codex:022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx

# Index maintenance
cc-session-finder index
cc-session-finder index --reindex --progress-json
```

`--format` accepts `json` (default), `tsv`, or `ids`. JSON output includes
`agent` and `native_session_id`. `ids` outputs the stable ID used by the shared
index, so Codex rows are prefixed like `codex:<uuid>`.

`search` and `list` both accept `--since` / `--until` time filters. `--from`
and `--to` are aliases. Values can be relative durations (`7d`, `24h`, `30m`),
`YYYY-MM-DD`, RFC3339 timestamps, or Unix timestamps.

`show` and `resume` accept stable IDs. Bare native IDs also work when they are
unambiguous; Claude IDs remain unchanged for compatibility.

#### Exit codes

| Code | Meaning |
| -- | -- |
| `0` | Success (0 results is still 0) |
| `1` | Argument or configuration error |
| `2` | Index inconsistency |
| `3` | `show` could not find the given session_id |
| `130` | `SIGINT` interrupt |

### Index management

TUI startup and `search` / `list` run an incremental index update before
reading results. If that update fails, they warn on stderr and continue with the
existing index when possible. Use `--no-update` with `search` or `list` to skip
the automatic update.

| Option | Effect |
| -- | -- |
| `--reindex` | Empty the DB and rebuild from scratch |
| `--reset` | Delete the DB file outright (more destructive than `--reindex`) |

## File layout

| Path | Purpose |
| -- | -- |
| `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` | Raw sessions written by Claude Code (read-only) |
| `~/.codex/state_5.sqlite` | Codex local thread metadata (read-only) |
| `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | Codex rollout transcripts (read-only) |
| `~/.codex/archived_sessions/rollout-*.jsonl` | Older or archived Codex rollout transcripts (read-only) |
| `~/.cache/cc-session-finder/index.db` | SQLite index (FTS5) |

## Logging

Use the `CC_SESSION_FINDER_LOG` environment variable to control the `tracing`
filter.

```sh
CC_SESSION_FINDER_LOG=debug cc-session-finder search graphql 2> debug.log
```

## Development

```sh
cargo build
cargo test
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`) runs fmt / clippy / test on both Ubuntu and
macOS.

## License

MIT
