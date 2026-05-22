//! Non-interactive CLI subcommands.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::index;
use crate::index::ingest::{IngestStats, Progress};
use crate::index::search::Hit;
use crate::{Format, IndexArgs, ListArgs, ResumeArgs, SearchArgs, ShowArgs};

#[derive(Serialize)]
struct OutputDoc<'a> {
    query: Option<&'a str>,
    cwd: Option<String>,
    results: &'a [Hit],
    stats: OutputStats,
}

#[derive(Serialize)]
struct OutputStats {
    total_sessions: i64,
    keyword_hits: usize,
    took_ms: u128,
}

fn default_cwd() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

fn maybe_update(reindex: bool, no_update: bool) -> Result<rusqlite::Connection> {
    let mut conn = index::open()?;
    if no_update && !reindex {
        return Ok(conn);
    }
    let _ = index::ingest::scan_and_update(&mut conn, reindex, &index::ingest::NoopProgress);
    Ok(conn)
}

pub fn run_list(args: ListArgs, reindex: bool) -> Result<ExitCode> {
    let start = std::time::Instant::now();
    let conn = maybe_update(reindex, args.no_update)?;
    let cwd = args.cwd.or_else(default_cwd);
    let since_secs = args.since.as_deref().and_then(parse_duration);
    let hits = index::search::list(&conn, cwd.as_deref(), args.cwd_only, since_secs, args.limit)?;
    write_results(&hits, None, cwd.as_deref(), args.format, start, &conn)?;
    Ok(ExitCode::SUCCESS)
}

pub fn run_search(args: SearchArgs, reindex: bool) -> Result<ExitCode> {
    let start = std::time::Instant::now();
    let conn = maybe_update(reindex, args.no_update)?;
    let cwd = args.cwd.or_else(default_cwd);

    let hits = index::search::keyword(
        &conn,
        &args.query,
        cwd.as_deref(),
        args.cwd_only,
        args.limit,
    )?;

    write_results(
        &hits,
        Some(&args.query),
        cwd.as_deref(),
        args.format,
        start,
        &conn,
    )?;
    Ok(ExitCode::SUCCESS)
}

pub fn run_show(args: ShowArgs) -> Result<ExitCode> {
    let conn = index::open()?;
    let hit = index::search::show(&conn, &args.session_id)?;
    match hit {
        Some(h) => {
            let json = serde_json::to_string_pretty(&h)?;
            println!("{}", json);
            let _ = args.with_preview; // body preview is v2
            Ok(ExitCode::SUCCESS)
        }
        None => {
            eprintln!("session not found: {}", args.session_id);
            Ok(ExitCode::from(3))
        }
    }
}

pub fn run_index(args: IndexArgs, reindex_top: bool) -> Result<ExitCode> {
    let mut conn = index::open()?;
    let progress: Box<dyn Progress> = if args.quiet {
        Box::new(index::ingest::NoopProgress)
    } else if args.progress_json {
        Box::new(JsonProgress)
    } else {
        Box::new(StderrProgress::default())
    };
    let stats =
        index::ingest::scan_and_update(&mut conn, args.reindex || reindex_top, progress.as_ref())?;
    if !args.quiet && !args.progress_json {
        eprintln!(
            "indexed {} files (upserted {}, deleted {}, total {})",
            stats.scanned, stats.upserted, stats.deleted, stats.total
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn run_resume(args: ResumeArgs) -> Result<ExitCode> {
    let conn = index::open()?;
    let hit = index::search::show(&conn, &args.session_id)?
        .with_context(|| format!("session not found: {}", args.session_id))?;
    crate::launch::resume(&hit)?;
    Ok(ExitCode::SUCCESS)
}

fn write_results(
    hits: &[Hit],
    query: Option<&str>,
    cwd: Option<&std::path::Path>,
    fmt: Format,
    start: std::time::Instant,
    conn: &rusqlite::Connection,
) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match fmt {
        Format::Json => {
            let total_sessions: i64 = conn
                .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
                .unwrap_or(0);
            let doc = OutputDoc {
                query,
                cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
                results: hits,
                stats: OutputStats {
                    total_sessions,
                    keyword_hits: hits
                        .iter()
                        .filter(|h| h.labels.iter().any(|l| l == "keyword"))
                        .count(),
                    took_ms: start.elapsed().as_millis(),
                },
            };
            serde_json::to_writer_pretty(&mut out, &doc)?;
            writeln!(out)?;
        }
        Format::Tsv => {
            for h in hits {
                writeln!(
                    out,
                    "{}\t{}\t{}\t{}",
                    h.session_id,
                    format_mtime(h.mtime),
                    h.labels.join(","),
                    h.ai_title.as_deref().unwrap_or("")
                )?;
            }
        }
        Format::Ids => {
            for h in hits {
                writeln!(out, "{}", h.session_id)?;
            }
        }
    }
    Ok(())
}

fn format_mtime(mtime: i64) -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    match OffsetDateTime::from_unix_timestamp(mtime) {
        Ok(t) => t.format(&Rfc3339).unwrap_or_else(|_| mtime.to_string()),
        Err(_) => mtime.to_string(),
    }
}

fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 86_400 * 7,
        _ => return None,
    };
    Some(n * mult)
}

#[derive(Default)]
struct StderrProgress {
    last_pct: std::sync::atomic::AtomicU32,
}
impl Progress for StderrProgress {
    fn on_total(&self, total: u32) {
        eprintln!("scanning {} files", total);
    }
    fn on_file(&self, done: u32, total: u32, _current: &std::path::Path) {
        if total == 0 {
            return;
        }
        let pct = (done * 100 / total).min(100);
        let last = self.last_pct.load(std::sync::atomic::Ordering::Relaxed);
        if pct >= last + 5 {
            eprintln!("scanning {}/{} ({}%)", done, total, pct);
            self.last_pct
                .store(pct, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn on_done(&self, s: &IngestStats) {
        eprintln!(
            "done: upserted={} deleted={} total={}",
            s.upserted, s.deleted, s.total
        );
    }
}

struct JsonProgress;
impl Progress for JsonProgress {
    fn on_total(&self, total: u32) {
        let _ = writeln!(
            io::stderr(),
            "{}",
            serde_json::json!({"event":"start","total":total})
        );
    }
    fn on_file(&self, done: u32, total: u32, current: &std::path::Path) {
        let _ = writeln!(
            io::stderr(),
            "{}",
            serde_json::json!({
                "event":"file","done":done,"total":total,
                "path":current.to_string_lossy()
            })
        );
    }
    fn on_done(&self, s: &IngestStats) {
        let _ = writeln!(
            io::stderr(),
            "{}",
            serde_json::json!({
                "event":"done","upserted":s.upserted,"deleted":s.deleted,"total":s.total
            })
        );
    }
}
