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
```

Type in the TUI to filter sessions incrementally.

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

## MCP server

`cc-session-finder` can expose your indexed sessions to an LLM over a read-only
[MCP](https://modelcontextprotocol.io/) stdio server, so an agent can search and
inspect your prior local work without the TUI or native resume commands.

```sh
cc-session-finder mcp        # speaks MCP JSON-RPC over stdio
```

The server is **read-only**: there is no resume, reset, reindex, or delete tool,
and responses never include native session IDs, transcript paths, or internal
scores.

### Tools

- `search_sessions` — search sessions by query, or list recent sessions when the
  query is omitted. Use this first to find candidates; when `has_more` is true,
  pass `next_cursor` as `cursor` to fetch the next page. The cursor carries the
  original query and filters.
- `get_session_overview` — metadata, first/latest message, and message count for
  one session.
- `get_session_messages` — page through a session's visible messages by
  `message_index`.
- `search_session_messages` — search visible messages within one session.
- `find_inefficient_sessions` — rank sessions by an efficiency signal to surface
  outliers. `sort_by` is `billable_tokens` (default), `error_rate` (tool errors
  ÷ tool calls), or `cache_read_ratio` (cache reads ÷ output tokens). Returns
  per-session counts and ratios only — no message text or tool bodies.
- `get_session_trajectory` — page through one session's step-level trajectory
  (one row per tool call, assistant turn, API error, or context-compaction
  event). Each step carries the tool name and input, byte sizes, per-step token
  attribution, error/sidechain flags, MCP/skill attribution, stop reason, and a
  derived duration. Use it to drill into *where* a session flagged by
  `find_inefficient_sessions` spent its tokens or hit errors.

Session IDs are opaque handles; pass the `id` from a `search_sessions` result to
the other tools.

### Client configuration

Register the server with an MCP-capable client. For example, in a Claude Code
`.mcp.json`:

```json
{
  "mcpServers": {
    "cc-session-finder": {
      "command": "cc-session-finder",
      "args": ["mcp"]
    }
  }
}
```

### Debug harness

The `sessions` subcommand group runs the same core as the MCP tools and emits the
exact same JSON shapes, which is handy for testing without an MCP client:

```sh
cc-session-finder sessions list --limit 20
cc-session-finder sessions list --limit 20 --cursor <next_cursor>
cc-session-finder sessions search "mcp support" --limit 20
cc-session-finder sessions search --limit 20 --cursor <next_cursor>
cc-session-finder sessions overview <id>
cc-session-finder sessions messages <id> --order asc --limit 10
cc-session-finder sessions search-messages <id> "schema" --limit 10
cc-session-finder sessions inefficient --sort-by cache-read-ratio --limit 20
cc-session-finder sessions trajectory <id> --limit 30
```

Each indexed session also carries derived analysis columns — `tool_call_count`,
`tool_error_count`, `thinking_tokens`, and `wall_clock_ms` (last − first
transcript timestamp) — surfaced in `show`, the `sessions` overview/cards
metadata, and `find_inefficient_sessions`.

### Step-level trajectory

Beyond the per-session aggregates, each Claude session is also decomposed into a
`trajectory` table — one row per step (tool call, tool-less assistant turn, API
error, or context-compaction event). A step records the `tool_use` input in full
(`tool_input`, capped at 128 KB with the original size kept in
`tool_input_bytes`), the result size (`tool_result_bytes`), an `is_error` flag,
per-step token attribution (deduped by `message.id`, so step tokens sum to the
session totals), and small inefficiency-detection fields: `is_sidechain`,
`context_management`, `is_api_error` / `api_error_status`, `stop_reason`,
`attribution_mcp_server` / `attribution_mcp_tool` / `attribution_skill`,
`duration_ms` (from the gap to the next step), and `permission_mode`. Read it via
`get_session_trajectory` or `sessions trajectory`. Codex sessions currently yield
an empty trajectory.

**Tool result bodies are not stored by default** — only `tool_result_bytes` is
recorded, since result content runs ~45 MB/month (~700 MB/year) here. To capture
bodies too, set `CC_SESSION_FINDER_STORE_TOOL_RESULTS=1` before indexing; bodies
are then stored (capped at 128 KB) in `tool_result`.

**Privacy:** `tool_input` (and opt-in `tool_result`) bodies are stored verbatim
in the local cache DB, so source code, file paths, shell commands, and any
secrets that passed through tool calls are kept in plaintext under the cache
directory. The cache never leaves your machine, but treat it as sensitive.

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
