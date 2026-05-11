use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::git::LineStatus;
use crate::syntax::{self, HighlightSpan};

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let content_area = chunks[0];
    let status_area = chunks[1];

    let gutter_width = gutter_width(app.buffer.line_count());
    let content_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(gutter_width),
            Constraint::Min(1),
        ])
        .split(content_area);
    let gutter_rect = content_split[0];
    let text_rect = content_split[1];

    render_gutter(frame, gutter_rect, app, gutter_width);
    render_text(frame, text_rect, app);
    render_status(frame, status_area, app);
    place_cursor(frame, text_rect, app);
}

fn gutter_width(line_count: usize) -> u16 {
    let digits = line_count.max(1).to_string().len() as u16;
    // <digits><space><1-char git status><space before text>
    digits + 2
}

fn render_gutter(frame: &mut Frame, rect: Rect, app: &App, gutter_width: u16) {
    let rows = rect.height as usize;
    let total_lines = app.buffer.line_count();
    let mut lines = Vec::with_capacity(rows);
    // Reserve the last column for the git status marker.
    let number_width = (gutter_width as usize).saturating_sub(2);
    let dim = Style::default().fg(Color::DarkGray);
    for r in 0..rows {
        let line_idx = app.view.top_line() + r;
        if line_idx >= total_lines {
            lines.push(Line::raw(""));
            continue;
        }
        let number = Span::styled(
            format!("{:>width$}", line_idx + 1, width = number_width),
            dim,
        );
        let marker = git_marker_span(app, line_idx);
        lines.push(Line::from(vec![number, Span::raw(" "), marker]));
    }
    frame.render_widget(Paragraph::new(lines), rect);
}

fn git_marker_span(app: &App, line_idx: usize) -> Span<'static> {
    match app.git_status.get(&line_idx) {
        Some(LineStatus::Added) => Span::styled("+", Style::default().fg(Color::Green)),
        Some(LineStatus::Modified) => Span::styled("~", Style::default().fg(Color::Yellow)),
        Some(LineStatus::DeletedAbove) => Span::styled("\u{203E}", Style::default().fg(Color::Red)),
        None => Span::raw(" "),
    }
}

fn render_text(frame: &mut Frame, rect: Rect, app: &App) {
    let rows = rect.height as usize;
    let width = rect.width as usize;
    let total_lines = app.buffer.line_count();
    let mut lines = Vec::with_capacity(rows);
    for r in 0..rows {
        let line_idx = app.view.top_line() + r;
        if line_idx >= total_lines {
            lines.push(Line::raw(""));
            continue;
        }
        let slice = app.buffer.line(line_idx);
        // Strip the trailing newline so it doesn't render as a control character.
        let mut text: String = slice.chars().collect();
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        // Truncate to viewport width (no horizontal scroll in Phase 1).
        if text.chars().count() > width {
            text = text.chars().take(width).collect();
        }
        let chars: Vec<char> = text.chars().collect();
        let spans = app
            .syntax
            .as_ref()
            .map(|s| s.line_spans(line_idx))
            .unwrap_or(&[]);
        lines.push(build_line(&chars, spans));
    }
    frame.render_widget(Paragraph::new(lines), rect);
}

/// Slice `chars` into ratatui spans, applying the style for each highlight.
/// `spans` is assumed non-overlapping and sorted by `col_start` (guaranteed
/// by tree-sitter-highlight's Source-event contract).
fn build_line(chars: &[char], spans: &[HighlightSpan]) -> Line<'static> {
    if spans.is_empty() {
        return Line::raw(chars.iter().collect::<String>());
    }
    let n = chars.len();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut i = 0usize;
    let mut si = 0usize;
    while i < n {
        // Discard spans that ended before our cursor.
        while si < spans.len() && spans[si].col_end <= i {
            si += 1;
        }
        // No more spans, or the next span is past the truncated line — flush rest unstyled.
        if si >= spans.len() || spans[si].col_start >= n {
            out.push(Span::raw(chars[i..n].iter().collect::<String>()));
            break;
        }
        let span = spans[si];
        // Gap before the next styled run.
        if i < span.col_start {
            let end = span.col_start.min(n);
            out.push(Span::raw(chars[i..end].iter().collect::<String>()));
            i = end;
            continue;
        }
        let end = span.col_end.min(n);
        let style = syntax::style_for(span.capture_idx);
        out.push(Span::styled(
            chars[i..end].iter().collect::<String>(),
            style,
        ));
        i = end;
    }
    Line::from(out)
}

fn render_status(frame: &mut Frame, rect: Rect, app: &App) {
    let path = match app.buffer.path() {
        Some(p) => p.display().to_string(),
        None => "[scratch]".into(),
    };
    let dirty = if app.buffer.is_dirty() { "+" } else { "·" };
    let left = format!(
        " {path}  L{line}:C{col}  {dirty} ",
        line = app.view.cursor_line() + 1,
        col = app.view.cursor_col() + 1,
    );
    let right = format!(" {} ", app.status);
    let bg = Style::default()
        .bg(Color::DarkGray)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    // Pad to fill the row.
    let total_width = rect.width as usize;
    let combined_len = left.chars().count() + right.chars().count();
    let pad = total_width.saturating_sub(combined_len);
    let line = format!("{left}{:pad$}{right}", "", pad = pad);
    frame.render_widget(Paragraph::new(line).style(bg), rect);
}

fn place_cursor(frame: &mut Frame, text_rect: Rect, app: &App) {
    let row = app.view.cursor_line().saturating_sub(app.view.top_line()) as u16;
    let col = app.view.cursor_col() as u16;
    // Clamp inside the visible viewport.
    if row < text_rect.height && col < text_rect.width {
        frame.set_cursor_position((text_rect.x + col, text_rect.y + row));
    }
}

pub fn text_viewport_rows(area: Rect) -> u16 {
    // The content area minus the 1-row status line.
    area.height.saturating_sub(1)
}
