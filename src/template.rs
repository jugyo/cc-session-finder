//! Row template — typed, internal representation of how a session is rendered
//! as a list of colored spans. Designed so that a future `--template` CLI
//! flag can parse a string and produce this same structure.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::index::search::Hit;

const TRIGRAM_MIN_LEN: usize = 3;

/// A renderable piece. Each variant produces one or more [`Span`]s.
#[derive(Debug, Clone)]
pub enum Part {
    /// Static text with a fixed style.
    Literal(&'static str, Style),
    /// A single-field substitution.
    Field {
        field: Field,
        style: Style,
        /// Maximum display width in columns; truncated with `…`.
        max_width: Option<u16>,
    },
    /// `Field::Labels` emits multiple spans (one per label, with its own color).
    Labels,
    /// Render `inner` only if `field` is non-empty for this hit.
    When { field: Field, inner: Vec<Part> },
    /// Consume the remaining width up to the soft cap for the row.
    Flex { field: Field, style: Style },
}

#[derive(Debug, Clone, Copy)]
pub enum Field {
    Age,
    Project,
    Title,
    /// First user prompt, collapsed to one line.
    PromptOneLine,
    /// `repo#NNN` (or `#NNN` if no repo).
    Pr,
    /// Branch (suppressed when it's `HEAD` or empty).
    Branch,
    /// `(worktree)` marker when the session lives in a `--claude-worktrees-*` dir.
    WorktreeTag,
    /// Not in `default_row_template`; reserved for a future `--template` flag.
    #[allow(dead_code)]
    SessionId,
    /// Total token consumption (input + output + cache_read + cache_create),
    /// formatted with k/M/B suffix.
    Tokens,
}

/// Render a row by walking `parts`, with `total_width` reserved for any
/// [`Part::Flex`] field.
pub fn render(parts: &[Part], hit: &Hit, total_width: u16, terms: &[String]) -> Vec<Span<'static>> {
    // First pass: render non-flex parts to find consumed width.
    let mut consumed: u16 = 0;
    let mut sketch: Vec<SpanOrFlex> = Vec::new();
    for p in parts {
        emit(p, hit, &mut sketch, &mut consumed, terms);
    }

    // Second pass: compute remaining for Flex fields and finalize spans.
    let flex_count = sketch
        .iter()
        .filter(|x| matches!(x, SpanOrFlex::Flex(..)))
        .count();
    let remaining = total_width.saturating_sub(consumed);
    let per_flex = if flex_count > 0 {
        remaining / flex_count as u16
    } else {
        0
    };

    let mut out: Vec<Span<'static>> = Vec::with_capacity(sketch.len());
    for item in sketch {
        match item {
            SpanOrFlex::Span(s) => out.push(s),
            SpanOrFlex::Flex(text, style, highlight) => {
                let trimmed = truncate_to_width(&text, per_flex);
                if highlight {
                    out.extend(split_with_highlight(&trimmed, style, terms));
                } else {
                    out.push(Span::styled(trimmed, style));
                }
            }
        }
    }
    out
}

