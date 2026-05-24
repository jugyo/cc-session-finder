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
const RESULT_CURSOR_WIDTH: u16 = 2;

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

    let spans: Vec<Span> = vec![Span::raw(q)];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let p = Paragraph::new(Line::from(spans)).block(block.clone());
    f.render_widget(p, area);

    // Caret position.
    let cursor_col = state.editor.cursor_col();
    let cx = area.x + 1 + cursor_col; // inside the bordered box
    let cy = area.y + 1;
    f.set_cursor_position((cx.min(area.right().saturating_sub(2)), cy));
}

fn draw_results(f: &mut Frame, area: Rect, state: &AppState) {
    let inner = area;

    let viewport_height = inner.height as usize;
    if viewport_height == 0 {
        return;
    }

    let highlight_terms = highlight_terms(state.editor.query());
    let content_width = inner.width.saturating_sub(RESULT_CURSOR_WIDTH);
    let items: Vec<Vec<Line<'static>>> = state
        .results
        .iter()
        .map(|hit| render_result_lines(hit, content_width, state.explain, &highlight_terms))
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
            let paragraph = Paragraph::new(line);
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            f.render_widget(paragraph, row_area);
            y = y.saturating_add(1);
        }
    }
}

fn result_line_with_cursor(line: &Line<'static>, selected_first_line: bool) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    if selected_first_line {
        spans.push(Span::styled(
            "> ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw("  "));
    }
    spans.extend(line.spans.iter().cloned().map(|mut span| {
        if selected_first_line {
            span.style = span.style.fg(Color::Cyan);
        }
        span
    }));
    Line::from(spans)
}

fn render_result_lines(
    hit: &Hit,
    width: u16,
    explain: bool,
    highlight_terms: &[String],
) -> Vec<Line<'static>> {
    let mut lines = vec![
        title_line(hit, width),
        snippet_line(hit, width, highlight_terms),
    ];
    lines.push(metadata_line(hit, width));
    if explain {
        for line in score_breakdown_lines(hit, width) {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    lines.push(Line::from(Span::raw("")));
    lines
}

fn title_line(hit: &Hit, width: u16) -> Line<'static> {
    let title = display_title(hit);
    let label_prefix = labels_prefix(&hit.labels);
    let title = if label_prefix.is_empty() {
        title
    } else {
        format!("{label_prefix} {title}")
    };
    Line::from(Span::styled(
        truncate_to_width(&title, width),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

fn labels_prefix(labels: &[String]) -> String {
    labels
        .iter()
        .map(|label| format!("[{label}]"))
        .collect::<Vec<_>>()
        .join("")
}

fn snippet_line(hit: &Hit, width: u16, highlight_terms: &[String]) -> Line<'static> {
    let snippet = hit.snippet.as_deref().map(one_line).unwrap_or_default();
    Line::from(split_with_highlight(
        &truncate_to_width(&snippet, width),
        Style::default().fg(Color::Gray),
        highlight_terms,
    ))
}

fn metadata_line(hit: &Hit, width: u16) -> Line<'static> {
    let term_program = std::env::var("TERM_PROGRAM").ok();
    metadata_line_for_terminal(hit, width, term_program.as_deref())
}

#[derive(Debug)]
struct StatusPart {
    text: String,
    url: Option<String>,
}

fn metadata_line_for_terminal(hit: &Hit, width: u16, term_program: Option<&str>) -> Line<'static> {
    let mut parts = vec![StatusPart {
        text: format_age_status(hit.mtime),
        url: None,
    }];

    let tokens = hit
        .tokens_input
        .saturating_add(hit.tokens_output)
        .saturating_add(hit.tokens_cache_read)
        .saturating_add(hit.tokens_cache_create);
    if tokens > 0 {
        parts.push(StatusPart {
            text: format!("{} token", crate::relative_time::format_count(tokens)),
            url: None,
        });
    }

    if let Some(branch) = hit
        .git_branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
    {
        parts.push(StatusPart {
            text: branch.to_string(),
            url: None,
        });
    }

    parts.push(StatusPart {
        text: short_project(&hit.cwd),
        url: None,
    });
    if let Some(pr_number) = hit.pr_number {
        parts.push(StatusPart {
            text: format!("PR #{pr_number}"),
            url: hit.pr_url.clone(),
        });
    }
    parts.push(StatusPart {
        text: hit.session_id.clone(),
        url: None,
    });

    let text = parts
        .iter()
        .map(|part| part.text.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    let truncated = truncate_to_width(&text, width);
    if truncated != text || !supports_osc8_links(term_program) {
        return Line::from(Span::styled(
            truncated,
            Style::default().fg(Color::DarkGray),
        ));
    }

    let mut spans = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        }

        if let Some(url) = part.url.filter(|url| !url.is_empty()) {
            spans.push(Span::styled(
                osc8_link(&part.text, &url),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::UNDERLINED),
            ));
        } else {
            spans.push(Span::styled(
                part.text,
                Style::default().fg(Color::DarkGray),
            ));
        }
    }

    Line::from(spans)
}

