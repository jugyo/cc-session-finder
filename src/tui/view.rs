//! Ratatui rendering for the finder UI.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::app::AppState;
use crate::index::search::Hit;

const TRIGRAM_MIN_LEN: usize = 3;
const SNIPPET_LINE_WIDTH: usize = 72;

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

    let highlight_terms = highlight_terms(state.editor.query());
    let content_width = inner.width;
    let items: Vec<Vec<Line<'static>>> = state
        .results
        .iter()
        .enumerate()
        .map(|(i, hit)| {
            let selected = i == state.selected;
            render_result_lines(
                hit,
                content_width,
                state.explain && selected,
                if selected { state.snippet_lines } else { 0 },
                &highlight_terms,
            )
        })
        .collect();
    let item_heights: Vec<usize> = items.iter().map(|lines| item_height(lines)).collect();
    let offset = result_scroll_offset(&item_heights, state.selected, viewport_height);

    let mut y = inner.y;

    for (i, lines) in items.iter().enumerate().skip(offset) {
        if y >= inner.bottom() {
            break;
        }
        let is_sel = i == state.selected;

        for (line_index, line) in lines.iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }

            let line = result_line_with_cursor(line, is_sel && line_index == 0);
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            f.render_widget(Paragraph::new(line), row_area);
            y = y.saturating_add(1);
        }
    }
}

fn result_line_with_cursor(line: &Line<'static>, selected_first_line: bool) -> Line<'static> {
    if !selected_first_line {
        return line.clone();
    }

    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(
        "> ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    spans.extend(line.spans.clone());
    Line::from(spans)
}

fn render_result_lines(
    hit: &Hit,
    width: u16,
    explain: bool,
    snippet_lines: usize,
    highlight_terms: &[String],
) -> Vec<Line<'static>> {
    let mut lines = vec![
        title_line(hit, width, highlight_terms),
        metadata_line(hit, width),
    ];
    for line in snippet_lines_for_hit(hit, width, snippet_lines, highlight_terms) {
        lines.push(line);
    }
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

fn title_line(hit: &Hit, width: u16, highlight_terms: &[String]) -> Line<'static> {
    let title = display_title(hit);
    Line::from(split_with_highlight(
        &truncate_to_width(&title, width.saturating_sub(2)),
        Style::default().add_modifier(Modifier::BOLD),
        highlight_terms,
    ))
}

fn metadata_line(hit: &Hit, width: u16) -> Line<'static> {
    let mut parts = vec![crate::relative_time::format_age(hit.mtime)];
    if let Some(branch) = hit
        .git_branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
    {
        parts.push(branch.to_string());
    }

    let tokens = hit
        .tokens_input
        .saturating_add(hit.tokens_output)
        .saturating_add(hit.tokens_cache_read)
        .saturating_add(hit.tokens_cache_create);
    if tokens > 0 {
        parts.push(crate::relative_time::format_count(tokens));
    }
    parts.push(short_project(&hit.cwd));

    Line::from(Span::styled(
        truncate_to_width(&format!("  {}", parts.join(" · ")), width),
        Style::default().fg(Color::DarkGray),
    ))
}

fn display_title(hit: &Hit) -> String {
    hit.ai_title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| hit.first_prompt.as_deref().map(one_line))
        .unwrap_or_else(|| hit.session_id.clone())
}

