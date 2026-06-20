//! Non-interactive CLI subcommands.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::index;
use crate::index::ingest::{IngestStats, Progress};
use crate::index::search::{Hit, TimeRange};
use crate::sessions::{
    self, InefficientParams, InefficientSort, MessageOrder, MessagesParams, SearchParams,
};
use crate::{
    Format, IndexArgs, ListArgs, OrderArg, ResumeArgs, SearchArgs, SessionsCmd, ShowArgs, SortByArg,
};

#[derive(Serialize)]
struct OutputDoc<'a> {
    query: Option<&'a str>,
    cwd: Option<String>,
    results: &'a [Hit],
    stats: OutputStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    explain: Option<OutputExplain>,
}

#[derive(Serialize)]
struct OutputStats {
    total_sessions: i64,
    text_search_hits: usize,
    took_ms: u128,
}

#[derive(Serialize)]
struct OutputExplain {
    requested: bool,
    details: &'static str,
}

fn default_cwd() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

/// Normalize a `--cwd` value, falling back to the current directory.
fn effective_cwd(cwd: Option<PathBuf>) -> Option<PathBuf> {
    cwd.as_deref()
        .map(crate::paths::normalize_cwd_filter)
        .or_else(default_cwd)
}

fn maybe_update(reindex: bool, no_update: bool) -> Result<rusqlite::Connection> {
    let mut conn = index::open()?;
    if no_update && !reindex {
        return Ok(conn);
    }
    if let Err(err) =
        index::ingest::scan_and_update(&mut conn, reindex, &index::ingest::NoopProgress)
    {
        eprintln!("warning: index update failed; using existing index: {err:#}");
    }
    Ok(conn)
}

pub fn run_list(args: ListArgs, reindex: bool, explain: bool) -> Result<ExitCode> {
    let start = std::time::Instant::now();
    let conn = maybe_update(reindex, args.no_update)?;
    let cwd = effective_cwd(args.cwd);
    let time_range = parse_time_range(args.since.as_deref(), args.until.as_deref())?;
    let hits = index::search::list_with_time_range(
        &conn,
        cwd.as_deref(),
        args.cwd_only,
        time_range,
        args.limit,
    )?;
    write_results(
        &hits,
        None,
        cwd.as_deref(),
        args.format,
        start,
        &conn,
        explain,
    )?;
    Ok(ExitCode::SUCCESS)
}

