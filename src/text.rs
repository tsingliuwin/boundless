//! Canvas text editing: a rope-backed editing session for a text element.
//!
//! GPUI's `EntityInputHandler` speaks UTF-16 offsets (like the OS IME APIs),
//! while the rope works in char/byte offsets — this module does the mapping.

use ropey::Rope;
use std::ops::Range;

/// Convert a UTF-16 offset into a byte offset (clamped to a char boundary).
pub fn utf16_to_utf8(text: &str, utf16_off: usize) -> usize {
    let mut units = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        if units >= utf16_off {
            return byte_idx;
        }
        units += ch.len_utf16();
        if units > utf16_off {
            // Offset points inside a surrogate pair: snap to the char start.
            return byte_idx;
        }
    }
    text.len()
}

/// Convert a byte offset into a UTF-16 offset.
pub fn utf8_to_utf16(text: &str, byte_off: usize) -> usize {
    text[..byte_off.min(text.len())]
        .chars()
        .map(char::len_utf16)
        .sum()
}

/// A live editing session on a text element.
///
/// The element's own `text` is only updated when the session is committed;
/// while editing, the canvas paints the session's rope content instead.
pub struct TextEditSession {
    pub rope: Rope,
    /// Caret selection as char offsets (anchor <= head after normalization).
    anchor: usize,
    head: usize,
    /// IME composition (marked text) range, as char offsets.
    pub marked: Option<Range<usize>>,
}

