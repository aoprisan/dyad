use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, GitFile, GitGroup, GitView, HistoryView, OpenFileView};
use crate::git::LineStatus;
use crate::syntax::{self, HighlightSpan};
use crate::theme;
use crate::tree::FileTree;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let content_area = chunks[0];
    let status_area = chunks[1];

    // Sidebar split: only when the tree is open. Keep the editor full
    // width otherwise so single-file workflows have no left margin.
    let (tree_rect, editor_area) = if app.tree.focused {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(TREE_WIDTH), Constraint::Min(1)])
            .split(content_area);
        (Some(split[0]), split[1])
    } else {
        (None, content_area)
    };

    let gutter_width = gutter_width(app.buffer.line_count());
    let content_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(gutter_width),
            Constraint::Min(1),
        ])
        .split(editor_area);
    let gutter_rect = content_split[0];
    let text_rect = content_split[1];

    if let Some(rect) = tree_rect {
        render_tree(frame, rect, &mut app.tree);
    }
    // Overlays take over the editor area (gutter + text) when open
    // so the user can scroll long content without fighting the syntax
    // viewport. Tree sidebar stays visible alongside if open.
    // Precedence: keys help > open-file > history > diff > normal.
    if app.keys_help {
        render_keys_help(frame, editor_area);
    } else if let Some(view) = app.open_file.as_ref() {
        render_open_file(frame, editor_area, view);
    } else if app.history.is_some() {
        render_history(frame, editor_area, app);
    } else if app.diff.is_some() {
        render_diff(frame, editor_area, app);
    } else {
        render_gutter(frame, gutter_rect, app, gutter_width);
        render_text(frame, text_rect, app);
    }
    render_status(frame, status_area, app);
    if !app.keys_help
        && app.open_file.is_none()
        && app.diff.is_none()
        && app.history.is_none()
        && !app.tree.focused
    {
        place_cursor(frame, text_rect, app);
    }
}

const TREE_WIDTH: u16 = 30;

fn render_tree(frame: &mut Frame, rect: Rect, tree: &mut FileTree) {
    let width = rect.width as usize;
    // First row is a header showing the project root's last segment so
    // the user can tell at a glance which folder they're browsing. The
    // remaining rows host the entry list.
    let list_viewport = (rect.height as usize).saturating_sub(1);
    tree.scroll_into_view(list_viewport);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rect.height as usize);
    let root_label = tree
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| tree.root.display().to_string());
    let header = truncate_to(&format!(" {root_label} "), width);
    lines.push(Line::from(Span::styled(header, theme::status_bar())));

    for r in 0..list_viewport {
        let idx = tree.top + r;
        let Some(entry) = tree.entries.get(idx) else {
            lines.push(Line::raw(""));
            continue;
        };
        let icon = if entry.is_parent_link {
            "↰ "
        } else if entry.is_dir {
            if entry.expanded { "▾ " } else { "▸ " }
        } else {
            "  "
        };
        let indent = "  ".repeat(entry.depth);
        let text = truncate_to(&format!("{indent}{icon}{}", entry.name), width);
        let style = if idx == tree.cursor {
            theme::tree_selected()
        } else if entry.is_dir {
            theme::tree_dir()
        } else {
            theme::tree()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    frame.render_widget(Paragraph::new(lines).style(theme::tree()), rect);
}

fn render_diff(frame: &mut Frame, rect: Rect, app: &App) {
    let Some(view) = app.diff.as_ref() else { return };
    // Two-pane horizontal layout: change-list on the left, diff on
    // the right. The change-list is sized to the longest entry's
    // rough width, clamped to keep the diff readable.
    let list_width = 36u16.min(rect.width.saturating_sub(20));
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(list_width), Constraint::Min(1)])
        .split(rect);
    render_git_files(frame, split[0], view);
    render_git_diff(frame, split[1], view);
}