fn supports_osc8_links(term_program: Option<&str>) -> bool {
    term_program.is_some_and(|program| program.eq_ignore_ascii_case("ghostty"))
}

fn osc8_link(label: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\")
}

fn format_age_status(mtime: i64) -> String {
    let age = crate::relative_time::format_age(mtime);
    if age == "now" {
        age
    } else {
        format!("{age} ago")
    }
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

fn score_breakdown_lines(hit: &Hit, width: u16) -> Vec<String> {
    let scores = &hit.scores;
    let Some(final_score) = scores.final_score else {
        return Vec::new();
    };
    let Some(relevance) = scores.relevance_score else {
        return Vec::new();
    };
    let Some(freshness) = scores.freshness_boost else {
        return Vec::new();
    };

    let mut lines = vec![fit_width(
        format!(
            "score {:.2} = relevance {:.2} * freshness {:.2}",
            final_score, relevance, freshness
        ),
        width,
    )];

    if let Some(message) = scores.message_weighted_score {
        let metadata = scores.metadata_score.unwrap_or(0.0);
        let match_count = scores.message_match_count.unwrap_or(0);
        lines.push(fit_width(
            format!(
                "relevance {:.2} = max(metadata {:.2}, message {:.2}) ({} hits)",
                relevance, metadata, message, match_count
            ),
            width,
        ));
    } else {
        let metadata = scores.metadata_score.unwrap_or(relevance);
        lines.push(fit_width(
            format!("relevance {:.2} = metadata {:.2}", relevance, metadata),
            width,
        ));
    }

    if let Some(keyword) = scores.keyword_score {
        let metadata = scores.metadata_score.unwrap_or(keyword);
        lines.push(fit_width(
            format!("metadata {:.2} = keyword {:.2}", metadata, keyword),
            width,
        ));
    }

    let recency = scores.recency.unwrap_or(0.0);
    lines.push(fit_width(
        format!("freshness {:.2}x (recency {:.2})", freshness, recency),
        width,
    ));

    lines
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
        assert_eq!(decorated.spans[1].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn unselected_result_line_reserves_cursor_space() {
        let line = Line::from(Span::raw("result"));

        let decorated = result_line_with_cursor(&line, false);

        assert_eq!(decorated.spans[0].content.as_ref(), "  ");
        assert_eq!(decorated.spans[1].content.as_ref(), "result");
    }

    #[test]
    fn item_uses_title_snippet_status_and_margin() {
        let hit = hit_with_snippet(Some("before [needle] after"), Some("user"));

        let lines = render_result_lines(&hit, 80, false, &[]);

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].spans[0].content.as_ref(), "[match] title");
        assert_eq!(lines[1].spans[0].content.as_ref(), "before [needle] after");
        assert!(lines[2].spans[0].content.as_ref().contains("ago"));
        assert_eq!(lines[3].spans[0].content.as_ref(), "");
    }

    #[test]
    fn title_line_renders_result_labels() {
        let mut hit = hit_with_scores(Scores::default());
        hit.labels = vec!["cwd".to_string(), "match".to_string()];

        let lines = render_result_lines(&hit, 80, false, &[]);

        assert_eq!(lines[0].spans[0].content.as_ref(), "[cwd][match] title");
    }

    #[test]
    fn status_labels_age_and_tokens() {
        let mut hit = hit_with_scores(Scores::default());
        hit.session_id = "session-123".to_string();
        hit.cwd = "/Users/jugyo/workspace/bringout/seci-server".to_string();
        hit.git_branch = Some("feat/google-meet-delete-handler".to_string());
        hit.pr_number = Some(2614);
        hit.tokens_input = 79_400;

        let lines = render_result_lines(&hit, 200, false, &[]);

        let status = lines[2].spans[0].content.as_ref();
        assert!(status.contains("ago"), "{status}");
        assert!(status.ends_with("PR #2614 · session-123"), "{status}");
        let parts: Vec<_> = status.split(" · ").collect();
        assert_eq!(
            &parts[1..],
            [
                "79.4k token",
                "feat/google-meet-delete-handler",
                "bringout/seci-server",
                "PR #2614",
                "session-123"
            ]
        );
    }

    #[test]
    fn status_links_pr_on_ghostty() {
        let mut hit = hit_with_scores(Scores::default());
        hit.session_id = "session-123".to_string();
        hit.cwd = "/repo/current".to_string();
        hit.pr_number = Some(2614);
        hit.pr_url = Some("https://github.com/owner/repo/pull/2614".to_string());

        let line = metadata_line_for_terminal(&hit, 200, Some("ghostty"));

        let rendered = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            rendered.contains(
                "\x1b]8;;https://github.com/owner/repo/pull/2614\x1b\\PR #2614\x1b]8;;\x1b\\"
            ),
            "{rendered:?}"
        );
    }

    #[test]
    fn snippet_is_one_line() {
        let snippet = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen";
        let hit = hit_with_snippet(Some(snippet), Some("user"));

        let lines = render_result_lines(&hit, 24, false, &[]);

        assert_eq!(lines.len(), 4);
        assert!(lines[1].spans[0].content.as_ref().ends_with('…'));
    }

    #[test]
    fn snippet_highlights_query_terms() {
        let hit = hit_with_snippet(Some("before Alpha then Beta after"), Some("user"));
        let terms = highlight_terms("alpha beta");

        let lines = render_result_lines(&hit, 80, false, &terms);

        assert_eq!(lines.len(), 4);
        let highlighted: Vec<_> = lines[1]
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

        let lines = render_result_lines(&hit, 80, false, &[]);

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].spans[0].content.as_ref(), "");
    }

    #[test]
    fn title_does_not_highlight_query_terms() {
        let hit = hit_with_scores(Scores::default());
        let terms = highlight_terms("title");

        let lines = render_result_lines(&hit, 80, false, &terms);
        assert_eq!(lines[0].spans[0].content.as_ref(), "[match] title");
        assert_ne!(lines[0].spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn explain_places_score_after_snippet() {
        let mut hit = hit_with_snippet(Some("body [needle] text"), Some("assistant"));
        hit.scores = Scores {
            keyword_score: Some(1.0),
            metadata_score: Some(1.0),
            recency: Some(0.5),
            freshness_boost: Some(1.5),
            relevance_score: Some(1.0),
            final_score: Some(1.5),
            ..Scores::default()
        };

        let lines = render_result_lines(&hit, 80, true, &[]);

        assert_eq!(lines.len(), 8);
        assert_eq!(lines[1].spans[0].content.as_ref(), "body [needle] text");
        assert_eq!(
            lines[3].spans[0].content.as_ref(),
            "score 1.50 = relevance 1.00 * freshness 1.50"
        );
    }

    #[test]
    fn snippet_line_is_clipped_to_width() {
        let hit = hit_with_snippet(Some("1234567890"), Some("user"));

        let lines = render_result_lines(&hit, 12, false, &[]);

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].spans[0].content.as_ref(), "1234567890");
    }

    #[test]
    fn explain_adds_score_line_when_scores_are_available() {
        let hit = hit_with_scores(Scores {
            keyword_score: Some(1.0),
            metadata_score: Some(1.0),
            recency: Some(0.5),
            freshness_boost: Some(1.5),
            relevance_score: Some(1.0),
            final_score: Some(1.5),
            ..Scores::default()
        });

        let lines = render_result_lines(&hit, 80, true, &[]);

        assert_eq!(lines.len(), 8);
        assert_eq!(
            lines[3].spans[0].content.as_ref(),
            "score 1.50 = relevance 1.00 * freshness 1.50"
        );
        assert_eq!(
            lines[4].spans[0].content.as_ref(),
            "relevance 1.00 = metadata 1.00"
        );
        assert_eq!(
            lines[5].spans[0].content.as_ref(),
            "metadata 1.00 = keyword 1.00"
        );
        assert_eq!(
            lines[6].spans[0].content.as_ref(),
            "freshness 1.50x (recency 0.50)"
        );
    }

    #[test]
    fn explain_adds_message_score_line_when_message_scores_are_available() {
        let hit = hit_with_scores(Scores {
            metadata_score: Some(2.5),
            message_weighted_score: Some(0.75),
            message_match_count: Some(2),
            recency: Some(0.25),
            freshness_boost: Some(1.25),
            relevance_score: Some(2.5),
            final_score: Some(3.125),
            ..Scores::default()
        });

        let lines = render_result_lines(&hit, 80, true, &[]);

        assert_eq!(lines.len(), 7);
        assert_eq!(
            lines[3].spans[0].content.as_ref(),
            "score 3.12 = relevance 2.50 * freshness 1.25"
        );
        assert_eq!(
            lines[4].spans[0].content.as_ref(),
            "relevance 2.50 = max(metadata 2.50, message 0.75) (2 hits)"
        );
    }

    #[test]
    fn explain_omits_score_line_when_scores_are_missing() {
        let hit = hit_with_scores(Scores::default());

        let lines = render_result_lines(&hit, 80, true, &[]);

        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn scroll_offset_keeps_multiline_selected_item_visible() {
        let heights = [1, 2, 2, 1];

        assert_eq!(result_scroll_offset(&heights, 2, 4), 1);
        assert_eq!(result_scroll_offset(&heights, 2, 3), 2);
    }
}