fn item_height(lines: &[Line<'static>]) -> usize {
    lines.len().max(1)
}

fn snippet_lines_for_hit(
    hit: &Hit,
    width: u16,
    max_lines: usize,
    highlight_terms: &[String],
) -> Vec<Line<'static>> {
    if max_lines == 0 {
        return Vec::new();
    }

    let Some(snippet) = hit.snippet.as_deref().map(str::trim) else {
        return Vec::new();
    };
    if snippet.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<Line<'static>> = wrap_snippet(&one_line(snippet), width, max_lines)
        .into_iter()
        .map(|line| {
            Line::from(split_with_highlight(
                &line,
                Style::default().fg(Color::Gray),
                highlight_terms,
            ))
        })
        .collect();
    while lines.len() < max_lines {
        lines.push(Line::from(Span::styled(
            "  ".to_string(),
            Style::default().fg(Color::Gray),
        )));
    }
    lines
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
    if let Some(message) = scores.message_weighted_score {
        let metadata = scores.metadata_score.unwrap_or(0.0);
        let count_bonus = scores.message_count_bonus.unwrap_or(0.0);
        let match_count = scores.message_match_count.unwrap_or(0);
        let line = format!(
            "  score {:.2} = metadata {:.2} + message {:.2} + count {:.2} ({} hits)",
            final_score, metadata, message, count_bonus, match_count
        );
        return Some(fit_width(line, width));
    }

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

fn truncate_to_width(s: &str, max_cols: u16) -> String {
    let max_cols = max_cols as usize;
    if max_cols == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max_cols {
        return s.to_string();
    }
    if max_cols == 1 {
        return "...".to_string();
    }
    let budget = max_cols - 1;
    let mut out = String::new();
    let mut w = 0usize;
    for g in s.graphemes(true) {
        let gw = UnicodeWidthStr::width(g);
        if w + gw > budget {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out.push('…');
    out
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn short_project(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.trim_start_matches('/').split('/').collect();
    let n = parts.len();
    if n >= 2 {
        format!("{}/{}", parts[n - 2], parts[n - 1])
    } else {
        cwd.to_string()
    }
}

fn highlight_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for (raw_index, raw) in query.split_whitespace().enumerate() {
        push_highlight_term(raw, raw_index > 0, &mut terms);

        let mut part = String::new();
        let mut part_index = 0usize;
        for ch in raw.chars() {
            if ch.is_alphanumeric() {
                part.push(ch);
            } else {
                push_highlight_term(&part, raw_index > 0 || part_index > 0, &mut terms);
                part.clear();
                part_index += 1;
            }
        }
        push_highlight_term(&part, raw_index > 0 || part_index > 0, &mut terms);
    }
    terms
}

fn push_highlight_term(term: &str, allow_short: bool, terms: &mut Vec<String>) {
    let trimmed = term.trim_matches(|ch: char| !ch.is_alphanumeric());
    if !is_highlightable_term(trimmed, allow_short) {
        return;
    }
    let term = trimmed.to_lowercase();
    if !terms.iter().any(|existing| existing == &term) {
        terms.push(term);
    }
}

fn is_highlightable_term(term: &str, allow_short: bool) -> bool {
    let char_count = term.chars().count();
    char_count >= TRIGRAM_MIN_LEN
        || (allow_short
            && char_count >= 2
            && term.chars().all(|ch| ch.is_ascii_alphabetic())
            && is_short_acronym(term))
}

fn is_short_acronym(term: &str) -> bool {
    matches!(
        term.to_ascii_lowercase().as_str(),
        "ai" | "ui" | "ux" | "ml" | "db" | "id" | "pr" | "ci" | "cd"
    )
}

fn split_with_highlight(text: &str, base: Style, terms: &[String]) -> Vec<Span<'static>> {
    if text.is_empty() || terms.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }

    let lower = text.to_lowercase();
    let mut spans = Vec::new();
    let mut pos = 0usize;
    for (start, end) in highlight_ranges(&lower, text, terms) {
        if start > pos {
            spans.push(Span::styled(text[pos..start].to_string(), base));
        }
        spans.push(Span::styled(
            text[start..end].to_string(),
            base.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        pos = end;
    }

    if pos < text.len() {
        spans.push(Span::styled(text[pos..].to_string(), base));
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base));
    }
    spans
}

fn highlight_ranges(
    lower_text: &str,
    original_text: &str,
    terms: &[String],
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for term in terms {
        if term.is_empty() {
            continue;
        }
        for (start, _) in lower_text.match_indices(term) {
            let end = start + term.len();
            if original_text.is_char_boundary(start) && original_text.is_char_boundary(end) {
                ranges.push((start, end));
            }
        }
    }
    ranges.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));

    let mut selected = Vec::new();
    let mut pos = 0usize;
    for (start, end) in ranges {
        if start < pos {
            continue;
        }
        selected.push((start, end));
        pos = end;
    }
    selected
}

