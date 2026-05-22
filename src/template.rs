//! Row template — typed, internal representation of how a session is rendered
//! as a list of colored spans. Designed so that a future `--template` CLI
//! flag can parse a string and produce this same structure.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::index::search::Hit;

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
pub fn render(parts: &[Part], hit: &Hit, total_width: u16) -> Vec<Span<'static>> {
    // First pass: render non-flex parts to find consumed width.
    let mut consumed: u16 = 0;
    let mut sketch: Vec<SpanOrFlex> = Vec::new();
    for p in parts {
        emit(p, hit, &mut sketch, &mut consumed);
    }

    // Second pass: compute remaining for Flex fields and finalize spans.
    let flex_count = sketch
        .iter()
        .filter(|x| matches!(x, SpanOrFlex::Flex(_, _)))
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
            SpanOrFlex::Flex(text, style) => {
                let trimmed = truncate_to_width(&text, per_flex);
                out.push(Span::styled(trimmed, style));
            }
        }
    }
    out
}

enum SpanOrFlex {
    Span(Span<'static>),
    Flex(String, Style),
}

fn emit(part: &Part, hit: &Hit, out: &mut Vec<SpanOrFlex>, consumed: &mut u16) {
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
            out.push(SpanOrFlex::Span(Span::styled(trimmed, *style)));
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
                    emit(p, hit, out, consumed);
                }
            }
        }
        Part::Flex { field, style } => {
            let raw = render_field(*field, hit).unwrap_or_default();
            out.push(SpanOrFlex::Flex(raw, *style));
        }
    }
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
