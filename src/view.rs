use crate::buffer::Buffer;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub struct View {
    cursor_line: usize,
    cursor_col: usize,
    sticky_col: usize,
    top_line: usize,
}

impl View {
    pub fn new() -> Self {
        Self {
            cursor_line: 0,
            cursor_col: 0,
            sticky_col: 0,
            top_line: 0,
        }
    }

    pub fn cursor_line(&self) -> usize {
        self.cursor_line
    }

    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    pub fn top_line(&self) -> usize {
        self.top_line
    }

    pub fn move_left(&mut self, buf: &Buffer) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = buf.line_len_chars(self.cursor_line);
        }
        self.sticky_col = self.cursor_col;
    }

    pub fn move_right(&mut self, buf: &Buffer) {
        let line_len = buf.line_len_chars(self.cursor_line);
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_line + 1 < buf.line_count() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
        self.sticky_col = self.cursor_col;
    }

    pub fn move_up(&mut self, buf: &Buffer) {
        if self.cursor_line == 0 {
            self.cursor_col = 0;
            self.sticky_col = 0;
            return;
        }
        self.cursor_line -= 1;
        self.cursor_col = self.sticky_col.min(buf.line_len_chars(self.cursor_line));
    }

    pub fn move_down(&mut self, buf: &Buffer) {
        if self.cursor_line + 1 >= buf.line_count() {
            let line_len = buf.line_len_chars(self.cursor_line);
            self.cursor_col = line_len;
            self.sticky_col = line_len;
            return;
        }
        self.cursor_line += 1;
        self.cursor_col = self.sticky_col.min(buf.line_len_chars(self.cursor_line));
    }

    /// Jump to the next word boundary to the right. Matches the macOS
    /// Option+Right convention: skip the rest of the current run (word
    /// chars or punctuation), then any whitespace, landing on the start
    /// of the next non-space character. Crosses line boundaries.
    pub fn move_word_right(&mut self, buf: &Buffer) {
        let total = buf.len_chars();
        let mut i = self.char_idx(buf);
        if i >= total {
            return;
        }
        let rope = buf.rope();
        let starting = rope.char(i);
        if is_word_char(starting) {
            while i < total && is_word_char(rope.char(i)) {
                i += 1;
            }
        } else if !starting.is_whitespace() {
            // Punctuation run — keep them grouped so e.g. `}}}` is one hop.
            while i < total && !is_word_char(rope.char(i)) && !rope.char(i).is_whitespace() {
                i += 1;
            }
        }
        while i < total && rope.char(i).is_whitespace() {
            i += 1;
        }
        self.set_char_idx(buf, i);
    }

    /// Mirror of `move_word_right`: step back over any whitespace, then
    /// over the contiguous run (word or punctuation) ending at the
    /// cursor's left.
    pub fn move_word_left(&mut self, buf: &Buffer) {
        let mut i = self.char_idx(buf);
        if i == 0 {
            return;
        }
        let rope = buf.rope();
        while i > 0 && rope.char(i - 1).is_whitespace() {
            i -= 1;
        }
        if i == 0 {
            self.set_char_idx(buf, 0);
            return;
        }
        let starting_word = is_word_char(rope.char(i - 1));
        while i > 0 {
            let c = rope.char(i - 1);
            if c.is_whitespace() || is_word_char(c) != starting_word {
                break;
            }
            i -= 1;
        }
        self.set_char_idx(buf, i);
    }

    fn set_char_idx(&mut self, buf: &Buffer, idx: usize) {
        let idx = idx.min(buf.len_chars());
        let line = buf.rope().char_to_line(idx);
        let col = idx - buf.line_to_char(line);
        let max_col = buf.line_len_chars(line);
        self.cursor_line = line;
        self.cursor_col = col.min(max_col);
        self.sticky_col = self.cursor_col;
    }

    pub fn move_home(&mut self) {
        self.cursor_col = 0;
        self.sticky_col = 0;
    }

    pub fn move_end(&mut self, buf: &Buffer) {
        self.cursor_col = buf.line_len_chars(self.cursor_line);
        self.sticky_col = self.cursor_col;
    }

    pub fn page_up(&mut self, buf: &Buffer, viewport_rows: u16) {
        let step = viewport_rows.max(1) as usize;
        self.cursor_line = self.cursor_line.saturating_sub(step);
        self.top_line = self.top_line.saturating_sub(step);
        self.cursor_col = self.sticky_col.min(buf.line_len_chars(self.cursor_line));
    }

    pub fn page_down(&mut self, buf: &Buffer, viewport_rows: u16) {
        let step = viewport_rows.max(1) as usize;
        let last_line = buf.line_count().saturating_sub(1);
        self.cursor_line = (self.cursor_line + step).min(last_line);
        self.top_line = (self.top_line + step).min(last_line);
        self.cursor_col = self.sticky_col.min(buf.line_len_chars(self.cursor_line));
    }

    pub fn scroll_into_view(&mut self, viewport_rows: u16) {
        let rows = viewport_rows.max(1) as usize;
        if self.cursor_line < self.top_line {
            self.top_line = self.cursor_line;
        } else if self.cursor_line >= self.top_line + rows {
            self.top_line = self.cursor_line + 1 - rows;
        }
    }

    pub fn char_idx(&self, buf: &Buffer) -> usize {
        buf.line_to_char(self.cursor_line) + self.cursor_col
    }

    pub fn after_insert(&mut self, buf: &Buffer, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.cursor_line += 1;
                self.cursor_col = 0;
            } else {
                self.cursor_col += 1;
            }
        }
        self.sticky_col = self.cursor_col;
        // Clamp in case insertions land at an unusual spot.
        let line_len = buf.line_len_chars(self.cursor_line);
        if self.cursor_col > line_len {
            self.cursor_col = line_len;
        }
    }

    pub fn after_delete_prev(&mut self, buf: &Buffer) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = buf.line_len_chars(self.cursor_line);
        }
        self.sticky_col = self.cursor_col;
    }

    /// Jump the cursor to (line, col), clamped to the buffer's bounds.
    /// `scroll_into_view` is the caller's responsibility — App::apply runs
    /// it after every action.
    pub fn goto(&mut self, buf: &Buffer, line: usize, col: usize) {
        let last_line = buf.line_count().saturating_sub(1);
        self.cursor_line = line.min(last_line);
        self.cursor_col = col.min(buf.line_len_chars(self.cursor_line));
        self.sticky_col = self.cursor_col;
    }
}
