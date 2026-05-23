use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

mod cli;
mod index;
mod launch;
mod paths;
mod relative_time;
mod session;
mod template;
mod tui;

#[derive(Debug, Parser)]
#[command(
    name = "cc-session-finder",
    version,
    about = "Fast finder for Claude Code sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Optional initial query (TUI mode only)
    query: Option<String>,

    /// Force a full reindex before starting
    #[arg(long, global = true)]
    reindex: bool,

    /// Delete the on-disk index DB before starting (more destructive than
    /// --reindex; drops the SQLite file and lets the next run recreate it
    /// from scratch).
    #[arg(long, global = true)]
    reset: bool,

    /// Show ranking explanation details when available
    #[arg(long, global = true)]
    explain: bool,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Search sessions and print structured output.
    Search(SearchArgs),
    /// List sessions newest-first.
    List(ListArgs),
    /// Show details of one session.
    Show(ShowArgs),
    /// Update the index (incremental by default).
    Index(IndexArgs),
    /// Resume a session by id (exec `claude --resume`).
    Resume(ResumeArgs),
}

#[derive(Debug, Args)]
struct SearchArgs {
    /// Query string
    query: String,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    cwd_only: bool,
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
    #[arg(long)]
    no_update: bool,
}

#[derive(Debug, Args)]
struct ListArgs {
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    cwd_only: bool,
    /// Duration filter, e.g. "7d" or "24h"
    #[arg(long)]
    since: Option<String>,
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
    #[arg(long)]
    no_update: bool,
}

#[derive(Debug, Args)]
struct ShowArgs {
    session_id: String,
    /// Include up to N user/assistant messages with body
    #[arg(long)]
    with_preview: Option<usize>,
}

#[derive(Debug, Args)]
struct IndexArgs {
    #[arg(long)]
    reindex: bool,
    #[arg(long)]
    quiet: bool,
    /// Emit progress as JSON Lines on stderr
    #[arg(long)]
    progress_json: bool,
}

#[derive(Debug, Args)]
struct ResumeArgs {
    session_id: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Json,
    Tsv,
    Ids,
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();

    if cli.reset {
        if let Err(e) = wipe_index_db() {
            eprintln!("error: --reset failed: {e:#}");
            return ExitCode::from(1);
        }
    }

    let result: anyhow::Result<ExitCode> = match cli.command {
        Some(Cmd::Search(args)) => cli::run_search(args, cli.reindex, cli.explain),
        Some(Cmd::List(args)) => cli::run_list(args, cli.reindex, cli.explain),
        Some(Cmd::Show(args)) => cli::run_show(args),
        Some(Cmd::Index(args)) => cli::run_index(args, cli.reindex),
        Some(Cmd::Resume(args)) => cli::run_resume(args),
        None => {
            if atty::is(atty::Stream::Stdout) {
                tui::run(cli.query, cli.reindex, cli.explain)
            } else {
                // Non-TTY without subcommand → behave like `list --format json`
                cli::run_list(
                    ListArgs {
                        limit: 50,
                        cwd: None,
                        cwd_only: false,
                        since: None,
                        format: Format::Json,
                        no_update: false,
                    },
                    cli.reindex,
                    cli.explain,
                )
            }
        }
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            if is_broken_pipe(&e) {
                ExitCode::from(0)
            } else {
                eprintln!("error: {e:#}");
                ExitCode::from(1)
            }
        }
    }
}

fn wipe_index_db() -> std::io::Result<()> {
    let db = index::db_path();
    for path in [
        db.clone(),
        db.with_extension("db-wal"),
        db.with_extension("db-shm"),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => eprintln!("removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    if err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .map(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            .unwrap_or(false)
    }) {
        return true;
    }
    // Fallback: some library errors (e.g. serde_json) don't preserve the
    // underlying io::Error through `source()`; sniff the message.
    let msg = format!("{:#}", err);
    msg.contains("Broken pipe") || msg.contains("os error 32")
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CC_SESSION_FINDER_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}
