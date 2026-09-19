//! Multiline text-input widget for popups (comment text, suggested edits).
//!
//! Two invariants hold after every operation: `lines` is never empty and
//! `row` always indexes an existing line; `col` is a byte offset inside that
//! line, always on a UTF-8 codepoint boundary, so `lines[row][..col]` is a
//! valid prefix. No line ever contains a literal `\n`; newlines split rows.
//!
//! Parent dispatchers own Esc, Ctrl-C and plain Enter: [`TextInput::handle_key`]
//! reports those as [`Handled::Ignored`] so the caller can fall through. It
//! also returns `Ignored` for keys the buffer cannot act on (backspace at the
//! very start, right at the very end), and `Moved`/`Changed` otherwise.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Outcome of [`TextInput::handle_key`], so callers can fall through to their
/// own bindings when the buffer did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// The key is not the widget's business (or cannot act) — parent's turn.
    Ignored,
    /// Cursor moved; text unchanged.
    Moved,
    /// Text changed.
    Changed,
}

#[derive(Debug, Clone)]
pub struct TextInput {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

impl TextInput {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub fn from(text: &str) -> Self {
        let mut me = Self {
            lines: text.split('\n').map(str::to_string).collect(),
            row: 0,
            col: 0,
        };
        if me.lines.is_empty() {
            me.lines.push(String::new());
        }
        me.row = me.lines.len() - 1;
        me.move_end();
        me
    }

    pub fn value(&self) -> String {
        self.lines.join("\n")
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// True when every line is empty.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    /// 0-based row index of the cursor.
    #[cfg(test)]
    pub fn cursor_line(&self) -> usize {
        self.row
    }

    /// Byte offset of the cursor inside the current line.
    #[cfg(test)]
    pub fn cursor_col(&self) -> usize {
        self.col
    }

    // --- editing -----------------------------------------------------------

    pub fn insert_text(&mut self, text: &str) {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.split_line();
            }
            if !part.is_empty() {
                self.insert_raw(part);
            }
        }
    }

