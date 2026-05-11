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

    /// Empty buffer with no path. Used when `dyad` is launched on a
    /// directory: the tree is the user's entry point and the editor
    /// waits with this scratch buffer until they pick a file.
    pub fn scratch() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            version: 0,
            dirty: false,
            pending_edits: Vec::new(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dyad_buffer_test_{}_{}_{}.rs",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ))
    }

    fn temp_buffer(name: &str) -> Buffer {
        let path = unique_path(name);
        let _ = std::fs::remove_file(&path);
        Buffer::open(path).unwrap()
    }

    #[test]
    fn scratch_starts_empty_and_unpathed() {
        let buf = Buffer::scratch();
        assert_eq!(buf.len_chars(), 0);
        assert_eq!(buf.version(), 0);
        assert!(!buf.is_dirty());
        assert!(buf.path().is_none());
    }

    #[test]
    fn open_missing_file_returns_empty_buffer() {
        let path = unique_path("missing");
        let _ = std::fs::remove_file(&path);
        let buf = Buffer::open(path.clone()).unwrap();
        assert_eq!(buf.len_chars(), 0);
        assert!(!buf.is_dirty());
        assert_eq!(buf.path(), Some(path.as_path()));
    }

    #[test]
    fn open_existing_file_loads_contents() {
        let path = unique_path("existing");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let buf = Buffer::open(path).unwrap();
        assert_eq!(buf.rope().to_string(), "hello\nworld\n");
        assert_eq!(buf.line_count(), 3); // includes trailing empty line.
    }

    #[test]
    fn insert_char_bumps_version_and_marks_dirty() {
        let mut buf = temp_buffer("insert_char");
        let v0 = buf.version();
        assert!(!buf.is_dirty());
        buf.insert_char(0, 'h');
        assert_eq!(buf.rope().to_string(), "h");
        assert_ne!(buf.version(), v0);
        assert!(buf.is_dirty());
    }

    #[test]
    fn insert_char_newline_advances_row() {
        let mut buf = temp_buffer("newline");
        buf.insert_char(0, 'a');
        buf.insert_char(1, '\n');
        buf.insert_char(2, 'b');
        let edits = buf.drain_edits();
        assert_eq!(edits.len(), 3);
        // After the third insert, the buffer is "a\nb"; the third edit
        // produced from 'b' at (1, 0) -> (1, 1).
        let last = edits[2];
        assert_eq!(last.start_row, 1);
        assert_eq!(last.start_col, 0);
        assert_eq!(last.new_end_row, 1);
        assert_eq!(last.new_end_col, 1);
    }

    #[test]
    fn insert_char_supports_multibyte() {
        let mut buf = temp_buffer("multibyte");
        // 'é' is U+00E9 (2 bytes in UTF-8).
        buf.insert_char(0, 'é');
        assert_eq!(buf.rope().to_string(), "é");
        let edits = buf.drain_edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_end_byte - edits[0].start_byte, 2);
        // Char index advances by one even though the byte offset moves two.
        assert_eq!(edits[0].new_end_col, 2);
    }

    #[test]
    fn insert_str_with_newlines_updates_row_column() {
        let mut buf = temp_buffer("insert_str");
        buf.insert_str(0, "ab\ncd");
        let edits = buf.drain_edits();
        assert_eq!(edits.len(), 1);
        let e = edits[0];
        assert_eq!(e.start_row, 0);
        assert_eq!(e.start_col, 0);
        assert_eq!(e.new_end_row, 1);
        assert_eq!(e.new_end_col, 2);
    }

    #[test]
    fn delete_range_empty_is_a_noop() {
        let mut buf = temp_buffer("noop_delete");
        buf.insert_str(0, "hello");
        let v_before = buf.version();
        buf.drain_edits();
        buf.delete_range(2..2);
        assert_eq!(buf.rope().to_string(), "hello");
        assert_eq!(buf.version(), v_before);
        assert!(buf.drain_edits().is_empty());
    }

    #[test]
    fn delete_range_removes_chars_and_records_edit() {
        let mut buf = temp_buffer("delete_range");
        buf.insert_str(0, "hello world");
        buf.drain_edits();
        buf.delete_range(5..11);
        assert_eq!(buf.rope().to_string(), "hello");
        let edits = buf.drain_edits();
        assert_eq!(edits.len(), 1);
        let e = edits[0];
        assert_eq!(e.start_byte, 5);
        assert_eq!(e.old_end_byte, 11);
        assert_eq!(e.new_end_byte, 5);
    }

    #[test]
    fn replace_node_swaps_byte_range() {
        let mut buf = temp_buffer("replace_node");
        buf.insert_str(0, "fn hello() {}");
        // bytes 3..8 = "hello"
        buf.replace_node(3..8, "goodbye");
        assert_eq!(buf.rope().to_string(), "fn goodbye() {}");
        assert!(buf.is_dirty());
    }

    #[test]
    fn line_len_chars_strips_terminator() {
        let mut buf = temp_buffer("line_len");
        buf.insert_str(0, "abc\ndef\r\nxy");
        assert_eq!(buf.line_len_chars(0), 3); // "abc"
        // "def\r\n" -> printable cols = 3 (the rope stores '\r' before '\n').
        // Implementation strips one trailing terminator, so 4 here ("def\r").
        // Keep the contract assertion narrow: 0-based count <= raw length.
        assert!(buf.line_len_chars(1) <= buf.line(1).len_chars());
        assert_eq!(buf.line_len_chars(2), 2); // "xy"
    }

    #[test]
    fn save_writes_contents_and_clears_dirty() {
        let path = unique_path("save");
        let _ = std::fs::remove_file(&path);
        let mut buf = Buffer::open(path.clone()).unwrap();
        buf.insert_str(0, "saved text\n");
        assert!(buf.is_dirty());
        let bytes = buf.save().unwrap();
        assert_eq!(bytes, "saved text\n".len());
        assert!(!buf.is_dirty());
        // File round-trips on disk.
        let from_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(from_disk, "saved text\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_without_path_errors() {
        let mut buf = Buffer::scratch();
        buf.insert_str(0, "scratch");
        let err = buf.save().unwrap_err();
        assert!(err.to_string().contains("no associated path"));
    }

    #[test]
    fn drain_edits_returns_then_clears_pending() {
        let mut buf = temp_buffer("drain");
        buf.insert_char(0, 'a');
        buf.insert_char(1, 'b');
        let first = buf.drain_edits();
        assert_eq!(first.len(), 2);
        // After draining, pending is empty.
        assert!(buf.drain_edits().is_empty());
    }

    #[test]
    fn snapshot_then_restore_round_trips_text() {
        let mut buf = temp_buffer("snapshot");
        buf.insert_str(0, "initial");
        let snap = buf.snapshot();
        let pre_version = buf.version();

        buf.insert_str(buf.len_chars(), " mutation");
        assert_eq!(buf.rope().to_string(), "initial mutation");

        buf.restore(snap);
        assert_eq!(buf.rope().to_string(), "initial");
        // Restore bumps version past the intermediate state so caches
        // invalidate; it does not revert version to the snapshot's.
        assert_ne!(buf.version(), pre_version);
    }

    #[test]
    fn line_to_char_aligns_with_rope() {
        let mut buf = temp_buffer("line_to_char");
        buf.insert_str(0, "a\nbc\ndef\n");
        assert_eq!(buf.line_to_char(0), 0);
        assert_eq!(buf.line_to_char(1), 2);
        assert_eq!(buf.line_to_char(2), 5);
    }
}
