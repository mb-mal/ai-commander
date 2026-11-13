//! Minimal multi-line text editor for the composer (no external widget deps,
//! full control over Enter/Ctrl+Enter behaviour and unicode). Cursor movement
//! and deletion are grapheme-aware (emoji, CJK combining sequences).

#[derive(Debug, Clone, Default)]
pub struct Editor {
    lines: Vec<String>,
    /// logical line index
    pub row: usize,
    /// column in chars
    pub col: usize,
}

impl Editor {
    pub fn new() -> Self {
        Editor {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub fn with_text(text: &str) -> Self {
        let mut ed = Editor::new();
        ed.set_text(text);
        ed
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_owned).collect()
        };
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn line_chars(&self) -> Vec<char> {
        self.lines[self.row].chars().collect()
    }

    fn set_line_from_chars(&mut self, chars: Vec<char>) {
        self.lines[self.row] = chars.into_iter().collect();
    }

    pub fn insert_char(&mut self, c: char) {
        let mut chars = self.line_chars();
        let col = self.col.min(chars.len());
        chars.insert(col, c);
        self.set_line_from_chars(chars);
        self.col = col + 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\r' {
                continue;
            }
            if c == '\n' {
                self.newline();
            } else {
                self.insert_char(c);
            }
        }
    }

    pub fn newline(&mut self) {
        let chars = self.line_chars();
        let col = self.col.min(chars.len());
        let rest: String = chars[col..].iter().collect();
        let head: String = chars[..col].iter().collect();
        self.lines[self.row] = head;
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    /// Char index of the start of the grapheme that *contains or ends at*
    /// `char_idx` — i.e. deleting backwards from `char_idx` removes exactly
    /// that grapheme (works for emoji, CJK and combining marks).
    fn floor_grapheme(&self, line: &str, char_idx: usize) -> usize {
        use unicode_segmentation::UnicodeSegmentation;
        let mut start = 0usize;
        for g in line.graphemes(true) {
            let len = g.chars().count();
            if char_idx < start + len {
                return start;
            }
            start += len;
        }
        start
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let line = self.lines[self.row].clone();
            let target = self.floor_grapheme(&line, self.col - 1);
            let removed = target..self.col;
            let kept: String = line
                .chars()
                .enumerate()
                .filter(|(i, _)| !removed.contains(i))
                .map(|(_, c)| c)
                .collect();
            self.lines[self.row] = kept;
            self.col = target;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&line);
        }
    }

    pub fn delete(&mut self) {
        use unicode_segmentation::UnicodeSegmentation;
        let line = self.lines[self.row].clone();
        let len = line.chars().count();
        if self.col < len {
            // v10: delete the WHOLE grapheme (not one char)
            let mut start = 0usize;
            let mut end = self.col;
            for g in line.graphemes(true) {
                let glen = g.chars().count();
                if self.col < start + glen {
                    end = start + glen;
                    break;
                }
                start += glen;
            }
            if end <= self.col {
                end = self.col + 1;
            }
            let kept: String = line
                .chars()
                .enumerate()
                .filter(|(i, _)| *i < self.col || *i >= end)
                .map(|(_, c)| c)
                .collect();
            self.lines[self.row] = kept;
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            // v10: step to the START of the previous GRAPHEME (not char)
            let line = self.lines[self.row].clone();
            self.col = self.floor_grapheme(&line, self.col);
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn right(&mut self) {
        use unicode_segmentation::UnicodeSegmentation;
        let line = &self.lines[self.row];
        let len = line.chars().count();
        if self.col < len {
            // v10: skip the WHOLE grapheme (emoji+ZWJ = one step)
            let mut start = 0usize;
            for g in line.graphemes(true) {
                let glen = g.chars().count();
                if self.col < start + glen {
                    self.col = start + glen;
                    return;
                }
                start += glen;
            }
            self.col = len;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    pub fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    /// Display position of the cursor (unwrapped, width in cells).
    /// v9 #5: display width via UnicodeWidthStr (CJK/emoji = 2 cells).
    pub fn screen_cursor(&self) -> (u16, u16) {
        let line = self.lines.get(self.row).map(|s| s.as_str()).unwrap_or("");
        let c = self.col.min(line.chars().count());
        let col = self.display_col(line, c);
        (self.row as u16, col as u16)
    }

    /// Display position of the cursor after soft-wrapping at `width`:
    /// (wrapped_line_index, column_within_line).
    pub fn screen_cursor_wrapped(&self, width: usize) -> (u16, u16) {
        let width = width.max(1); // v10: no div-by-zero
        let mut row_offset = 0u16;
        for (i, line) in self.lines.iter().enumerate() {
            let n = unicode_width::UnicodeWidthStr::width(line.as_str());
            // v10: an empty line still occupies ONE screen row
            let wrapped: u16 = if n == 0 { 1 } else { n.div_ceil(width) as u16 };
            if i == self.row {
                let c = self.col.min(line.chars().count());
                let w = self.display_col(line, c);
                let r = w / width;
                return (row_offset + r as u16, (w % width) as u16);
            }
            row_offset += wrapped;
        }
        (row_offset, 0)
    }

    /// Display column (in cells) of char index `char_idx` within `line`.
    fn display_col(&self, line: &str, char_idx: usize) -> usize {
        let end = line
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(line.len());
        unicode_width::UnicodeWidthStr::width(&line[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_and_newline() {
        let mut ed = Editor::new();
        ed.insert_str("hello");
        ed.newline();
        ed.insert_str("мир");
        assert_eq!(ed.text(), "hello\nмир");
        assert_eq!((ed.row, ed.col), (1, 3));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut ed = Editor::with_text("ab\ncd");
        ed.end();
        ed.backspace(); // remove 'd'
        ed.backspace(); // remove 'c' -> line empty
        assert_eq!(ed.text(), "ab\n");
        ed.backspace(); // join empty line into "ab"
        assert_eq!(ed.text(), "ab");
        assert_eq!((ed.row, ed.col), (0, 2));
    }

    #[test]
    fn backspace_removes_whole_grapheme() {
        let mut ed = Editor::with_text("привет👍!");
        ed.end();
        // "👍" is one grapheme of two chars: one backspace removes both
        ed.backspace(); // removes '!'
        ed.backspace(); // removes the whole emoji
        assert_eq!(ed.text(), "привет");
        assert_eq!(ed.col, 6);
    }

    #[test]
    fn screen_cursor_wraps() {
        let mut ed = Editor::with_text("abcdefghij");
        ed.end();
        let (r, c) = ed.screen_cursor_wrapped(4);
        assert_eq!((r, c), (2, 2));
    }
}
