use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ropey::{Rope, RopeSlice};

/// A summary of a single mutation, in tree-sitter-compatible coordinates
/// (byte offsets + zero-based row/column points). `Syntax::refresh` feeds
/// these into `Tree::edit` so the next reparse can be incremental.
///
/// Kept tree-sitter-agnostic on purpose — Buffer doesn't depend on
/// tree-sitter; the syntax layer translates `Edit` into `InputEdit`.
#[derive(Clone, Copy, Debug)]
pub struct Edit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub old_end_row: usize,
    pub old_end_col: usize,
    pub new_end_row: usize,
    pub new_end_col: usize,
}

pub struct Buffer {
    rope: Rope,
    path: Option<PathBuf>,
    version: u64,
    dirty: bool,
    pending_edits: Vec<Edit>,
}

/// Frozen Buffer state captured at `Buffer::snapshot` time. Phase 3 stores
/// one of these inside each active transaction so `tx.rollback` can put
/// the buffer back to where it was when the transaction began.
///
/// Fields stay private — the only legal way to apply a snapshot is via
/// `Buffer::restore`, which also bumps the version so any downstream
/// cache (e.g. the syntax tree) invalidates.
#[derive(Clone, Debug)]
pub struct BufferSnapshot {
    rope: Rope,
    version: u64,
    dirty: bool,
    pending_edits: Vec<Edit>,
}

impl Buffer {
    pub fn open(path: PathBuf) -> Result<Self> {
        let rope = match File::open(&path) {
            Ok(file) => Rope::from_reader(BufReader::new(file))
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Rope::new(),
            Err(e) => {
                return Err(e).with_context(|| format!("opening {}", path.display()));
            }
        };
        Ok(Self {
            rope,
            path: Some(path),
            version: 0,
            dirty: false,
            pending_edits: Vec::new(),
        })
    }

    pub fn insert_char(&mut self, char_idx: usize, c: char) {
        let (start_byte, row, col) = self.byte_row_col_at_char(char_idx);
        let c_len = c.len_utf8();
        let (new_end_row, new_end_col) = if c == '\n' {
            (row + 1, 0)
        } else {
            (row, col + c_len)
        };
        self.rope.insert_char(char_idx, c);
        self.push_edit(Edit {
            start_byte,
            old_end_byte: start_byte,
            new_end_byte: start_byte + c_len,
            start_row: row,
            start_col: col,
            old_end_row: row,
            old_end_col: col,
            new_end_row,
            new_end_col,
        });
    }

    #[allow(dead_code)] // Phase 4: maps to `edit.replace_range` multi-char insert path.
    pub fn insert_str(&mut self, char_idx: usize, s: &str) {
        let (start_byte, row, col) = self.byte_row_col_at_char(char_idx);
        let (new_end_row, new_end_col) = advance_by(row, col, s);
        self.rope.insert(char_idx, s);
        self.push_edit(Edit {
            start_byte,
            old_end_byte: start_byte,
            new_end_byte: start_byte + s.len(),
            start_row: row,
            start_col: col,
            old_end_row: row,
            old_end_col: col,
            new_end_row,
            new_end_col,
        });
    }

    pub fn delete_range(&mut self, char_range: Range<usize>) {
        if char_range.start >= char_range.end {
            return;
        }
        let (start_byte, start_row, start_col) = self.byte_row_col_at_char(char_range.start);
        let (end_byte, end_row, end_col) = self.byte_row_col_at_char(char_range.end);
        self.rope.remove(char_range);
        self.push_edit(Edit {
            start_byte,
            old_end_byte: end_byte,
            new_end_byte: start_byte,
            start_row,
            start_col,
            old_end_row: end_row,
            old_end_col: end_col,
            new_end_row: start_row,
            new_end_col: start_col,
        });
    }

    /// Structural replacement: swap the byte range (typically the byte
    /// extents of a tree-sitter node returned by `Syntax::ast_query`) for
    /// `text`. Maps to DESIGN.md §Edits tier-2 `edit.replace_node`; Phase 4
    /// will expose it over MCP.
    #[allow(dead_code)] // Phase 4: exposed as `edit.replace_node` over MCP.
    pub fn replace_node(&mut self, byte_range: Range<usize>, text: &str) {
        let (start_row, start_col) = self.row_col_at_byte(byte_range.start);
        let (old_end_row, old_end_col) = self.row_col_at_byte(byte_range.end);
        let start_byte = byte_range.start;
        let old_end_byte = byte_range.end;
        let start_char = self.rope.byte_to_char(start_byte);
        let end_char = self.rope.byte_to_char(old_end_byte);
        if start_char < end_char {
            self.rope.remove(start_char..end_char);
        }
        self.rope.insert(start_char, text);
        let (new_end_row, new_end_col) = advance_by(start_row, start_col, text);
        self.push_edit(Edit {
            start_byte,
            old_end_byte,
            new_end_byte: start_byte + text.len(),
            start_row,
            start_col,
            old_end_row,
            old_end_col,
            new_end_row,
            new_end_col,
        });
    }

