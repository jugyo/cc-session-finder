//! Pre-TUI indexing phase. Animates a single-line spinner on stderr while the
//! background worker scans and embeds session files. Returns only after the
//! worker reports completion; the main TUI starts on a fully populated DB.

use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::index;
use crate::index::ingest::{IngestStats, Progress};

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Clone)]
enum Phase {
    Starting,
    Scanning { done: u32, total: u32 },
    Embedding { done: u32, total: u32 },
    Done,
}

struct ChanProgress(mpsc::Sender<Phase>);
impl Progress for ChanProgress {
    fn on_total(&self, total: u32) {
        let _ = self.0.send(Phase::Scanning { done: 0, total });
    }
    fn on_file(&self, done: u32, total: u32, _current: &Path) {
        if done.is_multiple_of(8) || done + 1 == total {
            let _ = self.0.send(Phase::Scanning { done, total });
        }
    }
    fn on_done(&self, _stats: &IngestStats) {}
}

pub fn run(reindex: bool, no_vector: bool) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Phase>();
    let do_embed = !no_vector;

    thread::spawn(move || {
        let mut worker_conn = match index::open() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("indexer open failed: {e}");
                let _ = tx.send(Phase::Done);
                return;
            }
        };
        let progress = ChanProgress(tx.clone());
        let _ = index::ingest::scan_and_update(&mut worker_conn, reindex, &progress);

        #[cfg(feature = "embed")]
        if do_embed {
            let tx2 = tx.clone();
            let _ = index::embed::refresh_embeddings(&mut worker_conn, 16, |done, total| {
                let _ = tx2.send(Phase::Embedding { done, total });
            });
        }
        #[cfg(not(feature = "embed"))]
        let _ = do_embed;
        let _ = tx.send(Phase::Done);
    });

    let mut current = Phase::Starting;
    let mut tick: usize = 0;
    let mut prev_width: usize = 0;
    let is_tty = atty::is(atty::Stream::Stderr);
    let mut err = io::stderr();

    loop {
        match rx.recv_timeout(Duration::from_millis(80)) {
            Ok(Phase::Done) => break,
            Ok(p) => current = p,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tick = tick.wrapping_add(1);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if is_tty {
            let msg = render(&current, tick);
            // Erase previous line, then write new.
            let _ = write!(err, "\r{:1$}\r{2}", "", prev_width, msg);
            let _ = err.flush();
            prev_width = unicode_width::UnicodeWidthStr::width(msg.as_str());
        }
    }
    if is_tty && prev_width > 0 {
        let _ = write!(err, "\r{:1$}\r", "", prev_width);
        let _ = err.flush();
    }
    Ok(())
}

fn render(p: &Phase, tick: usize) -> String {
    let spinner = SPINNER[tick % SPINNER.len()];
    match p {
        Phase::Starting => format!("{} starting…", spinner),
        Phase::Scanning { done, total } => format!("{} indexing {}/{}", spinner, done, total),
        Phase::Embedding { done, total } => format!("{} embedding {}/{}", spinner, done, total),
        Phase::Done => String::new(),
    }
}
