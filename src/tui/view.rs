//! Ratatui rendering for the finder UI.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::app::AppState;
use crate::template;

pub fn draw(f: &mut Frame, state: &AppState, tick: usize) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // query box
            Constraint::Min(1),    // result list
            Constraint::Length(1), // hint / status bar
        ])
        .split(area);

    draw_query_box(f, chunks[0], state);
    draw_results(f, chunks[1], state);
    draw_status_bar(f, chunks[2], state, tick);
}

fn draw_query_box(f: &mut Frame, area: Rect, state: &AppState) {
    let q = state.editor.query();

    let spans: Vec<Span> = vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(q),
    ];

    let block = Block::default().borders(Borders::ALL).title("search");
    let p = Paragraph::new(Line::from(spans)).block(block.clone());
    f.render_widget(p, area);

    // Caret position.
    let cursor_col = state.editor.cursor_col();
    let prefix_w: u16 = 2; // "> "
    let cx = area.x + 1 + prefix_w + cursor_col; // inside the bordered box
    let cy = area.y + 1;
    f.set_cursor_position((cx.min(area.right().saturating_sub(2)), cy));
}

fn draw_results(f: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("sessions ({})", state.results.len()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible_rows = inner.height as usize;
    if visible_rows == 0 {
        return;
    }

    // Compute scroll offset so the selection is in view.
    let offset = if state.selected >= visible_rows {
        state.selected + 1 - visible_rows
    } else {
        0
    };

    let template_parts = template::default_row_template();

    for (i, hit) in state
        .results
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
    {
        let y = inner.y + (i - offset) as u16;
        let is_sel = i == state.selected;

        let spans = template::render(&template_parts, hit, inner.width);
        let style = if is_sel {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let line = Line::from(spans).style(style);
        let row_area = Rect::new(inner.x, y, inner.width, 1);
        let p = Paragraph::new(line);
        f.render_widget(p, row_area);
    }
}

fn draw_status_bar(f: &mut Frame, area: Rect, _state: &AppState, _tick: usize) {
    let hint = Line::from(vec![
        Span::styled("↑/↓", Style::default().fg(Color::Cyan)),
        Span::raw(" select  "),
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" resume  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" quit"),
    ]);
    let p = Paragraph::new(hint).style(Style::default().fg(Color::DarkGray));
    f.render_widget(p, area);
}
