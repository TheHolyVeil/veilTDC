//! The search box's text and cursor. UTF-8 safe: the cursor is a byte offset
//! that always sits on a character boundary, and every edit keeps it there.

/// Longest query kept; anything typed or pasted past it is dropped.
pub const MAX_BYTES: usize = 256;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    cursor: usize,
}

impl LineEdit {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the cursor, always on a character boundary.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Type text at the cursor. Control characters are dropped, newlines
    /// become spaces, and the total length is capped.
    pub fn insert(&mut self, s: &str) {
        for c in s.chars() {
            let c = if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c };
            if c.is_control() || self.text.len() + c.len_utf8() > MAX_BYTES {
                continue;
            }
            self.text.insert(self.cursor, c);
            self.cursor += c.len_utf8();
        }
    }

    pub fn backspace(&mut self) -> bool {
        let Some(prev) = self.prev_boundary(self.cursor) else { return false };
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        true
    }

    pub fn delete(&mut self) -> bool {
        let Some(next) = self.next_boundary(self.cursor) else { return false };
        self.text.drain(self.cursor..next);
        true
    }

    pub fn left(&mut self) -> bool {
        self.prev_boundary(self.cursor).map(|p| self.cursor = p).is_some()
    }

    pub fn right(&mut self) -> bool {
        self.next_boundary(self.cursor).map(|p| self.cursor = p).is_some()
    }

    pub fn home(&mut self) -> bool {
        std::mem::replace(&mut self.cursor, 0) != 0
    }

    pub fn end(&mut self) -> bool {
        std::mem::replace(&mut self.cursor, self.text.len()) != self.text.len()
    }

    /// Start of the word before the cursor (Ctrl+Left).
    pub fn word_left(&mut self) -> bool {
        let to = self.word_start_before(self.cursor);
        std::mem::replace(&mut self.cursor, to) != to
    }

    /// End of the word after the cursor (Ctrl+Right).
    pub fn word_right(&mut self) -> bool {
        let rest = &self.text[self.cursor..];
        let skipped = rest.len() - rest.trim_start_matches(|c: char| !c.is_alphanumeric()).len();
        let rest = &rest[skipped..];
        let word = rest.len() - rest.trim_start_matches(char::is_alphanumeric).len();
        let to = self.cursor + skipped + word;
        std::mem::replace(&mut self.cursor, to) != to
    }

    /// Delete back to the start of the word (Ctrl+W, Ctrl+Backspace).
    pub fn delete_word_back(&mut self) -> bool {
        let from = self.word_start_before(self.cursor);
        if from == self.cursor {
            return false;
        }
        self.text.drain(from..self.cursor);
        self.cursor = from;
        true
    }

    /// Clear everything (Ctrl+U).
    pub fn clear(&mut self) -> bool {
        let changed = !self.text.is_empty();
        self.text.clear();
        self.cursor = 0;
        changed
    }

    fn prev_boundary(&self, from: usize) -> Option<usize> {
        self.text[..from].char_indices().next_back().map(|(i, _)| i)
    }

    fn next_boundary(&self, from: usize) -> Option<usize> {
        self.text[from..].chars().next().map(|c| from + c.len_utf8())
    }

    fn word_start_before(&self, from: usize) -> usize {
        let head = &self.text[..from];
        let trimmed = head.trim_end_matches(|c: char| !c.is_alphanumeric());
        let word = trimmed.trim_end_matches(char::is_alphanumeric);
        word.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(s: &str) -> LineEdit {
        let mut e = LineEdit::default();
        e.insert(s);
        e
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut e = edit("helo");
        e.left();
        e.insert("l");
        assert_eq!((e.text(), e.cursor()), ("hello", 4));
        e.home();
        e.insert(">");
        assert_eq!(e.text(), ">hello");
    }

    #[test]
    fn multibyte_characters_are_never_split() {
        let mut e = edit("aé✓😀z");
        for expected in ["aé✓😀", "aé✓", "aé", "a", ""] {
            assert!(e.backspace());
            assert_eq!(e.text(), expected);
            assert!(e.text().is_char_boundary(e.cursor()));
        }
        assert!(!e.backspace() && !e.delete() && !e.left() && !e.right());
        // walking across multibyte characters never lands inside one
        let mut e = edit("é✓😀");
        while e.left() {
            assert!(e.text().is_char_boundary(e.cursor()));
        }
        assert_eq!(e.cursor(), 0);
        while e.right() {
            assert!(e.text().is_char_boundary(e.cursor()));
        }
        assert_eq!(e.cursor(), e.text().len());
    }

    #[test]
    fn cursor_movement_reports_whether_anything_moved() {
        let mut e = edit("ab");
        assert!(!e.right(), "already at the end");
        assert!(e.left() && e.left());
        assert!(!e.left(), "already at the start");
        assert!(!e.home());
        assert!(e.end());
        assert!(!e.end());
    }

    #[test]
    fn delete_removes_the_character_under_the_cursor() {
        let mut e = edit("a✓b");
        e.home();
        e.right();
        assert!(e.delete());
        assert_eq!((e.text(), e.cursor()), ("ab", 1));
        e.end();
        assert!(!e.delete());
    }

    #[test]
    fn word_movement_and_deletion() {
        let mut e = edit("foo bar-baz  qux");
        e.word_left();
        assert_eq!(&e.text()[e.cursor()..], "qux");
        e.word_left();
        assert_eq!(&e.text()[e.cursor()..], "baz  qux");
        e.word_right();
        assert_eq!(&e.text()[..e.cursor()], "foo bar-baz");
        e.end();
        assert!(e.delete_word_back());
        assert_eq!(e.text(), "foo bar-baz  ");
        assert!(e.delete_word_back());
        assert_eq!(e.text(), "foo bar-");
        assert!(e.delete_word_back());
        assert_eq!(e.text(), "foo ");
        assert!(e.delete_word_back());
        assert!(e.is_empty());
        assert!(!e.delete_word_back());
    }

    #[test]
    fn clear_empties_and_reports_a_change_only_when_there_was_one() {
        let mut e = edit("x");
        assert!(e.clear());
        assert!(!e.clear());
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn control_characters_are_dropped_and_whitespace_normalised() {
        let mut e = LineEdit::default();
        e.insert("a\u{0}b\u{1b}c\nd\te\r");
        assert_eq!(e.text(), "abc d e ");
    }

    #[test]
    fn length_is_capped_on_a_character_boundary() {
        let mut e = LineEdit::default();
        e.insert(&"x".repeat(1000));
        assert_eq!(e.text().len(), MAX_BYTES);
        let mut e = LineEdit::default();
        e.insert(&"✓".repeat(1000)); // 3 bytes each
        assert!(e.text().len() <= MAX_BYTES && e.text().len() % 3 == 0);
        assert!(e.text().is_char_boundary(e.cursor()));
    }
}
