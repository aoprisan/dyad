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

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_with(text: &str) -> Buffer {
        let mut b = Buffer::scratch();
        b.insert_str(0, text);
        b
    }

    #[test]
    fn new_view_starts_at_origin() {
        let v = View::new();
        assert_eq!(v.cursor_line(), 0);
        assert_eq!(v.cursor_col(), 0);
        assert_eq!(v.top_line(), 0);
    }

    #[test]
    fn move_right_steps_one_char_then_wraps_to_next_line() {
        let buf = buffer_with("ab\ncd\n");
        let mut v = View::new();
        v.move_right(&buf);
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 1));
        v.move_right(&buf);
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 2));
        // At end-of-line, next press jumps to start of next line.
        v.move_right(&buf);
        assert_eq!((v.cursor_line(), v.cursor_col()), (1, 0));
    }

    #[test]
    fn move_left_wraps_to_previous_line_end() {
        let buf = buffer_with("ab\ncd\n");
        let mut v = View::new();
        v.goto(&buf, 1, 0);
        v.move_left(&buf);
        // Lands at end of line 0 ("ab" = col 2).
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 2));
        // At buffer start, move_left is a no-op.
        v.goto(&buf, 0, 0);
        v.move_left(&buf);
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 0));
    }

    #[test]
    fn move_up_down_preserves_sticky_column() {
        let buf = buffer_with("longer line\nshort\nlonger again\n");
        let mut v = View::new();
        // Cursor on line 0, col 8.
        v.goto(&buf, 0, 8);
        v.move_down(&buf);
        // line 1 ("short") has only 5 chars; cursor clamps to 5.
        assert_eq!((v.cursor_line(), v.cursor_col()), (1, 5));
        v.move_down(&buf);
        // Sticky col 8 restored on the longer line.
        assert_eq!((v.cursor_line(), v.cursor_col()), (2, 8));
    }

    #[test]
    fn move_up_at_top_resets_column_to_zero() {
        let buf = buffer_with("abcd\nef\n");
        let mut v = View::new();
        v.goto(&buf, 0, 3);
        v.move_up(&buf);
        // Implementation: when on the first line, move_up homes the cursor.
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 0));
    }

    #[test]
    fn move_down_at_bottom_goes_to_end_of_last_line() {
        let buf = buffer_with("abc\nde");
        let mut v = View::new();
        v.goto(&buf, 1, 0);
        v.move_down(&buf);
        // Last line "de" -> cursor at col 2.
        assert_eq!((v.cursor_line(), v.cursor_col()), (1, 2));
    }

    #[test]
    fn move_home_and_end() {
        let buf = buffer_with("abcdef\n");
        let mut v = View::new();
        v.goto(&buf, 0, 3);
        v.move_home();
        assert_eq!(v.cursor_col(), 0);
        v.move_end(&buf);
        assert_eq!(v.cursor_col(), 6);
    }

    #[test]
    fn move_word_right_skips_word_then_whitespace() {
        let buf = buffer_with("hello world\n");
        let mut v = View::new();
        v.move_word_right(&buf);
        // Skips "hello" (5 chars) and the space → col 6.
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 6));
    }

    #[test]
    fn move_word_right_groups_punctuation_runs() {
        let buf = buffer_with("foo}}}bar\n");
        let mut v = View::new();
        v.goto(&buf, 0, 3);
        v.move_word_right(&buf);
        // The "}}}" run is treated as one hop, landing at "bar" (col 6).
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 6));
    }

    #[test]
    fn move_word_left_steps_back_one_word() {
        let buf = buffer_with("alpha beta gamma");
        let mut v = View::new();
        v.goto(&buf, 0, 16); // end of "gamma"
        v.move_word_left(&buf);
        // Land at start of "gamma".
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 11));
        v.move_word_left(&buf);
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 6));
    }

    #[test]
    fn move_word_left_at_start_is_noop() {
        let buf = buffer_with("hello");
        let mut v = View::new();
        v.move_word_left(&buf);
        assert_eq!(v.char_idx(&buf), 0);
    }

    #[test]
    fn page_up_and_page_down_clamp_to_buffer() {
        let buf = buffer_with("a\nb\nc\nd\ne\nf\ng\n");
        let mut v = View::new();
        v.page_down(&buf, 3);
        assert_eq!(v.cursor_line(), 3);
        assert_eq!(v.top_line(), 3);
        v.page_down(&buf, 100);
        // Clamped to last line.
        assert_eq!(v.cursor_line(), buf.line_count() - 1);
        v.page_up(&buf, 100);
        // Saturating subtract.
        assert_eq!(v.top_line(), 0);
        assert_eq!(v.cursor_line(), 0);
    }

    #[test]
    fn scroll_into_view_brings_cursor_into_viewport() {
        let buf = buffer_with("0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        let mut v = View::new();
        v.goto(&buf, 5, 0);
        v.scroll_into_view(3);
        // After scroll: top_line = cursor_line + 1 - viewport = 3.
        assert_eq!(v.top_line(), 3);
        // Scrolling further up moves top_line down.
        v.goto(&buf, 1, 0);
        v.scroll_into_view(3);
        assert_eq!(v.top_line(), 1);
    }

    #[test]
    fn after_insert_advances_for_each_char() {
        let buf = buffer_with("hi");
        let mut v = View::new();
        v.after_insert(&buf, "hi");
        assert_eq!(v.cursor_col(), 2);
        // After inserting a newline, cursor drops to next line.
        let buf2 = buffer_with("hi\n");
        v.after_insert(&buf2, "\n");
        assert_eq!(v.cursor_line(), 1);
        assert_eq!(v.cursor_col(), 0);
    }

    #[test]
    fn after_delete_prev_walks_back_across_lines() {
        let buf = buffer_with("ab\ncd\n");
        let mut v = View::new();
        v.goto(&buf, 1, 0);
        v.after_delete_prev(&buf);
        // Wraps to end of previous line.
        assert_eq!((v.cursor_line(), v.cursor_col()), (0, 2));
    }

    #[test]
    fn goto_clamps_line_and_column() {
        let buf = buffer_with("ab\nc\n");
        let mut v = View::new();
        // Past end of column.
        v.goto(&buf, 1, 99);
        assert_eq!((v.cursor_line(), v.cursor_col()), (1, 1));
        // Past end of buffer.
        v.goto(&buf, 99, 0);
        let last = buf.line_count() - 1;
        assert_eq!(v.cursor_line(), last);
    }

    #[test]
    fn char_idx_matches_line_to_char_plus_col() {
        let buf = buffer_with("ab\ncde\nfg\n");
        let mut v = View::new();
        v.goto(&buf, 1, 2);
        // line_to_char(1) = 3, col 2 => char_idx = 5.
        assert_eq!(v.char_idx(&buf), 5);
    }
}