fn render_git_files(frame: &mut Frame, rect: Rect, view: &GitView) {
    let width = rect.width as usize;
    let viewport = rect.height as usize;

    // Build the row list once (headers interleaved with file rows),
    // then page it against the cursor so the selection stays visible.
    enum Row<'a> {
        Header(GitGroup),
        File(&'a GitFile, usize), // file + its index in view.files
    }
    let mut rows: Vec<Row<'_>> = Vec::new();
    let mut last_group: Option<GitGroup> = None;
    for (i, f) in view.files.iter().enumerate() {
        if last_group != Some(f.group) {
            rows.push(Row::Header(f.group));
            last_group = Some(f.group);
        }
        rows.push(Row::File(f, i));
    }
    // Find the row index of the selected file so we can scroll into view.
    let cursor_row = rows
        .iter()
        .position(|r| matches!(r, Row::File(_, i) if *i == view.cursor))
        .unwrap_or(0);
    let top = scroll_top(cursor_row, viewport, rows.len());

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport);
    for r in 0..viewport {
        let idx = top + r;
        match rows.get(idx) {
            None => lines.push(Line::raw("")),
            Some(Row::Header(g)) => {
                let text = truncate_to(&format!(" {} ", g.header()), width);
                lines.push(Line::from(Span::styled(text, theme::status_hint())));
            }
            Some(Row::File(f, i)) => {
                let marker = format!("{}{}", f.staged, f.unstaged);
                let raw = format!(" {} {}", marker, f.path.display());
                let text = truncate_to(&raw, width);
                let style = if *i == view.cursor {
                    theme::tree_selected()
                } else {
                    git_status_style(f.staged, f.unstaged)
                };
                lines.push(Line::from(Span::styled(text, style)));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).style(theme::tree()), rect);
}

fn render_git_diff(frame: &mut Frame, rect: Rect, view: &GitView) {
    let viewport = rect.height as usize;
    let width = rect.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport);
    for r in 0..viewport {
        let idx = view.diff_scroll + r;
        let Some(raw) = view.diff_lines.get(idx) else {
            lines.push(Line::raw(""));
            continue;
        };
        let text = truncate_to(raw, width);
        let style = diff_line_style(raw);
        lines.push(Line::from(Span::styled(text, style)));
    }
    frame.render_widget(Paragraph::new(lines).style(theme::editor()), rect);
}

fn render_open_file(frame: &mut Frame, rect: Rect, view: &OpenFileView) {
    let viewport = rect.height as usize;
    let width = rect.width as usize;
    let top = scroll_top(view.cursor, viewport, view.matches.len());

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport);
    if view.matches.is_empty() {
        let msg = if view.query.is_empty() {
            "No files".to_string()
        } else {
            format!("No matches for \"{}\"", view.query)
        };
        lines.push(Line::from(Span::styled(
            truncate_to(&format!(" {msg}"), width),
            theme::status_hint(),
        )));
    } else {
        for r in 0..viewport {
            let row_idx = top + r;
            let Some(&file_idx) = view.matches.get(row_idx) else {
                lines.push(Line::raw(""));
                continue;
            };
            let Some(path) = view.files.get(file_idx) else {
                lines.push(Line::raw(""));
                continue;
            };
            let marker = if row_idx == view.cursor { "▸ " } else { "  " };
            let text = truncate_to(
                &format!("{marker}{}", path.display()),
                width,
            );
            let style = if row_idx == view.cursor {
                theme::tree_selected()
            } else {
                theme::editor()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }
    frame.render_widget(Paragraph::new(lines).style(theme::editor()), rect);
}

fn render_keys_help(frame: &mut Frame, rect: Rect) {
    let viewport = rect.height as usize;
    let width = rect.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport);
    let section = theme::status_hint();
    let body = theme::editor();
    for entry in KEYS_HELP {
        match *entry {
            KeyRow::Section(label) => {
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled(
                    truncate_to(&format!(" {label}"), width),
                    section,
                )));
            }
            KeyRow::Binding(keys, desc) => {
                let text = truncate_to(&format!("  {keys:<22}  {desc}"), width);
                lines.push(Line::from(Span::styled(text, body)));
            }
        }
        if lines.len() >= viewport {
            break;
        }
    }
    while lines.len() < viewport {
        lines.push(Line::raw(""));
    }
    frame.render_widget(Paragraph::new(lines).style(theme::editor()), rect);
}

