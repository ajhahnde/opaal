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
    // Anchor the window on the cursor so it is always on screen, but hold a
    // quarter of the row in reserve to its right: a window ending exactly at
    // the cursor hides every character being edited around, which makes
    // mid-line editing on a long line blind. The margin never exceeds the text
    // actually following the cursor, so a line ending under the cursor still
    // fills the window.
    let margin = (available / 4).min(characters.len() - cursor);
    let start = (cursor + margin).saturating_sub(available.saturating_sub(1));
    let end = characters.len().min(start + available);
    // This row is drawn with an absolute column request, so a control character
    // reaching the terminal would move the cursor out from under it and leave
    // the rows above unerased. Recalling a multi-line submission is the path
    // that produces one today; substituting keeps every character one column
    // wide, so the column arithmetic below is unaffected. Only the drawing is
    // sanitized — the stored and submitted text keep their newlines.
    let visible: String = characters[start..end]
        .iter()
        .map(|value| if value.is_control() { ' ' } else { *value })
        .collect();

    let column = prompt_width + (cursor - start) + 1;
    format!("\r\x1b[K{prompt}{visible}\r\x1b[{column}G")
}