fn wrap_snippet(s: &str, width: u16, max_lines: usize) -> Vec<String> {
    let available_width = usize::from(width);
    if available_width == 0 || max_lines == 0 {
        return Vec::new();
    }

    let content_width = available_width
        .saturating_sub(2)
        .clamp(1, SNIPPET_LINE_WIDTH);
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in s.split_whitespace() {
        let separator = usize::from(!current.is_empty());
        if !current.is_empty()
            && current.chars().count() + separator + word.chars().count() > content_width
        {
            lines.push(format!("  {}", current));
            if lines.len() == max_lines {
                return lines;
            }
            current = String::new();
        }

        if word.chars().count() > content_width {
            if !current.is_empty() {
                lines.push(format!("  {}", current));
                if lines.len() == max_lines {
                    return lines;
                }
            }
            let mut rest = word.to_string();
            while !rest.is_empty() && lines.len() < max_lines {
                let chunk: String = rest.chars().take(content_width).collect();
                rest = rest.chars().skip(content_width).collect();
                lines.push(format!("  {}", chunk));
            }
            current = String::new();
            if lines.len() == max_lines {
                return lines;
            }
            continue;
        }

        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }

    if !current.is_empty() && lines.len() < max_lines {
        lines.push(format!("  {}", current));
    }

    lines
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
            snippet: None,
            snippet_role: None,
            snippet_message_count: None,
            scores,
        }
    }

    fn hit_with_snippet(snippet: Option<&str>, role: Option<&str>) -> Hit {
        let mut hit = hit_with_scores(Scores::default());
        hit.snippet = snippet.map(str::to_string);
        hit.snippet_role = role.map(str::to_string);
        hit
    }

    #[test]
    fn selected_result_line_gets_cursor_prefix() {
        let line = Line::from(Span::raw("result"));

        let decorated = result_line_with_cursor(&line, true);

        assert_eq!(decorated.spans[0].content.as_ref(), "> ");
        assert_eq!(decorated.spans[1].content.as_ref(), "result");
        assert!(decorated.spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn unselected_result_line_is_not_indented() {
        let line = Line::from(Span::raw("result"));

        let decorated = result_line_with_cursor(&line, false);

        assert_eq!(decorated.spans.len(), 1);
        assert_eq!(decorated.spans[0].content.as_ref(), "result");
    }

    #[test]
    fn snippet_adds_two_lines_when_available() {
        let hit = hit_with_snippet(Some("before [needle] after"), Some("user"));

        let lines = render_result_lines(&hit, 80, false, 2, &[]);

        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[2].spans[0].content.as_ref(),
            "  before [needle] after"
        );
        assert_eq!(lines[3].spans[0].content.as_ref(), "  ");
    }

    #[test]
    fn snippet_wraps_to_two_lines_by_default() {
        let hit = hit_with_snippet(Some("one two three four five six seven"), Some("user"));

        let lines = render_result_lines(&hit, 18, false, 2, &[]);

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[2].spans[0].content.as_ref(), "  one two three");
        assert_eq!(lines[3].spans[0].content.as_ref(), "  four five six");
    }

    #[test]
    fn snippet_line_count_is_configurable() {
        let hit = hit_with_snippet(Some("one two three four five six seven"), Some("user"));

        let lines = render_result_lines(&hit, 18, false, 3, &[]);

        assert_eq!(lines.len(), 5);
        assert_eq!(lines[4].spans[0].content.as_ref(), "  seven");
    }

    #[test]
    fn snippet_wraps_even_when_terminal_is_wide() {
        let snippet = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen";
        let hit = hit_with_snippet(Some(snippet), Some("user"));

        let lines = render_result_lines(&hit, 200, false, 2, &[]);

        assert_eq!(lines.len(), 4);
        assert!(lines[2].spans[0].content.chars().count() <= SNIPPET_LINE_WIDTH + 2);
        assert!(lines[3].spans[0].content.chars().count() <= SNIPPET_LINE_WIDTH + 2);
    }

    #[test]
    fn snippet_lines_zero_hides_snippet() {
        let hit = hit_with_snippet(Some("before [needle] after"), Some("user"));

        let lines = render_result_lines(&hit, 80, false, 0, &[]);

        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn snippet_highlights_query_terms() {
        let hit = hit_with_snippet(Some("before Alpha then Beta after"), Some("user"));
        let terms = highlight_terms("alpha beta");

        let lines = render_result_lines(&hit, 80, false, 2, &terms);

        assert_eq!(lines.len(), 4);
        let highlighted: Vec<_> = lines[2]
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(highlighted, ["Alpha", "Beta"]);
    }

    #[test]
    fn snippet_is_omitted_when_missing() {
        let hit = hit_with_snippet(None, None);

        let lines = render_result_lines(&hit, 80, false, 2, &[]);

        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn explain_places_score_after_snippet() {
        let mut hit = hit_with_snippet(Some("body [needle] text"), Some("assistant"));
        hit.scores = Scores {
            keyword_score: Some(1.0),
            cwd_score: Some(2.0),
            recency: Some(0.5),
            final_score: Some(3.5),
            ..Scores::default()
        };

        let lines = render_result_lines(&hit, 80, true, 2, &[]);

        assert_eq!(lines.len(), 5);
        assert_eq!(lines[2].spans[0].content.as_ref(), "  body [needle] text");
        assert_eq!(lines[3].spans[0].content.as_ref(), "  ");
        assert_eq!(
            lines[4].spans[0].content.as_ref(),
            "  score 3.50 = keyword 1.00 + cwd 2.00 + recency 0.50"
        );
    }

    #[test]
    fn snippet_line_is_clipped_to_width() {
        let hit = hit_with_snippet(Some("1234567890"), Some("user"));

        let lines = render_result_lines(&hit, 12, false, 2, &[]);

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[2].spans[0].content.as_ref(), "  1234567890");
        assert_eq!(lines[3].spans[0].content.as_ref(), "  ");
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

        let lines = render_result_lines(&hit, 80, true, 2, &[]);

        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[2].spans[0].content.as_ref(),
            "  score 3.50 = keyword 1.00 + cwd 2.00 + recency 0.50"
        );
    }

    #[test]
    fn explain_adds_message_score_line_when_message_scores_are_available() {
        let hit = hit_with_scores(Scores {
            metadata_score: Some(2.5),
            message_weighted_score: Some(0.75),
            message_match_count: Some(2),
            message_count_bonus: Some(0.16),
            final_score: Some(3.41),
            ..Scores::default()
        });

        let lines = render_result_lines(&hit, 80, true, 2, &[]);

        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[2].spans[0].content.as_ref(),
            "  score 3.41 = metadata 2.50 + message 0.75 + count 0.16 (2 hits)"
        );
    }

    #[test]
    fn explain_omits_score_line_when_scores_are_missing() {
        let hit = hit_with_scores(Scores::default());

        let lines = render_result_lines(&hit, 80, true, 2, &[]);

        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn scroll_offset_keeps_multiline_selected_item_visible() {
        let heights = [1, 2, 2, 1];

        assert_eq!(result_scroll_offset(&heights, 2, 4), 1);
        assert_eq!(result_scroll_offset(&heights, 2, 3), 2);
    }
}