enum KeyRow {
    Section(&'static str),
    Binding(&'static str, &'static str),
}

const KEYS_HELP: &[KeyRow] = &[
    KeyRow::Section("Modals"),
    KeyRow::Binding("Ctrl-T", "toggle file tree sidebar"),
    KeyRow::Binding("Ctrl-R", "git status / stage / commit"),
    KeyRow::Binding("Ctrl-L", "commit history"),
    KeyRow::Binding("Ctrl-P", "show this keymap"),
    KeyRow::Binding("Esc", "close modal / clear status"),
    KeyRow::Section("File"),
    KeyRow::Binding("Ctrl-S", "save"),
    KeyRow::Binding("Ctrl-X", "open file (fuzzy)"),
    KeyRow::Binding("Ctrl-N", "new file (prompt)"),
    KeyRow::Binding("Ctrl-W", "toggle autosave (~500ms idle)"),
    KeyRow::Binding("Ctrl-Q", "quit (twice when dirty)"),
    KeyRow::Section("LSP"),
    KeyRow::Binding("Ctrl-G", "go to definition"),
    KeyRow::Binding("Ctrl-O", "back (nav stack)"),
    KeyRow::Binding("Ctrl-K", "show type at cursor"),
    KeyRow::Binding("Ctrl-Y", "rename symbol"),
    KeyRow::Section("Motion"),
    KeyRow::Binding("Arrows / Alt+hjkl", "move by char / line"),
    KeyRow::Binding("Ctrl-B / Ctrl-F", "word left / right (Alt+b/f also)"),
    KeyRow::Binding("Ctrl-A / Ctrl-E", "line start / end"),
    KeyRow::Binding("Ctrl-U / Ctrl-D", "page up / down"),
    KeyRow::Binding("Home / End", "line start / end"),
    KeyRow::Section("Tree (when focused)"),
    KeyRow::Binding("Up / Down", "move selection"),
    KeyRow::Binding("Enter", "open file / expand dir / ascend ↰ .."),
    KeyRow::Section("Git view"),
    KeyRow::Binding("Up / Down", "move between files (refreshes diff)"),
    KeyRow::Binding("s / u / c", "stage / unstage / commit"),
    KeyRow::Binding("Ctrl-U / Ctrl-D", "page the diff"),
    KeyRow::Section("History view"),
    KeyRow::Binding("Up / Down", "move between commits"),
    KeyRow::Binding("Ctrl-U / Ctrl-D", "page the commit show"),
];

fn render_history(frame: &mut Frame, rect: Rect, app: &App) {
    let Some(view) = app.history.as_ref() else { return };
    let list_width = 48u16.min(rect.width.saturating_sub(20));
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(list_width), Constraint::Min(1)])
        .split(rect);
    render_history_list(frame, split[0], view);
    render_history_show(frame, split[1], view);
}

