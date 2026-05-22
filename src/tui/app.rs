//! TUI event loop. Indexing is handled in [`super::indexing`] before this
//! starts; here we only handle user input, search, and the (async) vector
//! search worker.

use std::io::{self, Stdout};
use std::process::ExitCode;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::index;
use crate::index::search::Hit;

use super::input::QueryEditor;
use super::view;

pub struct AppState {
    pub editor: QueryEditor,
    /// Latest keyword/recent results for the current query.
    pub keyword_results: Vec<Hit>,
    /// Cached vector-search results, keyed by the query they were produced
    /// for. Allows the merged view to survive a `keyword_results` refresh.
    pub vector_cache: Option<(String, Vec<Hit>)>,
    /// Final display list. Always == merge(keyword_results, vector_cache.1)
    /// when the cache matches the current query.
    pub results: Vec<Hit>,
    pub selected: usize,
    pub cwd: Option<std::path::PathBuf>,
    pub last_query_at: Option<Instant>,
    /// `Some(start)` while a semantic-search worker is in flight; cleared
    /// when its result arrives.
    pub vector_search_started: Option<Instant>,
}

impl AppState {
    fn new(initial_query: Option<String>) -> Self {
        Self {
            editor: QueryEditor::with_initial(initial_query.unwrap_or_default()),
            keyword_results: Vec::new(),
            vector_cache: None,
            results: Vec::new(),
            selected: 0,
            cwd: std::env::current_dir().ok(),
            last_query_at: None,
            vector_search_started: None,
        }
    }
}

/// Channel events flowing into the UI thread.
enum UiEvent {
    Term(Event),
    VectorResults { query: String, hits: Vec<Hit> },
}

pub fn run(initial_query: Option<String>, no_vector: bool) -> Result<ExitCode> {
    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let exit_code = run_loop(&mut terminal, initial_query, no_vector);

    // Tear down.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let code = exit_code?;
    Ok(code)
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    initial_query: Option<String>,
    no_vector: bool,
) -> Result<ExitCode> {
    let (tx, rx) = std_mpsc::channel::<UiEvent>();

    let conn = Arc::new(Mutex::new(index::open()?));

    // Spawn terminal input pump.
    {
        let tx = tx.clone();
        thread::spawn(move || loop {
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(UiEvent::Term(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        });
    }

    let mut state = AppState::new(initial_query);

    // Initial query: list newest (using whatever is already in the DB).
    refresh_results(&conn, &mut state)?;

    let mut spinner_tick: usize = 0;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut pending_query: Option<String> = None;
    // Token incremented for every keystroke; the in-flight vector worker
    // checks this to bail out when the query has moved on.
    let vector_token = Arc::new(std::sync::atomic::AtomicU64::new(0));

    loop {
        // Draw at most every ~16ms or on demand.
        if last_draw.elapsed() >= Duration::from_millis(16) {
            terminal.draw(|f| view::draw(f, &state, spinner_tick))?;
            last_draw = Instant::now();
        }

        // Wait for the next event, with a short timeout so the spinner ticks.
        let ev = rx.recv_timeout(Duration::from_millis(80));
        match ev {
            Ok(UiEvent::Term(ev)) => {
                if let Some(action) = handle_event(ev, &mut state)? {
                    match action {
                        Action::Exit(code) => return Ok(ExitCode::from(code)),
                        Action::Resume(hit) => {
                            // Restore terminal before exec.
                            disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                            terminal.show_cursor()?;
                            crate::launch::resume(&hit)?; // never returns on success
                            return Ok(ExitCode::from(1));
                        }
                        Action::QueryChanged(q) => {
                            pending_query = Some(q);
                            state.last_query_at = Some(Instant::now());
                        }
                    }
                }
            }
            Ok(UiEvent::VectorResults { query, hits }) => {
                // Only cache if the worker's query still matches what the
                // user has typed (otherwise it's stale).
                if query == state.editor.query() {
                    state.vector_cache = Some((query, hits));
                    // Same query, just merging in late-arriving vector hits —
                    // keep the user's current selection.
                    rebuild_results(&mut state, true);
                }
                state.vector_search_started = None;
            }
            Err(_) => {
                spinner_tick = spinner_tick.wrapping_add(1);
            }
        }

        // Debounce query updates: re-run search if no input for 30ms.
        if let Some(q) = pending_query.clone() {
            if state
                .last_query_at
                .map(|t| t.elapsed() >= Duration::from_millis(30))
                .unwrap_or(true)
            {
                pending_query = None;
                state.editor.set_query(q.clone());
                refresh_results(&conn, &mut state)?;

                // Kick off async vector search if enabled and query non-empty.
                if !no_vector && !q.trim().is_empty() {
                    let token = vector_token.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    let tx2 = tx.clone();
                    let token_ref = vector_token.clone();
                    let cwd = state.cwd.clone();
                    let q_owned = q.clone();
                    state.vector_search_started = Some(Instant::now());
                    thread::spawn(move || {
                        if token != token_ref.load(std::sync::atomic::Ordering::SeqCst) {
                            return;
                        }
                        if let Ok(hits) = vector_search_in_worker(&q_owned, cwd.as_deref()) {
                            // Only send if we are still the latest.
                            if token == token_ref.load(std::sync::atomic::Ordering::SeqCst) {
                                let _ = tx2.send(UiEvent::VectorResults {
                                    query: q_owned,
                                    hits,
                                });
                            }
                        }
                    });
                }
            }
        }
    }
}

#[cfg(feature = "embed")]
fn vector_search_in_worker(query: &str, cwd: Option<&std::path::Path>) -> Result<Vec<Hit>> {
    let conn = index::open()?;
    index::search::vector(&conn, query, cwd, false, 50)
}

#[cfg(not(feature = "embed"))]
fn vector_search_in_worker(_query: &str, _cwd: Option<&std::path::Path>) -> Result<Vec<Hit>> {
    Ok(Vec::new())
}

enum Action {
    Exit(u8),
    Resume(Box<Hit>),
    QueryChanged(String),
}

fn handle_event(ev: Event, state: &mut AppState) -> Result<Option<Action>> {
    if let Event::Key(k) = ev {
        // Always allow Esc / Ctrl-C.
        if k.code == KeyCode::Esc {
            return Ok(Some(Action::Exit(130)));
        }
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(Some(Action::Exit(130)));
        }

        match k.code {
            KeyCode::Up => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
                return Ok(None);
            }
            KeyCode::Down => {
                if state.selected + 1 < state.results.len() {
                    state.selected += 1;
                }
                return Ok(None);
            }
            KeyCode::Enter => {
                if let Some(h) = state.results.get(state.selected).cloned() {
                    return Ok(Some(Action::Resume(Box::new(h))));
                }
                return Ok(None);
            }
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
                return Ok(None);
            }
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                if state.selected + 1 < state.results.len() {
                    state.selected += 1;
                }
                return Ok(None);
            }
            _ => {
                // Otherwise: text editing.
                if let Some(new_q) = handle_editor_key(state, k) {
                    return Ok(Some(Action::QueryChanged(new_q)));
                }
            }
        }
    }
    Ok(None)
}

