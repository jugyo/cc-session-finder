# cc-session-finder

Fast full-text search for Claude Code and Codex sessions.

Native resume pickers are simple chronological lists, which makes finding old
sessions tedious. `cc-session-finder` indexes local Claude Code and Codex
sessions into SQLite, searches them with FTS5, and resumes the selected session
with the native agent CLI.

It searches titles, first prompts, project paths, and human-visible message
text. Source session stores are read-only; the tool writes only to its own
cache under `~/.cache/cc-session-finder/`.

- TUI picker with incremental search
- CLI output for agents and scripts (`json`, `tsv`, `ids`)
- Claude Code and Codex support
- Optional cwd and time filters
- Works with non-ASCII queries (FTS5 trigram tokenizer)

## Install

### Build from source

```sh
git clone https://github.com/jugyo/cc-session-finder.git
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

```sh
cc-session-finder
cc-session-finder graphql
```

| Key | Action |
| -- | -- |
| Text input / editing keys | Edit query at the cursor (IME / multi-byte safe) |
| `Option-←` / `Option-→` (`Alt-←` / `Alt-→`) | Move the query cursor by word |
| `Option-Backspace` (`Alt-Backspace`) | Delete the previous word |
| `Ctrl-A` / `Ctrl-E` | Move the query cursor to start / end |
| `Ctrl-W` | Delete the previous word |
| `↑` / `↓` or `Ctrl-P` / `Ctrl-N` | Move selection |
| `Enter` | Resume the selected session |
| `Esc` / `Ctrl-C` | Cancel and exit |

Results show the agent label (`Claude Code` or `Codex`), age, project path, and
useful metadata such as token count, branch, or PR number when available.

### CLI mode (for AI agents / scripts)

CLI mode activates whenever a subcommand is given, or when stdout is not a
TTY. The default output format is JSON.

```sh
cc-session-finder search "graphql migration" --limit 10
cc-session-finder search "auth failure" --cwd-only
cc-session-finder list --limit 50 --since 7d
cc-session-finder list --from 2026-05-01 --to 2026-05-25
cc-session-finder search "auth" --format ids
cc-session-finder list --format tsv --limit 20
cc-session-finder show 022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx
cc-session-finder show codex:022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx
cc-session-finder resume 022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx
cc-session-finder resume codex:022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx
cc-session-finder index
cc-session-finder index --reindex --progress-json
```

`--format` accepts `json` (default), `tsv`, or `ids`. JSON includes the agent,
native session ID, token counts, git / PR metadata, and score fields.

`search` and `list` both accept `--since` / `--until` time filters. `--from`
and `--to` are aliases. Values can be relative durations (`7d`, `24h`, `30m`),
`YYYY-MM-DD`, RFC3339 timestamps, or Unix timestamps.

Codex session IDs are stored with a `codex:` prefix in the shared index.
`show` and `resume` accept stable IDs, and bare native IDs also work when they
are unambiguous.

### Index management

TUI startup and `search` / `list` run an incremental index update before
reading results. If that update fails, they warn on stderr and continue with the
existing index when possible. Use `--no-update` with `search` or `list` to skip
the automatic update.

| Option | Effect |
| -- | -- |
| `--reindex` | Reparse every source session and rebuild indexed rows |
| `--reset` | Delete the DB file outright; rarely needed and more destructive |

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
