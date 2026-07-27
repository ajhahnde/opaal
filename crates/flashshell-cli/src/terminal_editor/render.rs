//! Redrawing of one physical input line.
//!
//! Content that outgrows the terminal width scrolls horizontally inside its
//! single row rather than wrapping: wrapping needs row-count arithmetic
//! against an unknown console, while a scroll window keeps the cursor
//! placement exact. One column per character is assumed.

/// Build the escape sequence that redraws `text` under `prompt`.
///
/// `cursor_chars` counts the characters before the cursor. The result returns
/// to column one, erases the row, writes the visible window, and places the
/// cursor with an absolute column request.
#[must_use]
pub fn render_line(prompt: &str, text: &str, cursor_chars: usize, columns: u16) -> String {
    let width = usize::from(columns.max(1));
    let prompt_width = prompt.chars().count();
    // Leave at least one cell for the cursor itself.
    let available = width.saturating_sub(prompt_width).max(1);

    let characters: Vec<char> = text.chars().collect();
    let cursor = cursor_chars.min(characters.len());
    // Anchor the window on the cursor so it is always on screen.
    let start = cursor.saturating_sub(available.saturating_sub(1));
    let end = characters.len().min(start + available);
    let visible: String = characters[start..end].iter().collect();

    let column = prompt_width + (cursor - start) + 1;
    format!("\r\x1b[K{prompt}{visible}\r\x1b[{column}G")
}
