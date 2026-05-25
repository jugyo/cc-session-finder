//! TUI event loop. Indexing is handled in [`super::indexing`] before this
//! starts; here we only handle user input and text search.

use std::io::{self, Stdout};
use std::process::ExitCode;
use std::sync::mpsc as std_mpsc;
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
    pub results: Vec<Hit>,
    pub selected: usize,
    pub cwd: Option<std::path::PathBuf>,
    pub last_query_at: Option<Instant>,
    pub explain: bool,
}

impl AppState {
    fn new(explain: bool) -> Self {
        Self {
            editor: QueryEditor::with_initial(String::new()),
            results: Vec::new(),
            selected: 0,
            cwd: std::env::current_dir().ok(),
            last_query_at: None,
            explain,
        }
    }
}

pub fn run(explain: bool) -> Result<ExitCode> {
    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let exit_code = run_loop(&mut terminal, explain);

    // Tear down.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let code = exit_code?;
    Ok(code)
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, explain: bool) -> Result<ExitCode> {
    let (tx, rx) = std_mpsc::channel::<Event>();

    let conn = index::open()?;

    // Spawn terminal input pump.
    {
        let tx = tx.clone();
        thread::spawn(move || loop {
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
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

    let mut state = AppState::new(explain);

    // Initial query: list newest (using whatever is already in the DB).
    refresh_results(&conn, &mut state)?;

    let mut spinner_tick: usize = 0;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut pending_query: Option<String> = None;

    loop {
        // Draw at most every ~16ms or on demand.
        if last_draw.elapsed() >= Duration::from_millis(16) {
            terminal.draw(|f| view::draw(f, &state, spinner_tick))?;
            last_draw = Instant::now();
        }

        // Wait for the next event, with a short timeout so the spinner ticks.
        let ev = rx.recv_timeout(Duration::from_millis(80));
        match ev {
            Ok(ev) => {
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
            Err(_) => {
                spinner_tick = spinner_tick.wrapping_add(1);
            }
        }

        refresh_pending_query_if_ready(&conn, &mut state, &mut pending_query)?;
    }
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

    if k.modifiers.contains(KeyModifiers::ALT) {
        match k.code {
            KeyCode::Left => {
                state.editor.move_word_left();
                return None;
            }
            KeyCode::Right => {
                state.editor.move_word_right();
                return None;
            }
            KeyCode::Char('b') => {
                state.editor.move_word_left();
                return None;
            }
            KeyCode::Char('f') => {
                state.editor.move_word_right();
                return None;
            }
            KeyCode::Backspace => {
                state.editor.kill_word_backward();
                return Some(state.editor.query().to_string());
            }
            _ => return None,
        }
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

fn refresh_results(conn: &rusqlite::Connection, state: &mut AppState) -> Result<()> {
    let q = state.editor.query().to_string();
    let cwd = state.cwd.clone();
    state.results = if q.trim().is_empty() {
        index::search::list(conn, cwd.as_deref(), false, None, 100)?
    } else {
        index::search::text_search(conn, &q, cwd.as_deref(), false, 100)?
    };
    state.selected = 0;
    Ok(())
}

fn refresh_pending_query_if_ready(
    conn: &rusqlite::Connection,
    state: &mut AppState,
    pending_query: &mut Option<String>,
) -> Result<()> {
    if pending_query.is_some()
        && state
            .last_query_at
            .map(|t| t.elapsed() >= Duration::from_millis(30))
            .unwrap_or(true)
    {
        *pending_query = None;
        refresh_results(conn, state)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_query(query: &str) -> AppState {
        let mut state = AppState::new(false);
        state.editor = QueryEditor::with_initial(query.to_string());
        state
    }

    fn open_indexed_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        crate::index::schema::ensure(&conn).expect("schema");
        conn
    }

    #[test]
    fn debounced_search_preserves_query_cursor() {
        let conn = open_indexed_db();
        let mut state = state_with_query("abcd");
        state.editor.move_left();
        state.editor.move_left();
        let cursor_col = state.editor.cursor_col();
        let mut pending_query = Some(state.editor.query().to_string());
        state.last_query_at = Some(Instant::now() - Duration::from_millis(31));

        refresh_pending_query_if_ready(&conn, &mut state, &mut pending_query).unwrap();

        assert_eq!(pending_query, None);
        assert_eq!(state.editor.query(), "abcd");
        assert_eq!(state.editor.cursor_col(), cursor_col);
    }

    #[test]
    fn alt_arrows_move_query_cursor_by_word() {
        let mut state = state_with_query("alpha beta gamma");

        assert_eq!(
            handle_editor_key(&mut state, KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            None
        );
        assert_eq!(state.editor.cursor_col(), 11);

        assert_eq!(
            handle_editor_key(&mut state, KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
            None
        );
        assert_eq!(state.editor.cursor_col(), 16);
    }

    #[test]
    fn alt_b_and_f_move_query_cursor_by_word() {
        let mut state = state_with_query("alpha beta gamma");

        assert_eq!(
            handle_editor_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)
            ),
            None
        );
        assert_eq!(state.editor.cursor_col(), 11);

        assert_eq!(
            handle_editor_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT)
            ),
            None
        );
        assert_eq!(state.editor.cursor_col(), 16);
    }

    #[test]
    fn alt_backspace_deletes_previous_word() {
        let mut state = state_with_query("alpha beta  gamma");

        assert_eq!(
            handle_editor_key(
                &mut state,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)
            ),
            Some("alpha beta  ".to_string())
        );
        assert_eq!(state.editor.cursor_col(), 12);
    }
}