fn handle_editor_key(state: &mut AppState, k: KeyEvent) -> Option<String> {
    // Emacs/readline-style bindings (text-mutating ones return Some(q) so the
    // search is re-run; cursor-only moves return None).
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = k.code {
            match c {
                'a' => {
                    state.editor.move_home();
                    return None;
                }
                'e' => {
                    state.editor.move_end();
                    return None;
                }
                'b' => {
                    state.editor.move_left();
                    return None;
                }
                'f' => {
                    state.editor.move_right();
                    return None;
                }
                'd' => {
                    state.editor.delete_forward();
                    return Some(state.editor.query().to_string());
                }
                'h' => {
                    state.editor.backspace();
                    return Some(state.editor.query().to_string());
                }
                'k' => {
                    state.editor.kill_to_end();
                    return Some(state.editor.query().to_string());
                }
                'u' => {
                    state.editor.kill_to_start();
                    return Some(state.editor.query().to_string());
                }
                'w' => {
                    state.editor.kill_word_backward();
                    return Some(state.editor.query().to_string());
                }
                _ => return None,
            }
        }
        return None;
    }

    match k.code {
        KeyCode::Char(c) => {
            state.editor.insert(c);
            Some(state.editor.query().to_string())
        }
        KeyCode::Backspace => {
            state.editor.backspace();
            Some(state.editor.query().to_string())
        }
        KeyCode::Left => {
            state.editor.move_left();
            None
        }
        KeyCode::Right => {
            state.editor.move_right();
            None
        }
        KeyCode::Home => {
            state.editor.move_home();
            None
        }
        KeyCode::End => {
            state.editor.move_end();
            None
        }
        _ => None,
    }
}

fn refresh_results(conn: &Arc<Mutex<rusqlite::Connection>>, state: &mut AppState) -> Result<()> {
    let q = state.editor.query().to_string();
    let cwd = state.cwd.clone();
    let conn = conn.lock().expect("conn poisoned");
    state.keyword_results = if q.trim().is_empty() {
        index::search::list(&conn, cwd.as_deref(), false, None, 100)?
    } else {
        index::search::keyword(&conn, &q, cwd.as_deref(), false, 100)?
    };
    // Query changed → results are re-ranked, snap the cursor back to the top.
    rebuild_results(state, false);
    Ok(())
}

/// Recompute `state.results` from `keyword_results` + `vector_cache`. The
/// cache is only honored if it was built for the current query.
///
/// `preserve_selection`: when true, try to keep the cursor on the same
/// session by id (used when async vector hits merge into an existing query);
/// when false, reset to the top (used when the query itself just changed).
fn rebuild_results(state: &mut AppState, preserve_selection: bool) {
    let q = state.editor.query();
    let vec_hits: Vec<Hit> = match &state.vector_cache {
        Some((cached_q, hits)) if cached_q == q => hits.clone(),
        _ => Vec::new(),
    };

    let prev_selected_id = if preserve_selection {
        state
            .results
            .get(state.selected)
            .map(|h| h.session_id.clone())
    } else {
        None
    };

    state.results = crate::index::search::merge(state.keyword_results.clone(), vec_hits);

    if let Some(id) = prev_selected_id {
        if let Some(idx) = state.results.iter().position(|h| h.session_id == id) {
            state.selected = idx;
            return;
        }
    }
    state.selected = 0;
}