impl TextEditSession {
    pub fn new(text: &str) -> Self {
        let len = text.chars().count();
        Self {
            rope: Rope::from_str(text),
            anchor: len,
            head: len,
            marked: None,
        }
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    #[allow(dead_code)]
    pub fn caret(&self) -> usize {
        self.head
    }

    pub fn set_caret(&mut self, char_off: usize, extend: bool) {
        let off = char_off.min(self.len_chars());
        self.head = off;
        if !extend {
            self.anchor = off;
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.head = self.len_chars();
    }

    fn delete_range(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.rope.remove(range.clone());
        self.anchor = range.start;
        self.head = range.start;
    }

    /// Replace the current selection with text (typed input, paste).
    pub fn insert(&mut self, s: &str) {
        let sel = self.selection();
        self.delete_range(sel.clone());
        self.rope.insert(sel.start, s);
        let new_caret = sel.start + s.chars().count();
        self.anchor = new_caret;
        self.head = new_caret;
    }

    pub fn backspace(&mut self) {
        let sel = self.selection();
        if sel.is_empty() {
            if sel.start > 0 {
                self.delete_range(sel.start - 1..sel.start);
            }
        } else {
            self.delete_range(sel);
        }
    }

    pub fn delete_forward(&mut self) {
        let sel = self.selection();
        if sel.is_empty() {
            if sel.start < self.len_chars() {
                self.delete_range(sel.start..sel.start + 1);
            }
        } else {
            self.delete_range(sel);
        }
    }

    // --- caret movement -------------------------------------------------

    pub fn move_left(&mut self, extend: bool) {
        let sel = self.selection();
        if sel.is_empty() || extend {
            let to = self.head.saturating_sub(1);
            self.set_caret(to, extend);
        } else {
            self.set_caret(sel.start, false);
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        let sel = self.selection();
        if sel.is_empty() || extend {
            let to = (self.head + 1).min(self.len_chars());
            self.set_caret(to, extend);
        } else {
            self.set_caret(sel.end, false);
        }
    }

    pub fn move_home(&mut self, extend: bool) {
        let line = self.rope.char_to_line(self.head.min(self.len_chars()));
        let start = self.rope.line_to_char(line);
        self.set_caret(start, extend);
    }

    pub fn move_end(&mut self, extend: bool) {
        let line = self.rope.char_to_line(self.head.min(self.len_chars()));
        let mut end = self.rope.line_to_char(line + 1).min(self.len_chars());
        // Exclude the trailing newline.
        if end > 0 && self.rope.char(end - 1) == '\n' {
            end -= 1;
        }
        self.set_caret(end, extend);
    }

    /// Vertical caret position within the text, for up/down movement.
    fn line_and_column(&self, char_off: usize) -> (usize, usize) {
        let line = self.rope.char_to_line(char_off.min(self.len_chars()));
        (line, char_off - self.rope.line_to_char(line))
    }

    pub fn move_vertical(&mut self, delta_lines: isize, extend: bool) {
        let (line, col) = self.line_and_column(self.head);
        let last_line = self.rope.len_lines().saturating_sub(1);
        let target_line = (line as isize + delta_lines).clamp(0, last_line as isize) as usize;
        let line_start = self.rope.line_to_char(target_line);
        let mut line_len = self.rope.line(target_line).len_chars();
        if line_len > 0 && self.rope.line(target_line).char(line_len - 1) == '\n' {
            line_len -= 1;
        }
        self.set_caret(line_start + col.min(line_len), extend);
    }

    // --- UTF-16 bridge for EntityInputHandler ---------------------------

    pub fn utf16_selection(&self) -> Range<usize> {
        let text = self.text();
        let sel = self.selection();
        utf8_to_utf16(&text, char_to_byte(&self.rope, sel.start))
            ..utf8_to_utf16(&text, char_to_byte(&self.rope, sel.end))
    }

    pub fn replace_utf16_range(&mut self, range: Option<Range<usize>>, text: &str) {
        let full = self.text();
        let char_range = match range {
            Some(r) => {
                let start_byte = utf16_to_utf8(&full, r.start);
                let end_byte = utf16_to_utf8(&full, r.end);
                self.rope.byte_to_char(start_byte)..self.rope.byte_to_char(end_byte)
            }
            // When IME commits a composition it passes None: replace the
            // current marked (composition) region, not the caret.
            None => self.marked.clone().unwrap_or_else(|| self.selection()),
        };
        self.set_caret(char_range.start, false);
        self.set_caret(char_range.end, true);
        self.insert(text);
        self.marked = None;
    }

    pub fn replace_and_mark_utf16_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        new_selected_utf16: Option<Range<usize>>,
    ) {
        let full = self.text();
        let char_range = match range {
            Some(r) => {
                let start_byte = utf16_to_utf8(&full, r.start);
                let end_byte = utf16_to_utf8(&full, r.end);
                self.rope.byte_to_char(start_byte)..self.rope.byte_to_char(end_byte)
            }
            // When IME updates a composition it passes None: replace the
            // previous marked region with the new composition string.
            None => self.marked.clone().unwrap_or_else(|| self.selection()),
        };
        // Delete the target range directly (without disturbing caret via
        // set_caret, which would move selection to the wrong place).
        if !char_range.is_empty() {
            self.rope.remove(char_range.clone());
        }
        self.rope.insert(char_range.start, text);
        let mark_start = char_range.start;
        let mark_end = mark_start + text.chars().count();
        self.marked = Some(mark_start..mark_end);
        match new_selected_utf16 {
            Some(r) => {
                // The new selection is relative to the start of `text`.
                let start = mark_start + utf16_units_to_chars(text, r.start);
                let end = mark_start + utf16_units_to_chars(text, r.end);
                self.anchor = start;
                self.head = end;
            }
            None => {
                self.anchor = mark_end;
                self.head = mark_end;
            }
        }
    }
}

/// Helper: char offset of the given byte offset.
pub fn char_to_byte(rope: &Rope, char_off: usize) -> usize {
    rope.char_to_byte(char_off.min(rope.len_chars()))
}

/// Convert a UTF-16 offset within `s` to a char offset.
fn utf16_units_to_chars(s: &str, utf16_off: usize) -> usize {
    let mut units = 0usize;
    for (i, ch) in s.chars().enumerate() {
        if units >= utf16_off {
            return i;
        }
        units += ch.len_utf16();
    }
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_mapping_handles_cjk_and_emoji() {
        let s = "ab你好🦀cd";
        // utf16: a(1) b(1) 你(1) 好(1) 🦀(2) c(1) d(1) = 8 units
        assert_eq!(utf8_to_utf16(s, s.len()), 8);
        let crab_byte = s.find('🦀').unwrap();
        assert_eq!(utf8_to_utf16(s, crab_byte), 4);
        assert_eq!(utf16_to_utf8(s, 4), crab_byte);
        // Inside the surrogate pair snaps to char start.
        assert_eq!(utf16_to_utf8(s, 5), crab_byte);
        assert_eq!(utf16_to_utf8(s, 100), s.len());
    }

    #[test]
    fn session_editing_basics() {
        let mut s = TextEditSession::new("hello");
        assert_eq!(s.caret(), 5);
        s.insert(" world");
        assert_eq!(s.text(), "hello world");
        // caret at end; move left once -> between 'l' and 'd'.
        s.move_left(false);
        s.backspace();
        assert_eq!(s.text(), "hello word");
        s.set_caret(0, false);
        s.delete_forward();
        assert_eq!(s.text(), "ello word");
        s.select_all();
        s.insert("你好");
        assert_eq!(s.text(), "你好");
    }

    #[test]
    fn session_vertical_movement() {
        let mut s = TextEditSession::new("abc\ndefgh\nij");
        s.set_caret(1, false); // line 0, col 1
        s.move_vertical(1, false);
        assert_eq!(s.caret(), 5); // line 1, col 1
        s.move_vertical(1, false);
        assert_eq!(s.caret(), 11); // line 2, col 1
        s.move_vertical(-2, false);
        assert_eq!(s.caret(), 1);
    }

    #[test]
    fn utf16_replace_roundtrip() {
        let mut s = TextEditSession::new("你好世界");
        // Replace "好世" (utf16 range 1..3) with "ABC".
        s.replace_utf16_range(Some(1..3), "ABC");
        assert_eq!(s.text(), "你ABC界");
        assert_eq!(s.utf16_selection(), 4..4);
    }

    #[test]
    fn ime_composition_replaces_marked() {
        // Simulate pinyin input "ni" → "nihao": each replace_and_mark with
        // None range must replace the previous marked region, not append.
        let mut s = TextEditSession::new("");
        // 1st keystroke: mark "n" (caret at end of composition = 1..1).
        s.replace_and_mark_utf16_range(None, "n", Some(1..1));
        assert_eq!(s.text(), "n");
        assert_eq!(s.marked, Some(0..1));
        // 2nd: "n" → "ni", caret at 2.
        s.replace_and_mark_utf16_range(None, "ni", Some(2..2));
        assert_eq!(s.text(), "ni"); // NOT "nni"
        assert_eq!(s.marked, Some(0..2));
        // 3rd: "ni" → "niha", caret at 4.
        s.replace_and_mark_utf16_range(None, "niha", Some(4..4));
        assert_eq!(s.text(), "niha"); // NOT "niniha"
        assert_eq!(s.marked, Some(0..4));
        // User confirms the candidate: replace marked with "你好".
        s.replace_utf16_range(None, "你好");
        assert_eq!(s.text(), "你好");
        assert_eq!(s.marked, None);
    }
}
