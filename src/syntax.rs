//! Phase 2 — Tree-sitter integration.
//!
//! Today this module only powers syntax highlighting; the parser machinery
//! it sets up is also the foundation `ast.query` / `edit.replace_node`
//! (DESIGN.md §Edits, tier 2) will sit on later in the phase.
//!
//! The flow:
//!   App::apply mutates Buffer -> Buffer::version bumps -> App calls
//!   Syntax::refresh, which re-runs tree-sitter-highlight against the whole
//!   rope and caches per-line `HighlightSpan`s. `ui.rs` reads those spans
//!   to colorize the visible lines.

use std::path::Path;

use anyhow::{Context, Result};
use ratatui::style::{Color, Modifier, Style};
use ropey::Rope;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Highlight names we know how to style. The order is significant: the index
/// of a name here is what tree-sitter-highlight returns as the `Highlight` id
/// for every match against that capture, so `style_for` switches on this
/// table.
const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "function",
    "function.builtin",
    "function.macro",
    "keyword",
    "label",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "string.escape",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

#[derive(Clone, Copy, Debug)]
pub struct HighlightSpan {
    pub col_start: usize,
    pub col_end: usize,
    pub capture_idx: usize,
}

pub struct Syntax {
    highlighter: Highlighter,
    config: HighlightConfiguration,
    cached_version: Option<u64>,
    per_line: Vec<Vec<HighlightSpan>>,
}

impl Syntax {
    /// Pick a syntax based on the file extension. Returns `None` for files
    /// we don't have a grammar for; the renderer falls back to plain text.
    pub fn for_path(path: Option<&Path>) -> Option<Self> {
        let ext = path?.extension()?.to_str()?;
        match ext {
            "rs" => Self::rust().ok(),
            _ => None,
        }
    }

    fn rust() -> Result<Self> {
        let mut config = HighlightConfiguration::new(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
            "",
        )
        .context("building rust highlight configuration")?;
        config.configure(HIGHLIGHT_NAMES);
        Ok(Self {
            highlighter: Highlighter::new(),
            config,
            cached_version: None,
            per_line: Vec::new(),
        })
    }

    /// Re-run highlighting if the buffer has changed since the last refresh.
    pub fn refresh(&mut self, rope: &Rope, version: u64) {
        if self.cached_version == Some(version) {
            return;
        }
        self.cached_version = Some(version);
        self.per_line.clear();
        self.per_line.resize(rope.len_lines(), Vec::new());

        // Materialize the rope as a contiguous byte slice for the highlighter.
        // For Phase 2 we accept the per-keystroke copy; an incremental path
        // using `tree_sitter::Parser::parse_with` is a Phase-2.5 follow-up.
        let text: String = rope.to_string();
        let events = match self
            .highlighter
            .highlight(&self.config, text.as_bytes(), None, |_| None)
        {
            Ok(e) => e,
            Err(_) => return,
        };

        // We first drain the iterator into a flat list of (byte_start, byte_end,
        // capture_idx). We can't write into `self.per_line` directly because
        // `events` borrows `self.highlighter` mutably for the lifetime of the loop.
        let mut raw: Vec<(usize, usize, usize)> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let len_bytes = text.len();
        for ev in events.flatten() {
            match ev {
                HighlightEvent::HighlightStart(h) => stack.push(h.0),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let Some(&idx) = stack.last() else {
                        continue;
                    };
                    let end = end.min(len_bytes);
                    if start >= end {
                        continue;
                    }
                    raw.push((start, end, idx));
                }
            }
        }
        drop(stack);
        for (b0, b1, idx) in raw {
            split_into_lines(&mut self.per_line, rope, b0, b1, idx);
        }
    }

    pub fn line_spans(&self, line: usize) -> &[HighlightSpan] {
        self.per_line
            .get(line)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn split_into_lines(
    per_line: &mut [Vec<HighlightSpan>],
    rope: &Rope,
    b0: usize,
    b1: usize,
    capture_idx: usize,
) {
    let line0 = rope.byte_to_line(b0);
    let line1 = rope.byte_to_line(b1.min(rope.len_bytes()));
    for line in line0..=line1 {
        let Some(line_buf) = per_line.get_mut(line) else {
            break;
        };
        let line_byte_start = rope.line_to_byte(line);
        let line_byte_end = if line + 1 < rope.len_lines() {
            rope.line_to_byte(line + 1)
        } else {
            rope.len_bytes()
        };
        let chunk_start = b0.max(line_byte_start);
        let chunk_end = b1.min(line_byte_end);
        if chunk_start >= chunk_end {
            continue;
        }
        let line_char_start = rope.line_to_char(line);
        let col_start = rope.byte_to_char(chunk_start) - line_char_start;
        let col_end = rope.byte_to_char(chunk_end) - line_char_start;
        // Don't style the trailing newline — it isn't rendered as a
        // character anyway, and ratatui would treat it as a glyph.
        let col_end = col_end.min(printable_line_len(rope, line));
        if col_start >= col_end {
            continue;
        }
        line_buf.push(HighlightSpan {
            col_start,
            col_end,
            capture_idx,
        });
    }
}

fn printable_line_len(rope: &Rope, line: usize) -> usize {
    let s = rope.line(line);
    let n = s.len_chars();
    match s.get_char(n.saturating_sub(1)) {
        Some('\n') => n - 1,
        Some('\r') => n - 1,
        _ => n,
    }
}

pub fn style_for(capture_idx: usize) -> Style {
    let base = Style::default();
    let Some(name) = HIGHLIGHT_NAMES.get(capture_idx).copied() else {
        return base;
    };
    // Match on the primary capture name; ignore the dotted sub-category.
    let primary = name.split('.').next().unwrap_or("");
    match primary {
        "comment" => base.fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        "keyword" => base.fg(Color::Magenta).add_modifier(Modifier::BOLD),
        "string" => base.fg(Color::Green),
        "number" | "constant" => base.fg(Color::Yellow),
        "function" => base.fg(Color::Cyan),
        "type" => base.fg(Color::Blue),
        "attribute" => base.fg(Color::LightYellow),
        "property" => base.fg(Color::LightBlue),
        "tag" | "label" => base.fg(Color::Magenta),
        "punctuation" => base.fg(Color::Gray),
        _ => base,
    }
}
