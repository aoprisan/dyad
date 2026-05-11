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
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator, Tree};
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

/// A single match from `Syntax::ast_query`. Byte-coordinate against the
/// rope's current contents — pair it with `Buffer::version()` if you plan
/// to act on it (optimistic-concurrency, DESIGN.md §Buffers & views).
#[allow(dead_code)] // Phase 4: fields are consumed by the MCP `ast.query` handler.
#[derive(Clone, Debug)]
pub struct AstMatch {
    pub capture: String,
    pub kind: String,
    pub byte_start: usize,
    pub byte_end: usize,
}

pub struct Syntax {
    parser: Parser,
    tree: Option<Tree>,
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
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .context("loading rust grammar into parser")?;
        let mut config = HighlightConfiguration::new(
            language,
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
            "",
        )
        .context("building rust highlight configuration")?;
        config.configure(HIGHLIGHT_NAMES);
        Ok(Self {
            parser,
            tree: None,
            highlighter: Highlighter::new(),
            config,
            cached_version: None,
            per_line: Vec::new(),
        })
    }

    /// Re-parse the tree and re-run highlighting if the buffer has changed
    /// since the last refresh.
    pub fn refresh(&mut self, rope: &Rope, version: u64) {
        if self.cached_version == Some(version) {
            return;
        }
        self.cached_version = Some(version);
        self.per_line.clear();
        self.per_line.resize(rope.len_lines(), Vec::new());

        // Materialize the rope as a contiguous byte slice. We share it
        // between the parser and the highlighter; the per-keystroke copy
        // is the obvious Phase-2.5 optimization target.
        let text: String = rope.to_string();
        // Pass `None` as the old tree — incremental reparse via `Tree::edit`
        // is the Phase-2.5 follow-up.
        self.tree = self.parser.parse(text.as_bytes(), None);

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

    /// Run a tree-sitter query against the cached parse tree. Returns one
    /// `AstMatch` per capture across all matches (a multi-capture pattern
    /// yields multiple entries). Phase 4 will surface this as the
    /// `ast.query` MCP tool (DESIGN.md §Semantic queries).
    ///
    /// The `rope` argument must be the same content the tree was parsed
    /// from — callers typically use `App`'s syntax + buffer pair, where
    /// `App::apply` refreshes the tree after every mutation.
    #[allow(dead_code)] // Phase 4: exposed as `ast.query` over MCP.
    pub fn ast_query(&self, rope: &Rope, query_src: &str) -> Result<Vec<AstMatch>> {
        let tree = self.tree.as_ref().context("buffer has not been parsed")?;
        let query = Query::new(&self.config.language, query_src)
            .context("compiling tree-sitter query")?;
        let names = query.capture_names();
        let text = rope.to_string();
        let bytes = text.as_bytes();
        let mut cursor = QueryCursor::new();
        let mut iter = cursor.matches(&query, tree.root_node(), bytes);
        let mut out = Vec::new();
        while let Some(m) = iter.next() {
            for cap in m.captures {
                let node = cap.node;
                let name = names
                    .get(cap.index as usize)
                    .copied()
                    .unwrap_or("")
                    .to_string();
                out.push(AstMatch {
                    capture: name,
                    kind: node.kind().to_string(),
                    byte_start: node.start_byte(),
                    byte_end: node.end_byte(),
                });
            }
        }
        Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn slice(rope: &Rope, byte_start: usize, byte_end: usize) -> String {
        let cs = rope.byte_to_char(byte_start);
        let ce = rope.byte_to_char(byte_end);
        rope.slice(cs..ce).to_string()
    }

    #[test]
    fn ast_query_finds_function_names() {
        let mut syn = Syntax::rust().unwrap();
        let rope = Rope::from_str("fn hello() {}\nfn world() {}\n");
        syn.refresh(&rope, 1);
        let matches = syn
            .ast_query(&rope, "(function_item name: (identifier) @name)")
            .unwrap();
        let names: Vec<String> = matches
            .into_iter()
            .filter(|m| m.capture == "name")
            .map(|m| slice(&rope, m.byte_start, m.byte_end))
            .collect();
        assert_eq!(names, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn replace_node_renames_function() {
        // Use a non-existent path so Buffer::open returns an empty rope.
        let path = std::env::temp_dir()
            .join(format!("dyad_test_replace_node_{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut buf = Buffer::open(path).unwrap();
        buf.insert_str(0, "fn hello() {}\n");

        let mut syn = Syntax::rust().unwrap();
        syn.refresh(buf.rope(), buf.version());
        let matches = syn
            .ast_query(buf.rope(), "(function_item name: (identifier) @name)")
            .unwrap();
        let target = matches
            .into_iter()
            .find(|m| m.capture == "name")
            .expect("function name should match");

        let before_version = buf.version();
        buf.replace_node(target.byte_start..target.byte_end, "world");

        assert_eq!(buf.rope().to_string(), "fn world() {}\n");
        assert_ne!(buf.version(), before_version);
        assert!(buf.is_dirty());
    }
}