enum SpanOrFlex {
    Span(Span<'static>),
    Flex(String, Style, bool),
}

fn emit(part: &Part, hit: &Hit, out: &mut Vec<SpanOrFlex>, consumed: &mut u16, terms: &[String]) {
    match part {
        Part::Literal(s, style) => {
            *consumed = consumed.saturating_add(UnicodeWidthStr::width(*s) as u16);
            out.push(SpanOrFlex::Span(Span::styled((*s).to_string(), *style)));
        }
        Part::Field {
            field,
            style,
            max_width,
        } => {
            let raw = render_field(*field, hit).unwrap_or_default();
            let trimmed = match max_width {
                Some(w) => truncate_to_width(&raw, *w),
                None => raw,
            };
            *consumed = consumed.saturating_add(UnicodeWidthStr::width(trimmed.as_str()) as u16);
            if is_highlighted_field(*field) {
                out.extend(
                    split_with_highlight(&trimmed, *style, terms)
                        .into_iter()
                        .map(SpanOrFlex::Span),
                );
            } else {
                out.push(SpanOrFlex::Span(Span::styled(trimmed, *style)));
            }
        }
        Part::Labels => {
            for lab in &hit.labels {
                let (text, color) = label_style(lab);
                *consumed = consumed.saturating_add(UnicodeWidthStr::width(text.as_str()) as u16);
                out.push(SpanOrFlex::Span(Span::styled(
                    text,
                    Style::default().fg(color),
                )));
            }
        }
        Part::When { field, inner } => {
            if render_field(*field, hit)
                .map(|s| !s.is_empty())
                .unwrap_or(false)
            {
                for p in inner {
                    emit(p, hit, out, consumed, terms);
                }
            }
        }
        Part::Flex { field, style } => {
            let raw = render_field(*field, hit).unwrap_or_default();
            out.push(SpanOrFlex::Flex(raw, *style, is_highlighted_field(*field)));
        }
    }
}

fn is_highlighted_field(field: Field) -> bool {
    matches!(field, Field::Title | Field::PromptOneLine)
}

fn render_field(field: Field, hit: &Hit) -> Option<String> {
    Some(match field {
        Field::Age => crate::relative_time::format_age(hit.mtime),
        Field::Project => short_project(&hit.cwd),
        Field::Title => hit.ai_title.clone().unwrap_or_default(),
        Field::PromptOneLine => {
            let p = hit.first_prompt.as_deref()?;
            // Collapse whitespace, take first non-empty line-ish segment.
            let mut s = String::new();
            for ch in p.chars() {
                let c = match ch {
                    '\n' | '\r' | '\t' => ' ',
                    other => other,
                };
                if c == ' ' && s.ends_with(' ') {
                    continue;
                }
                s.push(c);
            }
            s.trim().to_string()
        }
        Field::Pr => match (&hit.pr_repo, hit.pr_number) {
            (Some(repo), Some(n)) => format!("{}#{}", repo, n),
            (None, Some(n)) => format!("#{}", n),
            _ => return None,
        },
        Field::Branch => {
            let b = hit.git_branch.as_deref()?;
            if b.is_empty() || b == "HEAD" {
                return None;
            }
            b.to_string()
        }
        Field::WorktreeTag => {
            if hit.is_worktree {
                "(wt)".to_string()
            } else {
                return None;
            }
        }
        Field::SessionId => hit.session_id.clone(),
        Field::Tokens => {
            let total = hit
                .tokens_input
                .saturating_add(hit.tokens_output)
                .saturating_add(hit.tokens_cache_read)
                .saturating_add(hit.tokens_cache_create);
            if total == 0 {
                return None;
            }
            crate::relative_time::format_count(total)
        }
    })
}

fn label_style(label: &str) -> (String, Color) {
    match label {
        "cwd" => ("[cwd]".to_string(), Color::Green),
        "match" => ("[match]".to_string(), Color::Yellow),
        "recent" => ("[recent]".to_string(), Color::DarkGray),
        other => (format!("[{}]", other), Color::Gray),
    }
}

pub fn highlight_terms(query: &str) -> Vec<String> {
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

pub fn split_with_highlight(text: &str, base: Style, terms: &[String]) -> Vec<Span<'static>> {
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
            highlight_style(base),
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
            if !original_text.is_char_boundary(start) || !original_text.is_char_boundary(end) {
                continue;
            }
            ranges.push((start, end));
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

fn highlight_style(base: Style) -> Style {
    base.fg(Color::Yellow).add_modifier(Modifier::BOLD)
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

/// Truncate `s` to fit in `max_cols` display columns, appending `…` when cut.
pub fn truncate_to_width(s: &str, max_cols: u16) -> String {
    let max_cols = max_cols as usize;
    if max_cols == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max_cols {
        return s.to_string();
    }
    if max_cols == 1 {
        return "…".to_string();
    }
    let budget = max_cols - 1; // reserve for ellipsis
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

/// Default row layout used by the TUI:
///
/// ```text
/// [cwd][match]  3h  project/leaf  Title — first prompt …  repo#1234 (wt)
/// ```
pub fn default_row_template() -> Vec<Part> {
    let dim = Style::default().fg(Color::DarkGray);
    let muted = Style::default().fg(Color::Gray);
    let title = Style::default().add_modifier(Modifier::BOLD);
    let sep = Style::default().fg(Color::DarkGray);
    let age = Style::default().fg(Color::Blue);
    let project = Style::default().fg(Color::Magenta);

    vec![
        Part::Labels,
        Part::Literal("  ", Style::default()),
        Part::Field {
            field: Field::Age,
            style: age,
            max_width: Some(5),
        },
        Part::Literal("  ", Style::default()),
        Part::Field {
            field: Field::Tokens,
            style: dim,
            max_width: Some(7),
        },
        Part::Literal("  ", Style::default()),
        Part::Field {
            field: Field::Project,
            style: project,
            max_width: Some(28),
        },
        Part::Literal("  ", Style::default()),
        Part::Field {
            field: Field::Title,
            style: title,
            max_width: Some(40),
        },
        Part::When {
            field: Field::PromptOneLine,
            inner: vec![
                Part::Literal(" — ", sep),
                Part::Flex {
                    field: Field::PromptOneLine,
                    style: muted,
                },
            ],
        },
        Part::When {
            field: Field::Branch,
            inner: vec![
                Part::Literal("  ", Style::default()),
                Part::Field {
                    field: Field::Branch,
                    style: dim,
                    max_width: Some(20),
                },
            ],
        },
        Part::When {
            field: Field::Pr,
            inner: vec![
                Part::Literal("  ", Style::default()),
                Part::Field {
                    field: Field::Pr,
                    style: dim,
                    max_width: Some(30),
                },
            ],
        },
        Part::When {
            field: Field::WorktreeTag,
            inner: vec![
                Part::Literal(" ", Style::default()),
                Part::Field {
                    field: Field::WorktreeTag,
                    style: dim,
                    max_width: Some(6),
                },
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit() -> Hit {
        Hit {
            session_id: "s1".to_string(),
            ai_title: Some("Phase Search Title".to_string()),
            cwd: "/repo/project".to_string(),
            mtime: 0,
            msg_count: Some(1),
            first_prompt: Some("Prompt has Search term".to_string()),
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
            scores: crate::index::search::Scores::default(),
        }
    }

    #[test]
    fn highlight_terms_filters_short_tokens_and_lowercases() {
        assert_eq!(
            highlight_terms("ab Search SEARCH"),
            vec!["search".to_string()]
        );
        assert!(highlight_terms("  a  bc  ").is_empty());
    }

    #[test]
    fn highlight_terms_keeps_short_acronyms() {
        assert_eq!(
            highlight_terms("Recall AI ai UX ux A1 a1 12 to bc"),
            vec!["recall".to_string(), "ai".to_string(), "ux".to_string()]
        );
    }

    #[test]
    fn highlight_terms_splits_punctuation_inside_tokens() {
        assert_eq!(
            highlight_terms("cc-session-finder foo,bar"),
            vec![
                "cc-session-finder".to_string(),
                "session".to_string(),
                "finder".to_string(),
                "foo,bar".to_string(),
                "foo".to_string(),
                "bar".to_string()
            ]
        );
    }

    #[test]
    fn split_highlight_returns_single_span_without_match() {
        let spans = split_with_highlight("plain text", Style::default(), &["needle".to_string()]);

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "plain text");
    }

    #[test]
    fn split_highlight_matches_case_insensitively() {
        let spans = split_with_highlight("Find Search", Style::default(), &["search".to_string()]);

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content.as_ref(), "Search");
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn split_highlight_splits_multiple_matches() {
        let spans = split_with_highlight("one two one", Style::default(), &["one".to_string()]);
        let contents: Vec<_> = spans.iter().map(|span| span.content.as_ref()).collect();

        assert_eq!(contents, ["one", " two ", "one"]);
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(spans[2].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn split_highlight_splits_multiple_terms() {
        let spans = split_with_highlight(
            "alpha then beta",
            Style::default(),
            &["alpha".to_string(), "beta".to_string()],
        );
        let highlighted: Vec<_> = spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|span| span.content.as_ref())
            .collect();

        assert_eq!(highlighted, ["alpha", "beta"]);
    }

    #[test]
    fn split_highlight_matches_short_acronym_after_punctuation() {
        let terms = highlight_terms("Recall AI");
        let spans = split_with_highlight("Recall.AI", Style::default(), &terms);
        let highlighted: Vec<_> = spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|span| span.content.as_ref())
            .collect();

        assert_eq!(highlighted, ["Recall", "AI"]);
    }

    #[test]
    fn split_highlight_matches_short_terms_case_insensitively() {
        let terms = highlight_terms("recall ai");
        let spans = split_with_highlight("Recall.AI", Style::default(), &terms);
        let highlighted: Vec<_> = spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|span| span.content.as_ref())
            .collect();

        assert_eq!(highlighted, ["Recall", "AI"]);
    }

    #[test]
    fn split_highlight_prefers_earliest_overlap() {
        let spans = split_with_highlight(
            "abcdef",
            Style::default(),
            &["abc".to_string(), "bcd".to_string()],
        );
        let contents: Vec<_> = spans.iter().map(|span| span.content.as_ref()).collect();

        assert_eq!(contents, ["abc", "def"]);
    }

    #[test]
    fn split_highlight_does_not_panic_on_non_ascii() {
        let spans =
            split_with_highlight("日本語 Search", Style::default(), &["search".to_string()]);

        assert!(spans.iter().any(|span| span.content.as_ref() == "Search"));
    }

    #[test]
    fn render_highlights_title_and_prompt() {
        let parts = vec![
            Part::Field {
                field: Field::Title,
                style: Style::default(),
                max_width: None,
            },
            Part::Literal(" ", Style::default()),
            Part::Flex {
                field: Field::PromptOneLine,
                style: Style::default(),
            },
        ];

        let spans = render(&parts, &hit(), 80, &["search".to_string()]);

        assert!(spans
            .iter()
            .filter(|span| span.content.eq_ignore_ascii_case("search"))
            .all(|span| span.style.add_modifier.contains(Modifier::BOLD)));
    }
}
