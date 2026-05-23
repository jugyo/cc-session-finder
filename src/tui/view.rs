//! Ratatui rendering for the finder UI.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::app::AppState;
use crate::index::search::Hit;
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

    let viewport_height = inner.height as usize;
    if viewport_height == 0 {
        return;
    }

    let template_parts = template::default_row_template();
    let items: Vec<Vec<Line<'static>>> = state
        .results
        .iter()
        .map(|hit| render_result_lines(hit, &template_parts, inner.width))
        .collect();
    let item_heights: Vec<usize> = items.iter().map(|lines| item_height(lines)).collect();
    let offset = result_scroll_offset(&item_heights, state.selected, viewport_height);

    let selected_style = Style::default().add_modifier(Modifier::REVERSED);
    let mut y = inner.y;

    for (i, lines) in items.iter().enumerate().skip(offset) {
        if y >= inner.bottom() {
            break;
        }
        let is_sel = i == state.selected;

        for line in lines {
            if y >= inner.bottom() {
                break;
            }

            let line = if is_sel {
                line.clone().style(selected_style)
            } else {
                line.clone()
            };
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            f.render_widget(Paragraph::new(line), row_area);
            y = y.saturating_add(1);
        }
    }
}

fn render_result_lines(
    hit: &Hit,
    template_parts: &[template::Part],
    width: u16,
) -> Vec<Line<'static>> {
    vec![Line::from(template::render(template_parts, hit, width))]
}

fn item_height(lines: &[Line<'static>]) -> usize {
    lines.len().max(1)
}

fn result_scroll_offset(item_heights: &[usize], selected: usize, viewport_height: usize) -> usize {
    if item_heights.is_empty() {
        return 0;
    }

    let selected = selected.min(item_heights.len() - 1);
    let mut offset = selected;
    let mut used = item_heights[selected];

    while offset > 0 && used.saturating_add(item_heights[offset - 1]) <= viewport_height {
        offset -= 1;
        used = used.saturating_add(item_heights[offset]);
    }

    offset
}

fn draw_status_bar(f: &mut Frame, area: Rect, state: &AppState, _tick: usize) {
    let mut spans = vec![
        Span::styled("↑/↓", Style::default().fg(Color::Cyan)),
        Span::raw(" select  "),
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" resume  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" quit"),
    ];

    if state.explain {
        spans.extend([
            Span::raw("  "),
            Span::styled("explain", Style::default().fg(Color::Cyan)),
            Span::raw(" requested"),
        ]);
    }

    let hint = Line::from(spans);
    let p = Paragraph::new(hint).style(Style::default().fg(Color::DarkGray));
    f.render_widget(p, area);
}
