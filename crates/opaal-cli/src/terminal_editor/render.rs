//! Display-cell-correct redrawing of one physical input line.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::editor::EditorPrompt;
use crate::highlight::{HighlightKind, HighlightSegment};

/// Build the escape sequence that redraws `text` under `prompt`.
///
/// `cursor` is a UTF-8 byte boundary. The result returns
/// to column one, erases the row, writes the visible window, and places the
/// cursor with an absolute column request.
#[must_use]
pub fn render_line(prompt: &str, text: &str, cursor: usize, columns: u16) -> String {
    let view = line_view(prompt, text, cursor, columns);
    format!(
        "\r\x1b[K{prompt}{}\r\x1b[{}G",
        view.visible, view.cursor_column
    )
}

/// Redraw a complete multiline submission and restore its logical cursor.
///
/// `previous_cursor_row` is the zero-based row where the preceding draw left
/// the cursor. It lets the next draw return to the submission's first row
/// before clearing and rebuilding all continuation lines.
#[must_use]
pub fn render_submission(
    prompt: &EditorPrompt,
    text: &str,
    cursor: usize,
    columns: u16,
    previous_cursor_row: usize,
    source_len: usize,
    highlights: &[HighlightSegment],
) -> (String, usize) {
    let cursor = cursor.min(text.len());
    let cursor_row = text[..cursor].bytes().filter(|byte| *byte == b'\n').count();
    let lines = text.split('\n').collect::<Vec<_>>();
    let mut rendered = String::new();
    rendered.push('\r');
    if previous_cursor_row > 0 {
        rendered.push_str(&format!("\x1b[{previous_cursor_row}A"));
    }
    rendered.push_str("\x1b[J");

    let mut line_start = 0;
    let mut cursor_column = 1;
    for (index, line) in lines.iter().enumerate() {
        let active_prompt = if index == 0 {
            prompt.primary()
        } else {
            prompt.continuation()
        };
        let local_cursor = if index == cursor_row {
            cursor.saturating_sub(line_start).min(line.len())
        } else {
            0
        };
        let view = line_view(active_prompt, line, local_cursor, columns);
        rendered.push_str(active_prompt);
        let visible_range = line_start + view.visible_start..line_start + view.visible_end;
        rendered.push_str(&style_visible(text, visible_range, source_len, highlights));
        if index == cursor_row {
            cursor_column = view.cursor_column;
        }
        if index + 1 < lines.len() {
            rendered.push_str("\r\n");
        }
        line_start = line_start.saturating_add(line.len() + 1);
    }

    let rows_after_cursor = lines.len().saturating_sub(cursor_row + 1);
    rendered.push('\r');
    if rows_after_cursor > 0 {
        rendered.push_str(&format!("\x1b[{rows_after_cursor}A"));
    }
    rendered.push_str(&format!("\x1b[{cursor_column}G"));
    (rendered, cursor_row)
}

struct LineView {
    visible: String,
    cursor_column: usize,
    visible_start: usize,
    visible_end: usize,
}

