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
        .map(|hit| render_result_lines(hit, &template_parts, inner.width, state.explain))
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
    explain: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(template::render(template_parts, hit, width))];
    if explain {
        if let Some(line) = score_breakdown_line(hit, width) {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    lines
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

fn score_breakdown_line(hit: &Hit, width: u16) -> Option<String> {
    let scores = &hit.scores;
    let final_score = scores.final_score?;
    let keyword = scores.keyword_score?;
    let cwd = scores.cwd_score?;
    let recency = scores.recency?;
    let line = format!(
        "  score {:.2} = keyword {:.2} + cwd {:.2} + recency {:.2}",
        final_score, keyword, cwd, recency
    );
    Some(fit_width(line, width))
}

fn fit_width(line: String, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 || line.chars().count() <= width {
        return line;
    }
    line.chars().take(width).collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::search::Scores;

    fn hit_with_scores(scores: Scores) -> Hit {
        Hit {
            session_id: "s1".to_string(),
            ai_title: Some("title".to_string()),
            cwd: "/repo".to_string(),
            mtime: 0,
            msg_count: Some(1),
            first_prompt: Some("prompt".to_string()),
            file_path: "/repo/session.jsonl".to_string(),
            git_branch: None,
            pr_number: None,
            pr_url: None,
            pr_repo: None,
            is_worktree: false,
            tokens_input: 0,
            tokens_output: 0,
            tokens_cache_read: 0,
            tokens_cache_create: 0,
            labels: vec!["match".to_string()],
            scores,
        }
    }

    #[test]
    fn explain_adds_score_line_when_scores_are_available() {
        let hit = hit_with_scores(Scores {
            keyword_score: Some(1.0),
            cwd_score: Some(2.0),
            recency: Some(0.5),
            final_score: Some(3.5),
            ..Scores::default()
        });

        let lines = render_result_lines(&hit, &template::default_row_template(), 80, true);

        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1].spans[0].content.as_ref(),
            "  score 3.50 = keyword 1.00 + cwd 2.00 + recency 0.50"
        );
    }

    #[test]
    fn explain_omits_score_line_when_scores_are_missing() {
        let hit = hit_with_scores(Scores::default());

        let lines = render_result_lines(&hit, &template::default_row_template(), 80, true);

        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn scroll_offset_keeps_multiline_selected_item_visible() {
        let heights = [1, 2, 2, 1];

        assert_eq!(result_scroll_offset(&heights, 2, 4), 1);
        assert_eq!(result_scroll_offset(&heights, 2, 3), 2);
    }
}