fn render_history_list(frame: &mut Frame, rect: Rect, view: &HistoryView) {
    let viewport = rect.height as usize;
    let width = rect.width as usize;
    let top = scroll_top(view.cursor, viewport, view.entries.len());

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport);
    for r in 0..viewport {
        let idx = top + r;
        let Some(entry) = view.entries.get(idx) else {
            lines.push(Line::raw(""));
            continue;
        };
        // `<short_sha> <date> <author> <subject>` — the author column
        // is clamped so long names don't crowd out the subject.
        let author = truncate_to(&entry.author, 10);
        let text = truncate_to(
            &format!(
                " {} {} {:<10} {}",
                entry.short_sha, entry.date, author, entry.subject
            ),
            width,
        );
        let style = if idx == view.cursor {
            theme::tree_selected()
        } else {
            theme::tree()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    frame.render_widget(Paragraph::new(lines).style(theme::tree()), rect);
}

fn render_history_show(frame: &mut Frame, rect: Rect, view: &HistoryView) {
    let viewport = rect.height as usize;
    let width = rect.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport);
    for r in 0..viewport {
        let idx = view.commit_scroll + r;
        let Some(raw) = view.commit_lines.get(idx) else {
            lines.push(Line::raw(""));
            continue;
        };
        let text = truncate_to(raw, width);
        // `git show` output mixes header lines ("commit ...", "Author: …",
        // "Date: …") with a diff body. Diff-line coloring applies to the
        // body; the header gets a muted accent so it doesn't compete.
        let style = if raw.starts_with("commit ")
            || raw.starts_with("Author:")
            || raw.starts_with("Date:")
            || raw.starts_with("Merge:")
        {
            theme::status_hint()
        } else {
            diff_line_style(raw)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    frame.render_widget(Paragraph::new(lines).style(theme::editor()), rect);
}

/// Pick a top-row offset so the selection sits comfortably inside
/// the viewport — same idea as `View::scroll_into_view` but for the
/// virtual row index that includes headers.
fn scroll_top(cursor_row: usize, viewport: usize, total_rows: usize) -> usize {
    if viewport == 0 || total_rows <= viewport {
        return 0;
    }
    if cursor_row < viewport {
        0
    } else {
        (cursor_row + 1).saturating_sub(viewport)
    }
}

/// Colour the file row by its working-tree state — added green,
/// modified yellow, deleted red, untracked muted. Mirrors the
/// gutter palette so the two views read consistently.
fn git_status_style(staged: char, unstaged: char) -> ratatui::style::Style {
    use ratatui::style::Style;
    let pick = if unstaged != ' ' { unstaged } else { staged };
    let base = Style::default();
    match pick {
        'A' => base.fg(theme::ok()),
        'M' => base.fg(theme::warn()),
        'D' => base.fg(theme::error()),
        '?' => base.fg(theme::diagnostic(Some(4))), // hint / violet
        _ => theme::tree(),
    }
}

/// Map a diff-line prefix to a Solarized color. Unprefixed context
/// lines render with the default editor style.
fn diff_line_style(line: &str) -> ratatui::style::Style {
    use ratatui::style::Style;
    let base = Style::default();
    if line.starts_with("@@") {
        base.fg(theme::diagnostic(Some(3))) // info / cyan
    } else if line.starts_with("+++") || line.starts_with("---") {
        base.fg(theme::diagnostic(Some(4))) // hint / violet — file headers
    } else if line.starts_with('+') {
        base.fg(theme::ok())
    } else if line.starts_with('-') {
        base.fg(theme::error())
    } else {
        theme::editor()
    }
}

fn truncate_to(s: &str, width: usize) -> String {
    if s.chars().count() > width {
        s.chars().take(width).collect()
    } else {
        s.to_string()
    }
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
    let number_style = theme::gutter();
    for r in 0..rows {
        let line_idx = app.view.top_line() + r;
        if line_idx >= total_lines {
            lines.push(Line::raw(""));
            continue;
        }
        let number = Span::styled(
            format!("{:>width$}", line_idx + 1, width = number_width),
            number_style,
        );
        let marker = git_marker_span(app, line_idx);
        lines.push(Line::from(vec![number, Span::raw(" "), marker]));
    }
    frame.render_widget(Paragraph::new(lines).style(theme::gutter()), rect);
}

fn git_marker_span(app: &App, line_idx: usize) -> Span<'static> {
    let bg = theme::gutter();
    match app.git_status.get(&line_idx) {
        Some(LineStatus::Added) => Span::styled("+", bg.fg(theme::ok())),
        Some(LineStatus::Modified) => Span::styled("~", bg.fg(theme::warn())),
        Some(LineStatus::DeletedAbove) => Span::styled("\u{203E}", bg.fg(theme::error())),
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
    frame.render_widget(Paragraph::new(lines).style(theme::editor()), rect);
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
    // Prompt and open-file dialog both take over the status bar —
    // they're the user's only input surface while active, so any
    // background text would just be noise.
    if let Some(prompt) = app.prompt.as_ref() {
        let bar = theme::status_bar();
        let text = format!(" {} {}_", prompt.label, prompt.buffer);
        let total_width = rect.width as usize;
        let pad = total_width.saturating_sub(text.chars().count());
        let mut spans = vec![Span::styled(text, bar)];
        spans.push(Span::styled(" ".repeat(pad), bar));
        frame.render_widget(Paragraph::new(Line::from(spans)), rect);
        return;
    }
    if let Some(view) = app.open_file.as_ref() {
        let bar = theme::status_bar();
        let hint_style = theme::status_hint();
        let left = format!(" Open file: {}_", view.query);
        let right = format!(" {} match(es) ", view.matches.len());
        let total_width = rect.width as usize;
        let pad = total_width
            .saturating_sub(left.chars().count() + right.chars().count());
        let spans = vec![
            Span::styled(left, bar),
            Span::styled(" ".repeat(pad), bar),
            Span::styled(right, hint_style),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), rect);
        return;
    }
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
    let bar = theme::status_bar();
    let hint = theme::status_hint();

    // Right side precedence: transient status > current-line diagnostic > hints.
    let (right_text, right_style) = if !app.status.is_empty() {
        (format!(" {} ", app.status), bar)
    } else if let Some((label, severity)) = current_line_diagnostic(app) {
        let style = theme::status_bar().fg(theme::diagnostic(severity));
        (label, style)
    } else if app.keys_help {
        (KEYS_HINT_TEXT.to_string(), hint)
    } else if app.history.is_some() {
        (HISTORY_HINT_TEXT.to_string(), hint)
    } else if app.diff.is_some() {
        (DIFF_HINT_TEXT.to_string(), hint)
    } else if app.tree.focused {
        (TREE_HINT_TEXT.to_string(), hint)
    } else {
        (HINT_TEXT.to_string(), hint)
    };

    // LSP badge sits between the left segment and the padding. Hidden for
    // non-Rust files; green when alive; red when we tried and failed.
    let lsp_badge = lsp_badge_span(app);
    let autosave_badge = if app.autosave {
        Some(Span::styled(
            "auto ",
            theme::status_bar().fg(theme::ok()),
        ))
    } else {
        None
    };

    let total_width = rect.width as usize;
    let badge_len = lsp_badge.as_ref().map(|s| s.content.chars().count()).unwrap_or(0)
        + autosave_badge
            .as_ref()
            .map(|s| s.content.chars().count())
            .unwrap_or(0);
    let combined_len = left.chars().count() + badge_len + right_text.chars().count();
    let pad = total_width.saturating_sub(combined_len);
    let mut spans = vec![Span::styled(left, bar)];
    if let Some(badge) = autosave_badge {
        spans.push(badge);
    }
    if let Some(badge) = lsp_badge {
        spans.push(badge);
    }
    spans.push(Span::styled(" ".repeat(pad), bar));
    spans.push(Span::styled(right_text, right_style));
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// Build the LSP indicator span. `None` when the file isn't a candidate
/// for LSP (the user shouldn't be told something's missing that was never
/// going to be there).
fn lsp_badge_span(app: &App) -> Option<Span<'static>> {
    if !app.lsp_attempted {
        return None;
    }
    let (text, fg) = match app.active_lsp() {
        Some(client) if client.is_indexing() => {
            // Yellow while the server is loading the workspace.
            // Definition/diagnostic requests return empty in this window.
            ("lsp… ", theme::warn())
        }
        Some(_) => ("lsp ", theme::ok()),
        // Tried and failed — most often the server binary isn't on PATH.
        None => ("lsp! ", theme::error()),
    };
    Some(Span::styled(text, theme::status_bar().fg(fg)))
}

const HINT_TEXT: &str =
    " Ctrl-P keys · Ctrl-X open · Ctrl-T tree · Ctrl-K type · Ctrl-R git · Ctrl-S save ";

const KEYS_HINT_TEXT: &str = " Esc close · Ctrl-P toggle ";

const TREE_HINT_TEXT: &str =
    " ↑/↓ move · Enter open · Esc close · Ctrl-T toggle ";

const DIFF_HINT_TEXT: &str =
    " ↑/↓ file · s stage · u unstage · c commit · Ctrl-U/D page · Esc close ";

const HISTORY_HINT_TEXT: &str =
    " ↑/↓ commit · Ctrl-U/D page · Esc close · Ctrl-L toggle ";

/// First LSP diagnostic that starts on the cursor line, formatted for
/// the status bar. Returns `(text, severity)` so the caller can colorize.
fn current_line_diagnostic(app: &App) -> Option<(String, Option<u8>)> {
    let lsp = app.active_lsp()?;
    let uri = app.lsp_uri.as_ref()?;
    let cursor_line = app.view.cursor_line() as u32;
    let diag = lsp
        .diagnostics(uri)
        .into_iter()
        .find(|d| d.range.start.line == cursor_line)?;
    let tag = match diag.severity {
        Some(1) => "error",
        Some(2) => "warn",
        Some(3) => "info",
        _ => "hint",
    };
    // Single-line: rust-analyzer sometimes embeds newlines.
    let one_line = diag.message.replace('\n', " · ");
    Some((format!(" {tag}: {one_line} "), diag.severity))
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
