//! The editable UTF-8 source submission.

use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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

    /// The number of grapheme clusters before the cursor.
    #[must_use]
    pub fn cursor_chars(&self) -> usize {
        self.text[..self.cursor].graphemes(true).count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The logical line containing the cursor, its local byte cursor, and
    /// whether it is the first line of the submission.
    #[must_use]
    pub fn current_line(&self) -> (&str, usize, bool) {
        let start = self.line_start();
        let end = self.line_end();
        (&self.text[start..end], self.cursor - start, start == 0)
    }

    pub fn insert(&mut self, value: char) {
        self.text.insert(self.cursor, value);
        self.cursor += value.len_utf8();
    }

    /// Replace one exact UTF-8 range and leave the cursor after the insertion.
    pub fn replace(&mut self, range: Range<usize>, value: &str) -> bool {
        if range.start > range.end
            || range.end > self.text.len()
            || !self.text.is_char_boundary(range.start)
            || !self.text.is_char_boundary(range.end)
        {
            return false;
        }
        self.text.replace_range(range.clone(), value);
        self.cursor = range.start + value.len();
        true
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
        self.cursor = self.line_start();
    }

    pub fn move_end(&mut self) {
        self.cursor = self.line_end();
    }

    /// Move to the closest display column on the preceding logical line.
    pub fn move_up(&mut self) -> bool {
        let start = self.line_start();
        let Some(previous_end) = start.checked_sub(1) else {
            return false;
        };
        let previous_start = self.text[..previous_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let column = UnicodeWidthStr::width(&self.text[start..self.cursor]);
        self.cursor = boundary_at_display_column(&self.text, previous_start, previous_end, column);
        true
    }

    /// Move to the closest display column on the following logical line.
    pub fn move_down(&mut self) -> bool {
        let end = self.line_end();
        if end == self.text.len() {
            return false;
        }
        let next_start = end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |index| next_start + index);
        let column = UnicodeWidthStr::width(&self.text[self.line_start()..self.cursor]);
        self.cursor = boundary_at_display_column(&self.text, next_start, next_end, column);
        true
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
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map(|value| self.cursor + value.len())
    }

    fn previous_character(&self) -> Option<char> {
        self.text[..self.cursor].chars().next_back()
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index)
    }
}

fn boundary_at_display_column(text: &str, start: usize, end: usize, target: usize) -> usize {
    let mut boundary = start;
    let mut column: usize = 0;
    for (offset, grapheme) in text[start..end].grapheme_indices(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if column.saturating_add(width) > target {
            break;
        }
        column = column.saturating_add(width);
        boundary = start + offset + grapheme.len();
    }
    boundary
}
