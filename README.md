# cc-session-finder

Fast keyword + semantic finder for [Claude Code](https://claude.com/claude-code) sessions.

`claude --resume`'s built-in picker is a simple chronological list, which makes
finding old sessions tedious. `cc-session-finder` indexes every JSONL session
under `~/.claude/projects/` into SQLite and runs **FTS5 keyword search** and
**multilingual embedding-based semantic search** in parallel from a TUI.
Selecting a result execs `claude --resume <session-id>` in place.

- Incremental search in a TUI
- Non-interactive CLI mode for AI agents / shells (JSON / TSV / IDs)
- Sessions from the current `cwd` are boosted to the top
- Works with non-ASCII queries (FTS5 trigram tokenizer)
- Runs locally after a one-time embedding model download (~120MB)

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
- On first launch the multilingual MiniLM embedding model (~120MB) is
  downloaded automatically and cached under
  `~/.cache/cc-session-finder/models/`

To build without embeddings:

```sh
cargo install --path . --no-default-features
```

Semantic search is disabled, but keyword search and the `cwd` boost still work.

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
| Any character | Append to query (IME / multi-byte safe) |
| `↑` / `↓` or `Ctrl-P` / `Ctrl-N` | Move selection |
| `Enter` | `chdir` to the session's `cwd` and exec `claude --resume` |
| `Esc` / `Ctrl-C` | Cancel and exit |

#### Result labels

| Label | Meaning |
| -- | -- |
| `[cwd]` | Session's `cwd` matches the current working directory |
| `[kw]` | Matched by FTS5 keyword search |
| `[~]` | Matched by vector KNN (and not already in the keyword hits) |
| `[recent]` | Default label when the query is empty (newest-first) |

### CLI mode (for AI agents / scripts)

CLI mode activates whenever a subcommand is given, or when stdout is not a
TTY. The default output format is JSON.

```sh
# Search (default: keyword + vector merged)
cc-session-finder search "graphql migration" --limit 10

# Keyword path only
cc-session-finder search "graphql" --mode keyword

# Vector path only
cc-session-finder search "database migration" --mode vector

# Newest-first listing
cc-session-finder list --limit 50 --since 7d

# Just the session IDs (for xargs piping)
cc-session-finder search "auth" --format ids

# Details of a single session
cc-session-finder show 022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx

# Resume directly without going through the TUI
cc-session-finder resume 022d82ca-xxxx-xxxx-xxxx-xxxxxxxxxxxx

# Index maintenance
cc-session-finder index
cc-session-finder index --reindex --progress-json
```

`--format` accepts `json` (default), `tsv`, or `ids`.

#### Exit codes

| Code | Meaning |
| -- | -- |
| `0` | Success (0 results is still 0) |
| `1` | Argument or configuration error |
| `2` | Index inconsistency |
| `3` | `show` could not find the given session_id |
| `130` | `SIGINT` interrupt |

### Index management

| Option | Effect |
| -- | -- |
| `--reindex` | Empty the DB and rebuild from scratch |
| `--reset` | Delete the DB file outright (more destructive than `--reindex`) |
| `--no-vector` | Disable the vector path (useful in CI without the embed model) |
| `index --no-embed` | Refresh metadata only; skip vector generation |

## File layout

| Path | Purpose |
| -- | -- |
| `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` | Raw sessions written by Claude Code (read-only) |
| `~/.cache/cc-session-finder/index.db` | SQLite index (FTS5 + sqlite-vec) |
| `~/.cache/cc-session-finder/models/` | fastembed multilingual model cache |

## Logging

Use the `CC_SESSION_FINDER_LOG` environment variable to control the `tracing`
filter.

```sh
CC_SESSION_FINDER_LOG=debug cc-session-finder search graphql 2> debug.log
```

## Development

```sh
cargo build --all-features
cargo test  --all-features
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

CI (`.github/workflows/ci.yml`) runs fmt / clippy / test on both Ubuntu and
macOS.

## License

MIT
