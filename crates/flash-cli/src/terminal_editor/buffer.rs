//! The editable text of one physical input line.
//!
//! The cursor is a byte offset that is always kept on a character boundary.
//! Movement steps whole `char` values, not grapheme clusters: correcting that
//! would require a Unicode segmentation dependency this editor does not take.

/// One line of editable text and its cursor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EditBuffer {
    text: String,
    cursor: usize,
}

impl EditBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer holding `text` with the cursor at its end.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            cursor: text.len(),
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor as a byte offset into [`text`](Self::text).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The number of characters before the cursor — the render column.
    #[must_use]
    pub fn cursor_chars(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn insert(&mut self, value: char) {
        self.text.insert(self.cursor, value);
        self.cursor += value.len_utf8();
    }

    /// Remove the character before the cursor, reporting whether one existed.
    pub fn backspace(&mut self) -> bool {
        let Some(start) = self.previous_boundary() else {
            return false;
        };
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }

    /// Remove the character at the cursor, reporting whether one existed.
    pub fn delete(&mut self) -> bool {
        let Some(end) = self.next_boundary() else {
            return false;
        };
        self.text.replace_range(self.cursor..end, "");
        true
    }

    /// Step one character left, reporting whether the cursor moved.
    pub fn move_left(&mut self) -> bool {
        let Some(start) = self.previous_boundary() else {
            return false;
        };
        self.cursor = start;
        true
    }

    /// Step one character right, reporting whether the cursor moved.
    pub fn move_right(&mut self) -> bool {
        let Some(end) = self.next_boundary() else {
            return false;
        };
        self.cursor = end;
        true
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn kill_to_end(&mut self) {
        self.text.truncate(self.cursor);
    }

    pub fn kill_to_start(&mut self) {
        self.text.replace_range(..self.cursor, "");
        self.cursor = 0;
    }

    /// Remove any run of spaces before the cursor, then the word before it.
    pub fn kill_word_back(&mut self) {
        while self
            .previous_character()
            .is_some_and(|value| value.is_whitespace())
        {
            self.backspace();
        }
        while self
            .previous_character()
            .is_some_and(|value| !value.is_whitespace())
        {
            self.backspace();
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|value| self.cursor + value.len_utf8())
    }

    fn previous_character(&self) -> Option<char> {
        self.text[..self.cursor].chars().next_back()
    }
}
