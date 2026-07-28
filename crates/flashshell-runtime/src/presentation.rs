//! Width-aware, host-free presentation of structured runtime values.
//!
//! This module formats already-materialized values for an interactive terminal
//! sink. Its output is intentionally not a serialization format.

use std::error::Error;
use std::fmt;
use std::fmt::Write as _;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::command::Carrier;
use crate::{Table, Value};

const COLUMN_SEPARATOR: &str = " | ";
const DIVIDER_SEPARATOR: &str = "-+-";
const TABLE_MARKER: &str = "(table)";
const EMPTY_TABLE_MARKER: &str = "(empty table)";

#[derive(Clone, Copy)]
enum Alignment {
    Left,
    Right,
}

struct Cell {
    text: String,
    alignment: Alignment,
}

/// The final destination selected for one pipeline's output carrier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputDestination {
    /// Human-facing standard output with its current cell width.
    InteractiveTerminal { columns: usize },
    /// A file or other descriptor selected by an output redirection.
    Redirected,
    /// A command-substitution or explicit capture sink.
    Captured,
    /// An external process consuming a pipeline edge.
    ExternalProcess,
    /// An inherited output sink that is not an interactive terminal.
    NonInteractive,
}

impl fmt::Display for OutputDestination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InteractiveTerminal { .. } => formatter.write_str("an interactive terminal"),
            Self::Redirected => formatter.write_str("redirected output"),
            Self::Captured => formatter.write_str("command capture"),
            Self::ExternalProcess => formatter.write_str("an external process"),
            Self::NonInteractive => formatter.write_str("noninteractive output"),
        }
    }
}

/// A structured carrier whose destination requires explicit serialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationError {
    carrier: Carrier,
    destination: OutputDestination,
}

impl PresentationError {
    /// The final structured carrier that was refused.
    #[must_use]
    pub const fn carrier(&self) -> Carrier {
        self.carrier
    }

    /// The byte-oriented destination that cannot consume human rendering.
    #[must_use]
    pub const fn destination(&self) -> OutputDestination {
        self.destination
    }
}

impl fmt::Display for PresentationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} output cannot use {}; add an explicit `encode`/`to` boundary because \
             terminal rendering is not serialization",
            self.carrier, self.destination
        )
    }
}

impl Error for PresentationError {}

/// Proof that human presentation was selected for an interactive terminal.
///
/// Construction stays private to [`select_terminal_presentation`], preventing a
/// noninteractive destination from reaching a rendering function by accident.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalPresentation {
    columns: usize,
}

impl TerminalPresentation {
    /// The terminal width in character cells at selection time.
    #[must_use]
    pub const fn columns(self) -> usize {
        self.columns
    }
}

/// Selects human presentation only for a final structured terminal carrier.
///
/// `ByteStream` and `Empty` return `Ok(None)` at every destination: final bytes
/// are written directly and never decoded for display. Structured carriers
/// return a presentation token only at an interactive terminal; every other
/// destination requires an explicit serializer.
pub fn select_terminal_presentation(
    carrier: Carrier,
    destination: OutputDestination,
) -> Result<Option<TerminalPresentation>, PresentationError> {
    match carrier {
        Carrier::Empty | Carrier::ByteStream => Ok(None),
        Carrier::Value | Carrier::ValueStream => match destination {
            OutputDestination::InteractiveTerminal { columns } => {
                Ok(Some(TerminalPresentation { columns }))
            }
            destination => Err(PresentationError {
                carrier,
                destination,
            }),
        },
    }
}

/// Renders an already-materialized table within a terminal-cell width.
///
/// The returned text has no trailing newline. Width zero returns an empty
/// string. When the width cannot hold the table's minimum frame, a clipped
/// marker is returned instead of overflowing.
#[must_use]
pub fn render_table(table: &Table, terminal_columns: usize) -> String {
    if terminal_columns == 0 {
        return String::new();
    }
    if table.columns().is_empty() {
        return truncate_to_width(EMPTY_TABLE_MARKER, terminal_columns);
    }

    let separator_width = COLUMN_SEPARATOR.len() * (table.columns().len() - 1);
    let minimum_width = table.columns().len() + separator_width;
    if terminal_columns < minimum_width {
        return truncate_to_width(TABLE_MARKER, terminal_columns);
    }

    let headers: Vec<String> = table
        .columns()
        .iter()
        .map(|column| escape_controls(column))
        .collect();
    let rows: Vec<Vec<Cell>> = table
        .rows()
        .iter()
        .map(|row| row.iter().map(cell).collect())
        .collect();

    let mut natural_widths: Vec<usize> = headers
        .iter()
        .map(|header| UnicodeWidthStr::width(header.as_str()).max(1))
        .collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            natural_widths[index] =
                natural_widths[index].max(UnicodeWidthStr::width(cell.text.as_str()));
        }
    }

    let content_budget = terminal_columns - separator_width;
    let widths = allocate_widths(&natural_widths, content_budget);
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(render_cells(
        headers
            .iter()
            .map(|text| Cell {
                text: text.clone(),
                alignment: Alignment::Left,
            })
            .collect::<Vec<_>>()
            .as_slice(),
        &widths,
    ));
    lines.push(render_divider(&widths));
    lines.extend(rows.iter().map(|row| render_cells(row, &widths)));
    lines.join("\n")
}

fn cell(value: &Value) -> Cell {
    let alignment = match value {
        Value::Int(_) | Value::Float(_) | Value::Duration(_) | Value::ByteSize(_) => {
            Alignment::Right
        }
        _ => Alignment::Left,
    };
    Cell {
        text: escape_controls(&value.to_string()),
        alignment,
    }
}

fn allocate_widths(natural: &[usize], content_budget: usize) -> Vec<usize> {
    let natural_total: usize = natural.iter().sum();
    if natural_total <= content_budget {
        return natural.to_vec();
    }

    let mut widths = vec![1; natural.len()];
    let mut remaining = content_budget - widths.len();
    while remaining != 0 {
        let mut grew = false;
        for (width, limit) in widths.iter_mut().zip(natural) {
            if remaining == 0 {
                break;
            }
            if *width < *limit {
                *width += 1;
                remaining -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    widths
}

fn render_cells(cells: &[Cell], widths: &[usize]) -> String {
    let mut output = String::new();
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        if index != 0 {
            output.push_str(COLUMN_SEPARATOR);
        }
        let text = truncate_to_width(&cell.text, *width);
        let padding = width - UnicodeWidthStr::width(text.as_str());
        if matches!(cell.alignment, Alignment::Right) {
            output.extend(std::iter::repeat_n(' ', padding));
            output.push_str(&text);
        } else {
            output.push_str(&text);
            output.extend(std::iter::repeat_n(' ', padding));
        }
    }
    output
}

fn render_divider(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join(DIVIDER_SEPARATOR)
}

fn truncate_to_width(text: &str, maximum: usize) -> String {
    if maximum == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= maximum {
        return text.to_owned();
    }

    let content_limit = maximum - 1;
    let mut output = String::new();
    let mut width = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_limit {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push('…');
    output
}

fn escape_controls(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\0' => escaped.push_str("\\0"),
            character if character.is_control() => {
                write!(escaped, "\\u{{{:X}}}", u32::from(character))
                    .expect("writing to a String cannot fail");
            }
            character => escaped.push(character),
        }
    }
    escaped
}
