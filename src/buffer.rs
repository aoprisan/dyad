use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ropey::{Rope, RopeSlice};

pub struct Buffer {
    rope: Rope,
    path: Option<PathBuf>,
    version: u64,
    dirty: bool,
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
        })
    }

    pub fn insert_char(&mut self, char_idx: usize, c: char) {
        self.rope.insert_char(char_idx, c);
        self.touch();
    }

    #[allow(dead_code)] // Phase 4: maps to `edit.replace_range` multi-char insert path.
    pub fn insert_str(&mut self, char_idx: usize, s: &str) {
        self.rope.insert(char_idx, s);
        self.touch();
    }

    pub fn delete_range(&mut self, char_range: Range<usize>) {
        if char_range.start >= char_range.end {
            return;
        }
        self.rope.remove(char_range);
        self.touch();
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

    fn touch(&mut self) {
        self.version = self.version.wrapping_add(1);
        self.dirty = true;
    }
}
