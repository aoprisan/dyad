//! Phase 2 — Tree-sitter integration.
//!
//! Owns the parser, the cached parse tree, and the compiled Rust highlights
//! query. `refresh` applies pending edits to the cached tree, reparses
//! incrementally, then runs the query against the new tree to produce
//! per-line highlight spans for the renderer. Same tree backs `ast.query`
//! and the byte-coordinate inputs to `edit.replace_node`.
//!
//! Highlighting runs as a manual `QueryCursor` pass with longest-span /
//! later-pattern precedence (Phase 2.5b — dropped the `tree-sitter-highlight`
//! crate so we own the only parse).

use std::path::Path;

use anyhow::{Context, Result};
use ratatui::style::Style;
use ropey::Rope;
use serde::Serialize;
use tree_sitter::{InputEdit, Language, Parser, Point, Query, QueryCursor, StreamingIterator, Tree};

use crate::buffer::{Buffer, Edit};

/// Highlight names we know how to style. `build_capture_map` matches each
/// query capture name to its longest prefix in this list, and the resulting
/// index is what `style_for` switches on.
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
#[derive(Clone, Debug, Serialize)]
pub struct AstMatch {
    pub capture: String,
    pub kind: String,
    pub byte_start: usize,
    pub byte_end: usize,
}

