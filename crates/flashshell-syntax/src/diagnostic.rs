use std::fmt::{self, Write};

use crate::{SourceFile, Span, SpanError};

/// User-facing diagnostic severity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl fmt::Display for Severity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Note => "note",
        })
    }
}

/// Whether a source label marks the direct cause or related context.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LabelStyle {
    Primary,
    Secondary,
}

/// A source range and its diagnostic annotation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Label {
    style: LabelStyle,
    span: Span,
    message: String,
}

impl Label {
    #[must_use]
    pub const fn style(&self) -> LabelStyle {
        self.style
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Structured diagnostic data independent of presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    severity: Severity,
    code: String,
    message: String,
    labels: Vec<Label>,
    notes: Vec<String>,
}

impl Diagnostic {
    pub fn new(severity: Severity, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity,
            code: code.into(),
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[must_use]
    pub const fn severity(&self) -> Severity {
        self.severity
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    #[must_use]
    pub fn with_primary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            style: LabelStyle::Primary,
            span,
            message: message.into(),
        });
        self
    }

    #[must_use]
    pub fn with_secondary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            style: LabelStyle::Secondary,
            span,
            message: message.into(),
        });
        self
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

/// A failure to turn structured diagnostic data into plain text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderError {
    MissingPrimaryLabel,
    InvalidSpan(SpanError),
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrimaryLabel => formatter.write_str("diagnostic has no primary label"),
            Self::InvalidSpan(error) => write!(formatter, "invalid diagnostic label: {error}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<SpanError> for RenderError {
    fn from(error: SpanError) -> Self {
        Self::InvalidSpan(error)
    }
}

/// Renders a diagnostic with source excerpts and deterministic ASCII markers.
pub fn render_diagnostic(
    source: &SourceFile,
    diagnostic: &Diagnostic,
) -> Result<String, RenderError> {
    let primary = diagnostic
        .labels
        .iter()
        .find(|label| label.style == LabelStyle::Primary)
        .ok_or(RenderError::MissingPrimaryLabel)?;
    source.validate_span(primary.span)?;
    let primary_location = source.location(primary.span.start())?;

    for label in &diagnostic.labels {
        source.validate_span(label.span)?;
    }

    let gutter_width = diagnostic
        .labels
        .iter()
        .map(|label| source.location(label.span.start()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|location| decimal_width(location.line()))
        .max()
        .unwrap_or(1);

    let mut output = String::new();
    writeln!(
        output,
        "{}[{}]: {}",
        diagnostic.severity, diagnostic.code, diagnostic.message
    )
    .expect("writing to a String cannot fail");
    writeln!(
        output,
        " --> {}:{}:{}",
        source.name(),
        primary_location.line(),
        primary_location.column()
    )
    .expect("writing to a String cannot fail");
    writeln!(output, "  |").expect("writing to a String cannot fail");

    for label in &diagnostic.labels {
        render_label(&mut output, source, label, gutter_width)?;
    }
    for note in &diagnostic.notes {
        writeln!(output, "  = note: {note}").expect("writing to a String cannot fail");
    }

    Ok(output)
}

fn render_label(
    output: &mut String,
    source: &SourceFile,
    label: &Label,
    gutter_width: usize,
) -> Result<(), RenderError> {
    let location = source.location(label.span.start())?;
    let line_number = location.line();
    let line_text = source
        .line_text(line_number)
        .expect("a validated source location must identify an indexed line");
    let line_start = source
        .line_start(line_number)
        .expect("a validated source location must identify an indexed line");
    let line_end = source
        .line_content_end(line_number)
        .expect("a validated source location must identify an indexed line");

    writeln!(output, "{line_number:>gutter_width$} | {line_text}")
        .expect("writing to a String cannot fail");

    let prefix = marker_prefix(&source.text()[line_start..label.span.start().min(line_end)]);
    let marked_end = label.span.end().min(line_end);
    let marker_count = if label.span.is_empty() || marked_end <= label.span.start() {
        1
    } else {
        source.text()[label.span.start()..marked_end]
            .chars()
            .count()
            .max(1)
    };
    let marker = match label.style {
        LabelStyle::Primary => '^',
        LabelStyle::Secondary => '-',
    };
    let markers: String = std::iter::repeat_n(marker, marker_count).collect();
    let annotation = if label.message.is_empty() {
        String::new()
    } else {
        format!(" {}", label.message)
    };

    writeln!(
        output,
        "{:>gutter_width$} | {prefix}{markers}{annotation}",
        ""
    )
    .expect("writing to a String cannot fail");
    Ok(())
}

fn marker_prefix(text: &str) -> String {
    text.chars()
        .map(|character| if character == '\t' { '\t' } else { ' ' })
        .collect()
}

fn decimal_width(value: usize) -> usize {
    value.to_string().len()
}