fn line_view(prompt: &str, text: &str, cursor: usize, columns: u16) -> LineView {
    let width = usize::from(columns.max(1));
    let prompt_width = UnicodeWidthStr::width(prompt);
    // Leave at least one cell for the cursor itself.
    let available = width.saturating_sub(prompt_width).max(1);

    let cursor = cursor.min(text.len());
    let cursor = if text.is_char_boundary(cursor) {
        cursor
    } else {
        text.floor_char_boundary(cursor)
    };
    let graphemes = text
        .grapheme_indices(true)
        .map(|(start, grapheme)| {
            let sanitized = grapheme
                .chars()
                .map(|value| if value.is_control() { ' ' } else { value })
                .collect::<String>();
            let cells = UnicodeWidthStr::width(sanitized.as_str());
            (start, start + grapheme.len(), sanitized, cells)
        })
        .collect::<Vec<_>>();
    let cursor_cells = graphemes
        .iter()
        .take_while(|(_, end, _, _)| *end <= cursor)
        .map(|entry| entry.3)
        .sum::<usize>();
    let total_cells = graphemes.iter().map(|entry| entry.3).sum::<usize>();
    // Anchor the window on the cursor so it is always on screen, but hold a
    // quarter of the row in reserve to its right: a window ending exactly at
    // the cursor hides every character being edited around, which makes
    // mid-line editing on a long line blind. The margin never exceeds the text
    // actually following the cursor, so a line ending under the cursor still
    // fills the window.
    let margin = (available / 4).min(total_cells.saturating_sub(cursor_cells));
    let start_cell = (cursor_cells + margin).saturating_sub(available.saturating_sub(1));
    let visible_budget = if cursor == text.len() {
        available.saturating_sub(1)
    } else {
        available
    };
    // This row is drawn with an absolute column request, so a control character
    // reaching the terminal would move the cursor out from under it and leave
    // the rows above unerased. Recalling a multi-line submission is the path
    // that produces one today; substituting keeps every character one column
    // wide, so the column arithmetic below is unaffected. Only the drawing is
    // sanitized — the stored and submitted text keep their newlines.
    let mut visible = String::new();
    let mut visible_cells: usize = 0;
    let mut skipped_cells: usize = 0;
    let mut started = false;
    let mut visible_start = 0;
    let mut visible_end = 0;
    for (start, end, sanitized, cells) in &graphemes {
        if !started && skipped_cells.saturating_add(*cells) <= start_cell {
            skipped_cells = skipped_cells.saturating_add(*cells);
            continue;
        }
        if visible_cells.saturating_add(*cells) > visible_budget {
            break;
        }
        if !started {
            visible_start = *start;
            started = true;
        }
        visible.push_str(sanitized);
        visible_cells = visible_cells.saturating_add(*cells);
        visible_end = *end;
    }

    let cursor_column = prompt_width + cursor_cells.saturating_sub(skipped_cells) + 1;
    LineView {
        visible,
        cursor_column,
        visible_start,
        visible_end,
    }
}

fn style_visible(
    display: &str,
    range: std::ops::Range<usize>,
    source_len: usize,
    highlights: &[HighlightSegment],
) -> String {
    if range.is_empty() {
        return String::new();
    }
    let mut rendered = String::new();
    let source_end = range.end.min(source_len);
    let mut offset = 0;
    for segment in highlights {
        let segment_start = offset;
        let segment_end = offset + segment.text().len();
        offset = segment_end;
        let start = segment_start.max(range.start);
        let end = segment_end.min(source_end);
        if start >= end {
            continue;
        }
        let text = sanitize(&display[start..end]);
        let style = ansi_style(segment.kind());
        if style.is_empty() {
            rendered.push_str(&text);
        } else {
            rendered.push_str(style);
            rendered.push_str(&text);
            rendered.push_str("\x1b[0m");
        }
    }
    if range.end > source_len {
        let start = range.start.max(source_len);
        if start < range.end {
            rendered.push_str("\x1b[2m");
            rendered.push_str(&sanitize(&display[start..range.end]));
            rendered.push_str("\x1b[0m");
        }
    }
    rendered
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|value| if value.is_control() { ' ' } else { value })
        .collect()
}

const fn ansi_style(kind: HighlightKind) -> &'static str {
    match kind {
        HighlightKind::Plain => "",
        HighlightKind::Comment => "\x1b[90;3m",
        HighlightKind::Keyword => "\x1b[35;1m",
        HighlightKind::Literal => "\x1b[33m",
        HighlightKind::String => "\x1b[32m",
        HighlightKind::Escape => "\x1b[93m",
        HighlightKind::Expansion => "\x1b[36;1m",
        HighlightKind::Operator => "\x1b[34m",
        HighlightKind::Delimiter => "\x1b[37;1m",
        HighlightKind::Invalid => "\x1b[31;4m",
    }
}