    pub fn save(&mut self) -> Result<usize> {
        let path = self
            .path
            .as_ref()
            .context("buffer has no associated path")?;
        let file = File::create(path)
            .with_context(|| format!("creating {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        self.rope
            .write_to(&mut writer)
            .with_context(|| format!("writing {}", path.display()))?;
        let bytes = self.rope.len_bytes();
        self.dirty = false;
        Ok(bytes)
    }

    pub fn line(&self, idx: usize) -> RopeSlice<'_> {
        self.rope.line(idx)
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn line_len_chars(&self, line: usize) -> usize {
        let s = self.rope.line(line);
        let len = s.len_chars();
        // Strip trailing line break so column math means "printable cols".
        let last = s.get_char(len.saturating_sub(1));
        match last {
            Some('\n') => len - 1,
            Some('\r') => len - 1,
            _ => len,
        }
    }

    pub fn line_to_char(&self, line: usize) -> usize {
        self.rope.line_to_char(line)
    }

    // Read-only handle to the underlying rope. Phase 2's syntax layer feeds
    // this to tree-sitter; Phase 4's MCP read path will use it too.
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    // Every protocol read returns a version (DESIGN.md §Buffers & views).
    // Phase 2 also reads it to invalidate the syntax cache.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Hand off the queued edits to the syntax layer. Returning the Vec
    /// (instead of `&[Edit]`) lets the caller forward it without holding
    /// a borrow on the Buffer during the reparse.
    pub fn drain_edits(&mut self) -> Vec<Edit> {
        std::mem::take(&mut self.pending_edits)
    }

    /// Snapshot the mutable buffer state so a transaction can roll back
    /// to it later. The clone is cheap — ropey's tree is shared via Arc,
    /// so only the structural delta gets copied on subsequent writes.
    pub fn snapshot(&self) -> BufferSnapshot {
        BufferSnapshot {
            rope: self.rope.clone(),
            version: self.version,
            dirty: self.dirty,
            pending_edits: self.pending_edits.clone(),
        }
    }

    /// Restore a snapshot taken via `snapshot`. Version bumps past its
    /// current value (rather than reverting) so any cache that saw the
    /// intermediate state — most notably the syntax tree — re-runs on
    /// the next refresh.
    ///
    /// NB: rolling back while the syntax cache holds a tree from the
    /// rolled-back side leaves that tree mismatched with the rope. Until
    /// Phase 4 wires App-level rollback (which can call
    /// `Syntax::invalidate`), only call this from contexts where syntax
    /// gets a clean re-run (e.g., in tests, or via a future App helper).
    pub fn restore(&mut self, snap: BufferSnapshot) {
        self.rope = snap.rope;
        self.version = self.version.wrapping_add(1);
        self.dirty = snap.dirty;
        self.pending_edits = snap.pending_edits;
        // snap.version is intentionally unused — see the doc comment.
        let _ = snap.version;
    }

    fn byte_row_col_at_char(&self, char_idx: usize) -> (usize, usize, usize) {
        let byte = self.rope.char_to_byte(char_idx);
        let row = self.rope.char_to_line(char_idx);
        let col = byte - self.rope.line_to_byte(row);
        (byte, row, col)
    }

    fn row_col_at_byte(&self, byte_idx: usize) -> (usize, usize) {
        let row = self.rope.byte_to_line(byte_idx);
        let col = byte_idx - self.rope.line_to_byte(row);
        (row, col)
    }

    fn push_edit(&mut self, edit: Edit) {
        self.pending_edits.push(edit);
        self.touch();
    }

    fn touch(&mut self) {
        self.version = self.version.wrapping_add(1);
        self.dirty = true;
    }
}

/// Given a starting `(row, col)` byte-coordinate and an inserted string,
/// return the `(row, col)` coordinate immediately after the insertion.
fn advance_by(row: usize, col: usize, s: &str) -> (usize, usize) {
    if let Some(last_nl) = s.rfind('\n') {
        let nl_count = s.as_bytes().iter().filter(|&&b| b == b'\n').count();
        (row + nl_count, s.len() - last_nl - 1)
    } else {
        (row, col + s.len())
    }
}