    pub fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_raw(c.encode_utf8(&mut buf));
    }

    fn insert_raw(&mut self, s: &str) {
        let line = &mut self.lines[self.row];
        line.insert_str(self.col, s);
        self.col += s.len();
    }

    fn split_line(&mut self) {
        let line = &mut self.lines[self.row];
        let tail = line.split_off(self.col);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    /// Backspace: delete before the cursor, joining lines at the start.
    pub fn remove_char(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let start = prev_boundary(line, self.col);
            line.replace_range(start..self.col, "");
            self.col = start;
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].push_str(&cur);
        }
    }

    /// Delete under the cursor, joining lines at the end.
    pub fn delete_char(&mut self) {
        let line = &mut self.lines[self.row];
        if self.col < line.len() {
            let end = next_boundary(line, self.col);
            line.replace_range(self.col..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    /// Ctrl-W: delete trailing whitespace and then one word before the cursor.
    pub fn remove_word_before(&mut self) {
        if self.col == 0 {
            return;
        }
        let line = self.lines[self.row].clone();
        let start = word_start(&line, self.col);
        self.lines[self.row].replace_range(start..self.col, "");
        self.col = start;
    }

    /// Ctrl-K: truncate the current line at the cursor.
    pub fn kill_to_end_of_line(&mut self) {
        self.lines[self.row].truncate(self.col);
    }

    // --- movement ----------------------------------------------------------

    pub fn move_left(&mut self) {
        if self.col > 0 {
            let line = &self.lines[self.row];
            self.col = prev_boundary(line, self.col);
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    pub fn move_right(&mut self) {
        let line = &self.lines[self.row];
        if self.col < line.len() {
            self.col = next_boundary(line, self.col);
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.clamp_col();
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.clamp_col();
        }
    }

    pub fn move_home(&mut self) {
        self.col = 0;
    }

    pub fn move_end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    /// Vim `b`: back to the start of the previous word.
    pub fn move_word_left(&mut self) {
        if self.col == 0 {
            return;
        }
        let line = self.lines[self.row].clone();
        self.col = word_start(&line, self.col);
    }

    /// Vim `w`: forward to the start of the next word.
    pub fn move_word_right(&mut self) {
        let line = self.lines[self.row].clone();
        if self.col >= line.len() {
            self.move_right();
            return;
        }
        self.col = word_end(&line, self.col);
    }

    /// No desired-column memory: vertical moves just clamp into the new line.
    fn clamp_col(&mut self) {
        let line = &self.lines[self.row];
        if self.col > line.len() {
            self.col = line.len();
        }
        while self.col > 0 && !line.is_char_boundary(self.col) {
            self.col -= 1;
        }
    }

    // --- keys --------------------------------------------------------------

    pub fn handle_key(&mut self, ev: &KeyEvent) -> Handled {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        match ev.code {
            KeyCode::Char('c') if ctrl => Handled::Ignored,
            KeyCode::Esc => Handled::Ignored,
            KeyCode::Enter => Handled::Ignored,

            KeyCode::Char('j') if ctrl => {
                self.insert_text("\n");
                Handled::Changed
            }
            KeyCode::Char('a') if ctrl => {
                self.move_home();
                Handled::Moved
            }
            KeyCode::Char('e') if ctrl => {
                self.move_end();
                Handled::Moved
            }
            KeyCode::Char('w') if ctrl => {
                if self.col == 0 {
                    return Handled::Ignored;
                }
                self.remove_word_before();
                Handled::Changed
            }
            KeyCode::Char('k') if ctrl => {
                if self.col >= self.lines[self.row].len() {
                    return Handled::Ignored;
                }
                self.kill_to_end_of_line();
                Handled::Changed
            }
            KeyCode::Char('u') if ctrl => {
                if self.col == 0 {
                    return Handled::Ignored;
                }
                self.lines[self.row].replace_range(..self.col, "");
                self.col = 0;
                Handled::Changed
            }
            KeyCode::Char('d') if ctrl => {
                if self.col >= self.lines[self.row].len() {
                    return Handled::Ignored;
                }
                self.delete_char();
                Handled::Changed
            }

            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_char(c);
                Handled::Changed
            }

            KeyCode::Backspace => {
                if self.col == 0 && self.row == 0 {
                    return Handled::Ignored;
                }
                self.remove_char();
                Handled::Changed
            }
            KeyCode::Delete => {
                let line_len = self.lines[self.row].len();
                if self.col >= line_len && self.row + 1 >= self.lines.len() {
                    return Handled::Ignored;
                }
                self.delete_char();
                Handled::Changed
            }

            KeyCode::Left => {
                let at_start = self.col == 0 && self.row == 0;
                if at_start {
                    return Handled::Ignored;
                }
                if alt {
                    self.move_word_left();
                } else {
                    self.move_left();
                }
                Handled::Moved
            }
            KeyCode::Right => {
                let line_len = self.lines[self.row].len();
                if self.col >= line_len && self.row + 1 >= self.lines.len() {
                    return Handled::Ignored;
                }
                if alt {
                    self.move_word_right();
                } else {
                    self.move_right();
                }
                Handled::Moved
            }
            KeyCode::Home => {
                self.move_home();
                Handled::Moved
            }
            KeyCode::End => {
                self.move_end();
                Handled::Moved
            }
            KeyCode::Up => {
                if self.row == 0 {
                    return Handled::Ignored;
                }
                self.move_up();
                Handled::Moved
            }
            KeyCode::Down => {
                if self.row + 1 >= self.lines.len() {
                    return Handled::Ignored;
                }
                self.move_down();
                Handled::Moved
            }

            _ => Handled::Ignored,
        }
    }

    // --- rendering ---------------------------------------------------------

    pub fn render(&self, frame: &mut Frame, area: Rect, title: &str) {
        let inner_w = area.width.saturating_sub(2) as usize;
        let inner_h = area.height.saturating_sub(2) as usize;
        let offset = self.scroll_offset(inner_h);

        let mut rows: Vec<Line> = Vec::new();
        for (i, text) in self.lines.iter().enumerate().skip(offset).take(inner_h) {
            rows.push(self.render_row(text, inner_w, i == self.row));
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title.to_string());
        frame.render_widget(Paragraph::new(rows).block(block), area);
    }

    fn scroll_offset(&self, height: usize) -> usize {
        if height == 0 || self.row < height {
            0
        } else {
            self.row + 1 - height
        }
    }

    /// Clip the line to the pane width, highlighting the cell under the cursor.
    fn render_row(&self, text: &str, width: usize, is_cursor_row: bool) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        let mut buf = String::new();
        let mut buf_cursor = false;

        let flush = |spans: &mut Vec<Span<'static>>, buf: &mut String, cursor: &mut bool| {
            if buf.is_empty() {
                return;
            }
            let style = if *cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            spans.push(Span::styled(std::mem::take(buf), style));
            *cursor = false;
        };

        for (byte, ch) in text.char_indices() {
            if used >= width {
                break;
            }
            let at_cursor = is_cursor_row && byte == self.col;
            if at_cursor != buf_cursor {
                flush(&mut spans, &mut buf, &mut buf_cursor);
                buf_cursor = at_cursor;
            }
            buf.push(ch);
            used += 1;
        }

        if is_cursor_row && self.col >= text.len() && used < width {
            flush(&mut spans, &mut buf, &mut buf_cursor);
            buf_cursor = true;
            buf.push(' ');
        }
        flush(&mut spans, &mut buf, &mut buf_cursor);

        if used == 0 && spans.is_empty() {
            spans.push(Span::raw(""));
        }
        Line::from(spans)
    }
}

/// Byte offset of the previous codepoint boundary strictly before `col`.
fn prev_boundary(line: &str, col: usize) -> usize {
    let mut i = col.saturating_sub(1);
    while i > 0 && !line.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Byte offset of the next codepoint boundary strictly after `col`.
fn next_boundary(line: &str, col: usize) -> usize {
    let mut i = col + 1;
    while i < line.len() && !line.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Start of the word (or whitespace run) preceding `col`, Vim `b`-ish: first
/// step back over any whitespace, then over the run of same-class chars.
fn word_start(line: &str, col: usize) -> usize {
    let mut idx: Vec<usize> = line.char_indices().map(|(i, _)| i).collect();
    idx.push(line.len());
    let pos = idx.iter().position(|&i| i >= col).unwrap_or(idx.len() - 1);
    let mut i = pos;
    while i > 0 && line[idx[i - 1]..idx[i]].chars().all(char::is_whitespace) {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let class = line[idx[i - 1]..idx[i]].chars().next().is_some_and(is_word);
    while i > 0 {
        let prev = &line[idx[i - 1]..idx[i]];
        if prev.chars().all(char::is_whitespace) {
            break;
        }
        if prev.chars().next().is_some_and(is_word) != class {
            break;
        }
        i -= 1;
    }
    idx[i]
}

/// End of the run following `col`, then any trailing whitespace (Vim `w`):
/// the cursor lands on the first char of the next word.
fn word_end(line: &str, col: usize) -> usize {
    let mut idx: Vec<usize> = line.char_indices().map(|(i, _)| i).collect();
    idx.push(line.len());
    let pos = idx.iter().position(|&i| i >= col).unwrap_or(idx.len() - 1);
    let class = idx
        .get(pos)
        .filter(|&&i| i < line.len())
        .and_then(|&i| line[i..].chars().next())
        .is_some_and(is_word);
    let mut i = pos;
    while i + 1 < idx.len() {
        let ch = line[idx[i]..idx[i + 1]].chars().next();
        match ch {
            Some(c) if c.is_whitespace() => break,
            Some(c) if is_word(c) == class => i += 1,
            _ => break,
        }
    }
    while i + 1 < idx.len() && line[idx[i]..idx[i + 1]].chars().all(char::is_whitespace) {
        i += 1;
    }
    idx[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), m)
    }

    fn code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(text: &str) -> TextInput {
        let mut t = TextInput::new();
        for c in text.chars() {
            t.insert_char(c);
        }
        t
    }

    #[test]
    fn new_is_one_empty_line() {
        let t = TextInput::new();
        assert_eq!(t.line_count(), 1);
        assert!(t.is_empty());
        assert_eq!(t.value(), "");
    }

    #[test]
    fn insert_and_value_roundtrip() {
        let t = TextInput::from("line 1\nline 2");
        assert_eq!(t.line_count(), 2);
        assert_eq!(t.value(), "line 1\nline 2");
        assert!(!t.is_empty());
        assert_eq!(TextInput::from("").value(), "");
    }

    #[test]
    fn from_places_cursor_at_end() {
        let t = TextInput::from("abc\nde");
        assert_eq!(t.cursor_line(), 1);
        assert_eq!(t.cursor_col(), 2);
    }

    #[test]
    fn backspace_at_origin_is_ignored() {
        let mut t = TextInput::new();
        assert_eq!(t.handle_key(&code(KeyCode::Backspace)), Handled::Ignored);
        assert_eq!(t.value(), "");
    }

    #[test]
    fn delete_at_end_is_ignored() {
        let mut t = typed("abc");
        assert_eq!(t.handle_key(&code(KeyCode::Delete)), Handled::Ignored);
    }

    #[test]
    fn backspace_joins_lines() {
        let mut t = TextInput::from("ab\ncd");
        t.move_home();
        assert_eq!(t.handle_key(&code(KeyCode::Backspace)), Handled::Changed);
        assert_eq!(t.value(), "abcd");
        assert_eq!(t.cursor_line(), 0);
        assert_eq!(t.cursor_col(), 2);
    }

    #[test]
    fn delete_joins_lines() {
        let mut t = TextInput::from("ab\ncd");
        t.row = 0;
        t.col = 2;
        assert_eq!(t.handle_key(&code(KeyCode::Delete)), Handled::Changed);
        assert_eq!(t.value(), "abcd");
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        let mut t = typed("ab");
        assert_eq!(
            t.handle_key(&key('j', KeyModifiers::CONTROL)),
            Handled::Changed
        );
        t.insert_char('c');
        assert_eq!(t.value(), "ab\nc");
        assert_eq!(t.line_count(), 2);
    }

    #[test]
    fn parent_owned_keys_are_ignored() {
        let mut t = typed("abc");
        assert_eq!(t.handle_key(&code(KeyCode::Enter)), Handled::Ignored);
        assert_eq!(t.handle_key(&code(KeyCode::Esc)), Handled::Ignored);
        assert_eq!(
            t.handle_key(&key('c', KeyModifiers::CONTROL)),
            Handled::Ignored
        );
        assert_eq!(t.value(), "abc");
    }

    #[test]
    fn vertical_moves_clamp_on_ragged_lines() {
        let mut t = TextInput::from("abcdef\nx");
        t.row = 0;
        t.col = 6;
        assert_eq!(t.handle_key(&code(KeyCode::Down)), Handled::Moved);
        assert_eq!(t.cursor_col(), 1);
        assert_eq!(t.handle_key(&code(KeyCode::Up)), Handled::Moved);
        assert_eq!(t.cursor_col(), 1, "no desired-column drift");
    }

    #[test]
    fn vertical_moves_are_ignored_at_ends() {
        let mut t = TextInput::from("a\nb");
        t.row = 0;
        assert_eq!(t.handle_key(&code(KeyCode::Up)), Handled::Ignored);
        t.row = 1;
        assert_eq!(t.handle_key(&code(KeyCode::Down)), Handled::Ignored);
    }

    #[test]
    fn home_end_and_ctrl_aliases() {
        let mut t = typed("hello");
        assert_eq!(t.handle_key(&code(KeyCode::Home)), Handled::Moved);
        assert_eq!(t.cursor_col(), 0);
        assert_eq!(t.handle_key(&code(KeyCode::Right)), Handled::Moved);
        assert_eq!(t.cursor_col(), 1, "plain Right moves one char");
        assert_eq!(t.handle_key(&code(KeyCode::End)), Handled::Moved);
        assert_eq!(t.cursor_col(), 5);
        assert_eq!(
            t.handle_key(&key('a', KeyModifiers::CONTROL)),
            Handled::Moved
        );
        assert_eq!(t.cursor_col(), 0);
        assert_eq!(
            t.handle_key(&key('e', KeyModifiers::CONTROL)),
            Handled::Moved
        );
        assert_eq!(t.cursor_col(), 5);
    }

    #[test]
    fn ctrl_w_removes_word_before() {
        let mut t = typed("hello world");
        assert_eq!(
            t.handle_key(&key('w', KeyModifiers::CONTROL)),
            Handled::Changed
        );
        assert_eq!(t.value(), "hello ");
        assert_eq!(t.cursor_col(), 6);
        assert_eq!(
            t.handle_key(&key('w', KeyModifiers::CONTROL)),
            Handled::Changed
        );
        assert_eq!(t.value(), "");
        assert_eq!(
            t.handle_key(&key('w', KeyModifiers::CONTROL)),
            Handled::Ignored
        );
    }

    #[test]
    fn ctrl_k_kills_to_end_of_line() {
        let mut t = typed("hello");
        t.col = 2;
        assert_eq!(
            t.handle_key(&key('k', KeyModifiers::CONTROL)),
            Handled::Changed
        );
        assert_eq!(t.value(), "he");
        assert_eq!(
            t.handle_key(&key('k', KeyModifiers::CONTROL)),
            Handled::Ignored
        );
    }

    #[test]
    fn ctrl_u_kills_from_line_start() {
        let mut t = typed("hello");
        t.col = 3;
        assert_eq!(
            t.handle_key(&key('u', KeyModifiers::CONTROL)),
            Handled::Changed
        );
        assert_eq!(t.value(), "lo");
        assert_eq!(t.cursor_col(), 0);
    }

    #[test]
    fn word_movement_skips_whitespace_and_punctuation() {
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);
        let mut t = typed("foo bar_baz(x, y)");
        assert_eq!(t.handle_key(&left), Handled::Moved);
        assert_eq!(t.cursor_col(), 16, "b to start of `y`");
        assert_eq!(t.handle_key(&left), Handled::Moved);
        assert_eq!(t.cursor_col(), 15, "skips the space before `y`");
        t.col = 12;
        assert_eq!(t.handle_key(&right), Handled::Moved);
        assert_eq!(t.cursor_col(), 13, "w lands on `x`");
        t.col = 5;
        assert_eq!(t.handle_key(&right), Handled::Moved);
        assert_eq!(t.cursor_col(), 11, "w skips the whole identifier run");
    }

    #[test]
    fn alt_arrows_jump_whole_words() {
        let mut t = typed("hello world");
        assert_eq!(
            t.handle_key(&key('0', KeyModifiers::NONE)),
            Handled::Changed
        );
        t.move_home();
        assert_eq!(
            t.handle_key(&KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
            Handled::Moved
        );
        assert_eq!(t.cursor_col(), 6);
        assert_eq!(
            t.handle_key(&KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            Handled::Moved
        );
        assert_eq!(t.cursor_col(), 0);
    }

    #[test]
    fn multibyte_cursor_stays_on_boundaries() {
        let mut t = typed("héllo wörld");
        t.move_home();
        for _ in 0..20 {
            t.move_right();
            let line = t.lines[t.row].clone();
            assert!(line.is_char_boundary(t.col), "col {} not a boundary", t.col);
        }
        assert_eq!(t.cursor_col(), t.lines[0].len());
        t.col = t.lines[0].len();
        t.remove_char();
        assert_eq!(t.value(), "héllo wörl", "one char, not one byte");
        let line = t.lines[0].clone();
        assert!(line.is_char_boundary(t.col));
    }

    #[test]
    fn japanese_word_ops_keep_boundaries() {
        let mut t = typed("日本語 text");
        assert_eq!(t.cursor_col(), 14, "cursor starts at the end");
        assert_eq!(t.handle_key(&code(KeyCode::Left)), Handled::Moved);
        assert_eq!(t.cursor_col(), 13, "plain Left is one byte here");
        let line = t.lines[0].clone();
        assert!(line.is_char_boundary(t.col));
        t.move_word_left();
        let line = t.lines[0].clone();
        assert!(line.is_char_boundary(t.col));
        assert_eq!(t.cursor_col(), 10, "start of `text`");
        t.remove_char();
        let line = t.lines[0].clone();
        assert!(line.is_char_boundary(t.col));
        assert_eq!(t.value(), "日本語text");
    }

    #[test]
    fn insert_text_with_newlines_splits_rows() {
        let mut t = typed("ab");
        t.insert_text("\nline 1\nline 2");
        assert_eq!(t.value(), "ab\nline 1\nline 2");
        assert_eq!(t.line_count(), 3);
    }
}