pub fn run_search(args: SearchArgs, reindex: bool, explain: bool) -> Result<ExitCode> {
    let start = std::time::Instant::now();
    let conn = maybe_update(reindex, args.no_update)?;
    let cwd = effective_cwd(args.cwd);
    let time_range = parse_time_range(args.since.as_deref(), args.until.as_deref())?;

    let hits = index::search::text_search_with_time_range(
        &conn,
        &args.query,
        cwd.as_deref(),
        args.cwd_only,
        time_range,
        args.limit,
    )?;

    write_results(
        &hits,
        Some(&args.query),
        cwd.as_deref(),
        args.format,
        start,
        &conn,
        explain,
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
            stats.indexed, stats.upserted, stats.deleted, stats.total
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

pub fn run_sessions(cmd: SessionsCmd) -> Result<ExitCode> {
    match cmd {
        SessionsCmd::List(args) => {
            let conn = maybe_update(false, args.no_update)?;
            let time_range = parse_time_range(args.since.as_deref(), args.until.as_deref())?;
            let response = sessions::search_sessions(
                &conn,
                SearchParams {
                    query: None,
                    limit: Some(args.limit),
                    cursor: args.cursor,
                    cwd: resolve_cwd(args.cwd, args.cwd_only),
                    cwd_only: args.cwd_only,
                    time_range,
                },
            )?;
            print_json(&response)
        }
        SessionsCmd::Search(args) => {
            let conn = maybe_update(false, args.no_update)?;
            let time_range = parse_time_range(args.since.as_deref(), args.until.as_deref())?;
            let response = sessions::search_sessions(
                &conn,
                SearchParams {
                    query: args.query,
                    limit: Some(args.limit),
                    cursor: args.cursor,
                    cwd: resolve_cwd(args.cwd, args.cwd_only),
                    cwd_only: args.cwd_only,
                    time_range,
                },
            )?;
            print_json(&response)
        }
        SessionsCmd::Overview(args) => {
            let conn = index::open()?;
            match sessions::get_session_overview(&conn, &args.id)? {
                Some(response) => print_json(&response),
                None => session_not_found(&args.id),
            }
        }
        SessionsCmd::Messages(args) => {
            let conn = index::open()?;
            let params = MessagesParams {
                id: args.id.clone(),
                limit: Some(args.limit),
                order: match args.order {
                    OrderArg::Asc => MessageOrder::Asc,
                    OrderArg::Desc => MessageOrder::Desc,
                },
                after_message_index: args.after,
                before_message_index: args.before,
            };
            match sessions::get_session_messages(&conn, params)? {
                Some(response) => print_json(&response),
                None => session_not_found(&args.id),
            }
        }
        SessionsCmd::SearchMessages(args) => {
            let conn = index::open()?;
            match sessions::search_session_messages(&conn, &args.id, &args.query, Some(args.limit))?
            {
                Some(response) => print_json(&response),
                None => session_not_found(&args.id),
            }
        }
        SessionsCmd::Inefficient(args) => {
            let conn = maybe_update(false, args.no_update)?;
            let time_range = parse_time_range(args.since.as_deref(), None)?;
            let response = sessions::find_inefficient_sessions(
                &conn,
                InefficientParams {
                    since: time_range.since,
                    limit: Some(args.limit),
                    sort_by: match args.sort_by {
                        SortByArg::BillableTokens => InefficientSort::BillableTokens,
                        SortByArg::ErrorRate => InefficientSort::ErrorRate,
                        SortByArg::CacheReadRatio => InefficientSort::CacheReadRatio,
                    },
                },
            )?;
            print_json(&response)
        }
    }
}

fn resolve_cwd(cwd: Option<PathBuf>, cwd_only: bool) -> Option<PathBuf> {
    let normalized = cwd.as_deref().map(crate::paths::normalize_cwd_filter);
    if cwd_only {
        normalized.or_else(default_cwd)
    } else {
        normalized
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<ExitCode> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(ExitCode::SUCCESS)
}

fn session_not_found(id: &str) -> Result<ExitCode> {
    eprintln!("session not found: {id}");
    Ok(ExitCode::from(3))
}

fn write_results(
    hits: &[Hit],
    query: Option<&str>,
    cwd: Option<&std::path::Path>,
    fmt: Format,
    start: std::time::Instant,
    conn: &rusqlite::Connection,
    explain: bool,
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
                    text_search_hits: query.map(|_| hits.len()).unwrap_or(0),
                    took_ms: start.elapsed().as_millis(),
                },
                explain: explain.then_some(OutputExplain {
                    requested: true,
                    details:
                        "score breakdown fields are included in results[].scores when available",
                }),
            };
            serde_json::to_writer_pretty(&mut out, &doc)?;
            writeln!(out)?;
        }
        Format::Tsv => {
            // Keep TSV and IDs single-record-per-line even with --explain.
            for h in hits {
                writeln!(
                    out,
                    "{}\t{}\t{}",
                    h.session_id,
                    format_mtime(h.mtime),
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

pub(crate) fn parse_time_range(since: Option<&str>, until: Option<&str>) -> Result<TimeRange> {
    parse_time_range_at(since, until, current_unix_secs())
}

fn parse_time_range_at(since: Option<&str>, until: Option<&str>, now: i64) -> Result<TimeRange> {
    let range = TimeRange {
        since: since
            .map(|value| parse_time_bound(value, now, false))
            .transpose()?,
        until: until
            .map(|value| parse_time_bound(value, now, true))
            .transpose()?,
    };
    if let (Some(since), Some(until)) = (range.since, range.until) {
        if since > until {
            bail!("invalid time range: --since is later than --until");
        }
    }
    Ok(range)
}

fn parse_time_bound(value: &str, now: i64, end_of_day: bool) -> Result<i64> {
    let value = value.trim();
    if let Some(seconds) = parse_duration(value) {
        return Ok(now - seconds);
    }
    if let Ok(timestamp) = value.parse::<i64>() {
        return Ok(timestamp);
    }
    if let Ok(datetime) =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    {
        return Ok(datetime.unix_timestamp());
    }

    let date_format = time::macros::format_description!("[year]-[month]-[day]");
    if let Ok(date) = time::Date::parse(value, date_format) {
        let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let start = date.midnight().assume_offset(offset).unix_timestamp();
        if end_of_day {
            let next = date.next_day().unwrap_or(date).midnight();
            return Ok(next
                .assume_offset(offset)
                .unix_timestamp()
                .saturating_sub(1));
        }
        return Ok(start);
    }

    bail!(
        "invalid time value {value:?}; use a duration like 7d/24h, YYYY-MM-DD, RFC3339, or a Unix timestamp"
    )
}

fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relative_time_range() {
        let now = 1_000_000;
        let range = parse_time_range_at(Some("7d"), Some("1d"), now).unwrap();

        assert_eq!(range.since, Some(now - 7 * 86_400));
        assert_eq!(range.until, Some(now - 86_400));
    }

    #[test]
    fn parses_rfc3339_time_range() {
        let range = parse_time_range_at(
            Some("1970-01-02T00:00:00Z"),
            Some("1970-01-03T00:00:00Z"),
            0,
        )
        .unwrap();

        assert_eq!(range.since, Some(86_400));
        assert_eq!(range.until, Some(2 * 86_400));
    }

    #[test]
    fn rejects_inverted_time_range() {
        let err = parse_time_range_at(Some("1d"), Some("7d"), 1_000_000).unwrap_err();

        assert!(err.to_string().contains("--since is later than --until"));
    }
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