pub struct Syntax {
    parser: Parser,
    tree: Option<Tree>,
    language: Language,
    highlight_query: Query,
    /// Maps `highlight_query.capture_names()[i]` to its `HIGHLIGHT_NAMES`
    /// index, or `None` when the capture (e.g. a predicate parameter) is
    /// not a visible highlight.
    capture_map: Vec<Option<usize>>,
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
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .context("loading rust grammar into parser")?;
        let highlight_query = Query::new(&language, tree_sitter_rust::HIGHLIGHTS_QUERY)
            .context("compiling rust highlights query")?;
        let capture_map = build_capture_map(&highlight_query, HIGHLIGHT_NAMES);
        Ok(Self {
            parser,
            tree: None,
            language,
            highlight_query,
            capture_map,
            cached_version: None,
            per_line: Vec::new(),
        })
    }

    /// Re-parse the tree and re-run highlighting if the buffer has changed
    /// since the last refresh. Pending edits are applied to the cached tree
    /// first, so the reparse is incremental — tree-sitter only walks the
    /// changed region (DESIGN.md §Edits is implicit on this).
    pub fn refresh(&mut self, buffer: &mut Buffer) {
        let version = buffer.version();
        if self.cached_version == Some(version) {
            // Already in sync; still drain the buffer's edit queue so it
            // doesn't accumulate stale entries across no-op refreshes.
            let _ = buffer.drain_edits();
            return;
        }
        self.cached_version = Some(version);
        let edits = buffer.drain_edits();
        if let Some(tree) = self.tree.as_mut() {
            for e in &edits {
                tree.edit(&to_input_edit(e));
            }
        }

        let rope = buffer.rope();
        self.per_line.clear();
        self.per_line.resize(rope.len_lines(), Vec::new());

        // Materialize the rope as a contiguous byte slice. The parser
        // and the highlight query both consume it; we own the only parse.
        let text: String = rope.to_string();
        self.tree = self.parser.parse(text.as_bytes(), self.tree.as_ref());
        let Some(tree) = self.tree.as_ref() else {
            return;
        };

        // Collect every (pattern_index, byte_start, byte_end, highlight_idx).
        // We need to drain into an owned Vec so the QueryCursor borrow on
        // self.highlight_query ends before we touch self.per_line.
        let len_bytes = text.len();
        let mut collected: Vec<(usize, usize, usize, usize)> = Vec::new();
        {
            let mut cursor = QueryCursor::new();
            let mut iter = cursor.matches(&self.highlight_query, tree.root_node(), text.as_bytes());
            while let Some(m) = iter.next() {
                for cap in m.captures {
                    let Some(h_idx) = self
                        .capture_map
                        .get(cap.index as usize)
                        .copied()
                        .flatten()
                    else {
                        continue;
                    };
                    let s = cap.node.start_byte().min(len_bytes);
                    let e = cap.node.end_byte().min(len_bytes);
                    if s < e {
                        collected.push((m.pattern_index, s, e, h_idx));
                    }
                }
            }
        }

        // Paint precedence: outer (longest) spans first so inner ones
        // overwrite them, then later patterns override earlier ones on
        // ties. This mirrors what tree-sitter-highlight does with its
        // stack-of-active-captures, without the event-stream machinery.
        collected.sort_by_key(|&(pat, s, e, _)| (std::cmp::Reverse(e - s), pat));
        let mut painted: Vec<u32> = vec![u32::MAX; len_bytes];
        for &(_, s, e, h) in &collected {
            painted[s..e].fill(h as u32);
        }

        // Walk runs of equal capture index out of the painted buffer,
        // splitting each run into the per-line span list the renderer
        // consumes.
        let mut i = 0;
        while i < len_bytes {
            let v = painted[i];
            if v == u32::MAX {
                i += 1;
                continue;
            }
            let start = i;
            while i < len_bytes && painted[i] == v {
                i += 1;
            }
            split_into_lines(&mut self.per_line, rope, start, i, v as usize);
        }
    }

    pub fn line_spans(&self, line: usize) -> &[HighlightSpan] {
        self.per_line
            .get(line)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Drop the cached parse tree and force the next `refresh` to do a
    /// full reparse from scratch. Call after `Buffer::restore` (Phase 3
    /// rollback) — otherwise the incremental-reparse path would feed
    /// tree-sitter an old tree that doesn't match the restored rope.
    #[allow(dead_code)] // Phase 4: invoked from ProtocolState::tx_rollback.
    pub fn invalidate(&mut self) {
        self.tree = None;
        self.cached_version = None;
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
        let query = Query::new(&self.language, query_src)
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

/// For each capture name in `query`, find the longest entry in `recognized`
/// that is either equal to it or is a dotted prefix of it. The chosen
/// index is what `style_for` switches on; `None` means the capture is a
/// query-internal name (predicate parameters, injection scaffolding) and
/// should not be painted.
fn build_capture_map(query: &Query, recognized: &[&str]) -> Vec<Option<usize>> {
    query
        .capture_names()
        .iter()
        .map(|cap_name| {
            recognized
                .iter()
                .enumerate()
                .filter(|&(_, &rn)| {
                    *cap_name == rn
                        || (cap_name.len() > rn.len()
                            && cap_name.starts_with(rn)
                            && cap_name.as_bytes()[rn.len()] == b'.')
                })
                .max_by_key(|&(_, &rn)| rn.len())
                .map(|(i, _)| i)
        })
        .collect()
}

fn to_input_edit(e: &Edit) -> InputEdit {
    InputEdit {
        start_byte: e.start_byte,
        old_end_byte: e.old_end_byte,
        new_end_byte: e.new_end_byte,
        start_position: Point::new(e.start_row, e.start_col),
        old_end_position: Point::new(e.old_end_row, e.old_end_col),
        new_end_position: Point::new(e.new_end_row, e.new_end_col),
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
    let Some(name) = HIGHLIGHT_NAMES.get(capture_idx).copied() else {
        return Style::default();
    };
    // Match on the primary capture name; ignore the dotted sub-category.
    let primary = name.split('.').next().unwrap_or("");
    crate::theme::syntax(primary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_buffer(name: &str) -> Buffer {
        let path = std::env::temp_dir()
            .join(format!("dyad_test_{}_{}.rs", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        Buffer::open(path).unwrap()
    }

    fn slice(rope: &Rope, byte_start: usize, byte_end: usize) -> String {
        let cs = rope.byte_to_char(byte_start);
        let ce = rope.byte_to_char(byte_end);
        rope.slice(cs..ce).to_string()
    }

    #[test]
    fn ast_query_finds_function_names() {
        let mut buf = scratch_buffer("ast_query");
        buf.insert_str(0, "fn hello() {}\nfn world() {}\n");
        let mut syn = Syntax::rust().unwrap();
        syn.refresh(&mut buf);
        let matches = syn
            .ast_query(buf.rope(), "(function_item name: (identifier) @name)")
            .unwrap();
        let names: Vec<String> = matches
            .into_iter()
            .filter(|m| m.capture == "name")
            .map(|m| slice(buf.rope(), m.byte_start, m.byte_end))
            .collect();
        assert_eq!(names, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn replace_node_renames_function() {
        let mut buf = scratch_buffer("replace_node");
        buf.insert_str(0, "fn hello() {}\n");
        let mut syn = Syntax::rust().unwrap();
        syn.refresh(&mut buf);
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

    fn name_to_idx(name: &str) -> usize {
        HIGHLIGHT_NAMES.iter().position(|n| *n == name).unwrap()
    }

    /// Phase 2.5b: our homegrown highlight pass should mark the `fn`
    /// keyword and the function-name identifier with the right captures.
    #[test]
    fn highlight_spans_cover_keyword_and_function_name() {
        let mut buf = scratch_buffer("highlight");
        buf.insert_str(0, "fn hello() {}\n");
        let mut syn = Syntax::rust().unwrap();
        syn.refresh(&mut buf);

        let line0 = syn.line_spans(0);
        let keyword_idx = name_to_idx("keyword");
        let function_idx = name_to_idx("function");

        let fn_span = line0
            .iter()
            .find(|s| s.col_start == 0 && s.col_end == 2)
            .expect("`fn` keyword span");
        assert_eq!(fn_span.capture_idx, keyword_idx);

        let name_span = line0
            .iter()
            .find(|s| s.col_start == 3 && s.col_end == 8)
            .expect("`hello` function-name span");
        assert_eq!(name_span.capture_idx, function_idx);
    }

    /// After an incremental edit, ast_query should still see the new state.
    /// This exercises the Tree::edit + reparse-with-old-tree path.
    #[test]
    fn incremental_reparse_tracks_renames() {
        let mut buf = scratch_buffer("incremental");
        buf.insert_str(0, "fn hello() {}\n");
        let mut syn = Syntax::rust().unwrap();
        syn.refresh(&mut buf);

        // Drive an incremental refresh: rename `hello` -> `farewell`.
        let target = syn
            .ast_query(buf.rope(), "(function_item name: (identifier) @name)")
            .unwrap()
            .into_iter()
            .find(|m| m.capture == "name")
            .unwrap();
        buf.replace_node(target.byte_start..target.byte_end, "farewell");
        syn.refresh(&mut buf);

        let names: Vec<String> = syn
            .ast_query(buf.rope(), "(function_item name: (identifier) @name)")
            .unwrap()
            .into_iter()
            .filter(|m| m.capture == "name")
            .map(|m| slice(buf.rope(), m.byte_start, m.byte_end))
            .collect();
        assert_eq!(names, vec!["farewell".to_string()]);
    }
}
