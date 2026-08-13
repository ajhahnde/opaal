use std::fmt;
use std::ops::Range;
use std::string::FromUtf8Error;

/// Identifies one source file within a compilation or session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceId(u32);

impl SourceId {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A source-identified, half-open byte range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Span {
    source_id: SourceId,
    start: usize,
    end: usize,
}

impl Span {
    #[must_use]
    pub const fn source_id(self) -> SourceId {
        self.source_id
    }

    #[must_use]
    pub const fn start(self) -> usize {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> usize {
        self.end
    }

    #[must_use]
    pub const fn len(self) -> usize {
        self.end - self.start
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// A one-based source position.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LineColumn {
    line: usize,
    column: usize,
}

/// The code-unit convention used for zero-based text positions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
}

/// A zero-based line and code-unit position within source text.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TextPosition {
    line: usize,
    character: usize,
}

impl TextPosition {
    #[must_use]
    pub const fn new(line: usize, character: usize) -> Self {
        Self { line, character }
    }

    #[must_use]
    pub const fn line(self) -> usize {
        self.line
    }

    #[must_use]
    pub const fn character(self) -> usize {
        self.character
    }
}

/// A half-open range expressed as zero-based text positions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TextRange {
    start: TextPosition,
    end: TextPosition,
}

impl TextRange {
    #[must_use]
    pub const fn new(start: TextPosition, end: TextPosition) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub const fn start(self) -> TextPosition {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> TextPosition {
        self.end
    }
}

impl LineColumn {
    #[must_use]
    pub const fn line(self) -> usize {
        self.line
    }

    #[must_use]
    pub const fn column(self) -> usize {
        self.column
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Line {
    start: usize,
    content_end: usize,
}

/// Maps UTF-8 byte offsets to source lines and Unicode-scalar columns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineIndex {
    lines: Vec<Line>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut lines = Vec::new();
        let mut start = 0;

        for (newline, byte) in bytes.iter().copied().enumerate() {
            if byte != b'\n' {
                continue;
            }

            let content_end = if newline > start && bytes[newline - 1] == b'\r' {
                newline - 1
            } else {
                newline
            };
            lines.push(Line { start, content_end });
            start = newline + 1;
        }

        lines.push(Line {
            start,
            content_end: text.len(),
        });
        Self { lines }
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn location(&self, text: &str, offset: usize) -> Result<LineColumn, SpanError> {
        validate_offset(text, offset)?;

        let line_index = self
            .lines
            .partition_point(|line| line.start <= offset)
            .saturating_sub(1);
        let line = self.lines[line_index];
        let column_end = offset.min(line.content_end);
        let column = text[line.start..column_end].chars().count() + 1;

        Ok(LineColumn {
            line: line_index + 1,
            column,
        })
    }

    fn line(&self, one_based_line: usize) -> Option<Line> {
        one_based_line
            .checked_sub(1)
            .and_then(|index| self.lines.get(index).copied())
    }

    fn text_position(
        &self,
        text: &str,
        offset: usize,
        encoding: PositionEncoding,
    ) -> Result<TextPosition, PositionError> {
        if offset > text.len() {
            return Err(PositionError::ByteOutOfBounds {
                offset,
                len: text.len(),
            });
        }
        if !text.is_char_boundary(offset) {
            return Err(PositionError::NotUtf8Boundary { offset });
        }

        let line_index = self
            .lines
            .partition_point(|line| line.start <= offset)
            .saturating_sub(1);
        let line = self.lines[line_index];
        let prefix = &text[line.start..offset.min(line.content_end)];
        let character = match encoding {
            PositionEncoding::Utf8 => prefix.len(),
            PositionEncoding::Utf16 => prefix.encode_utf16().count(),
        };
        Ok(TextPosition::new(line_index, character))
    }

    fn byte_offset(
        &self,
        text: &str,
        position: TextPosition,
        encoding: PositionEncoding,
    ) -> Result<usize, PositionError> {
        let Some(line) = self.lines.get(position.line).copied() else {
            return Err(PositionError::LineOutOfBounds {
                line: position.line,
                line_count: self.lines.len(),
            });
        };
        let line_text = &text[line.start..line.content_end];

        match encoding {
            PositionEncoding::Utf8 => {
                if position.character > line_text.len() {
                    return Err(PositionError::CharacterOutOfBounds {
                        line: position.line,
                        character: position.character,
                        maximum: line_text.len(),
                    });
                }
                let offset = line.start + position.character;
                if !text.is_char_boundary(offset) {
                    return Err(PositionError::NotUtf8Boundary { offset });
                }
                Ok(offset)
            }
            PositionEncoding::Utf16 => {
                let mut code_units = 0;
                for (byte, scalar) in line_text.char_indices() {
                    if code_units == position.character {
                        return Ok(line.start + byte);
                    }
                    let next = code_units + scalar.len_utf16();
                    if position.character < next {
                        return Err(PositionError::InsideUtf16Scalar {
                            line: position.line,
                            character: position.character,
                        });
                    }
                    code_units = next;
                }
                if code_units == position.character {
                    Ok(line.content_end)
                } else {
                    Err(PositionError::CharacterOutOfBounds {
                        line: position.line,
                        character: position.character,
                        maximum: code_units,
                    })
                }
            }
        }
    }
}

/// Valid UTF-8 source text and its byte-to-line index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFile {
    id: SourceId,
    name: String,
    text: String,
    line_index: LineIndex,
}

impl SourceFile {
    pub fn new(id: SourceId, name: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        let line_index = LineIndex::new(&text);
        Self {
            id,
            name: name.into(),
            text,
            line_index,
        }
    }

    pub fn from_bytes(
        id: SourceId,
        name: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<Self, FromUtf8Error> {
        String::from_utf8(bytes).map(|text| Self::new(id, name, text))
    }

    #[must_use]
    pub const fn id(&self) -> SourceId {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.text.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    #[must_use]
    pub const fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    pub fn span(&self, range: Range<usize>) -> Result<Span, SpanError> {
        if range.start > range.end {
            return Err(SpanError::Reversed {
                start: range.start,
                end: range.end,
            });
        }
        if range.end > self.text.len() {
            return Err(SpanError::OutOfBounds {
                end: range.end,
                len: self.text.len(),
            });
        }
        validate_offset(&self.text, range.start)?;
        validate_offset(&self.text, range.end)?;

        Ok(Span {
            source_id: self.id,
            start: range.start,
            end: range.end,
        })
    }

    pub fn slice(&self, span: Span) -> Result<&str, SpanError> {
        self.validate_span(span)?;
        Ok(&self.text[span.start..span.end])
    }

    pub fn location(&self, offset: usize) -> Result<LineColumn, SpanError> {
        self.line_index.location(&self.text, offset)
    }

    /// Converts one checked UTF-8 byte offset to a zero-based text position.
    pub fn text_position(
        &self,
        offset: usize,
        encoding: PositionEncoding,
    ) -> Result<TextPosition, PositionError> {
        self.line_index.text_position(&self.text, offset, encoding)
    }

    /// Converts one zero-based text position to a checked UTF-8 byte offset.
    pub fn byte_offset(
        &self,
        position: TextPosition,
        encoding: PositionEncoding,
    ) -> Result<usize, PositionError> {
        self.line_index.byte_offset(&self.text, position, encoding)
    }

    /// Converts a source-owned byte span to a checked zero-based text range.
    pub fn text_range(
        &self,
        span: Span,
        encoding: PositionEncoding,
    ) -> Result<TextRange, PositionError> {
        self.validate_span(span)
            .map_err(PositionError::InvalidSpan)?;
        Ok(TextRange::new(
            self.text_position(span.start, encoding)?,
            self.text_position(span.end, encoding)?,
        ))
    }

    pub(crate) fn validate_span(&self, span: Span) -> Result<(), SpanError> {
        if span.source_id != self.id {
            return Err(SpanError::WrongSource {
                expected: self.id,
                actual: span.source_id,
            });
        }
        if span.start > span.end {
            return Err(SpanError::Reversed {
                start: span.start,
                end: span.end,
            });
        }
        if span.end > self.text.len() {
            return Err(SpanError::OutOfBounds {
                end: span.end,
                len: self.text.len(),
            });
        }
        validate_offset(&self.text, span.start)?;
        validate_offset(&self.text, span.end)
    }

    pub(crate) fn line_text(&self, one_based_line: usize) -> Option<&str> {
        let line = self.line_index.line(one_based_line)?;
        Some(&self.text[line.start..line.content_end])
    }

    pub(crate) fn line_start(&self, one_based_line: usize) -> Option<usize> {
        self.line_index.line(one_based_line).map(|line| line.start)
    }

    pub(crate) fn line_content_end(&self, one_based_line: usize) -> Option<usize> {
        self.line_index
            .line(one_based_line)
            .map(|line| line.content_end)
    }
}

fn validate_offset(text: &str, offset: usize) -> Result<(), SpanError> {
    if offset > text.len() {
        return Err(SpanError::OutOfBounds {
            end: offset,
            len: text.len(),
        });
    }
    if !text.is_char_boundary(offset) {
        return Err(SpanError::NotCharBoundary { offset });
    }
    Ok(())
}

/// Explains why a byte range cannot address a source file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanError {
    Reversed {
        start: usize,
        end: usize,
    },
    OutOfBounds {
        end: usize,
        len: usize,
    },
    NotCharBoundary {
        offset: usize,
    },
    WrongSource {
        expected: SourceId,
        actual: SourceId,
    },
}

impl fmt::Display for SpanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reversed { start, end } => {
                write!(formatter, "span start {start} is after its end {end}")
            }
            Self::OutOfBounds { end, len } => {
                write!(formatter, "byte offset {end} exceeds source length {len}")
            }
            Self::NotCharBoundary { offset } => {
                write!(
                    formatter,
                    "byte offset {offset} is not a UTF-8 character boundary"
                )
            }
            Self::WrongSource { expected, actual } => write!(
                formatter,
                "span belongs to source {}, not source {}",
                actual.get(),
                expected.get()
            ),
        }
    }
}

impl std::error::Error for SpanError {}

/// Explains why a byte offset and encoded text position cannot be converted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionError {
    ByteOutOfBounds {
        offset: usize,
        len: usize,
    },
    NotUtf8Boundary {
        offset: usize,
    },
    LineOutOfBounds {
        line: usize,
        line_count: usize,
    },
    CharacterOutOfBounds {
        line: usize,
        character: usize,
        maximum: usize,
    },
    InsideUtf16Scalar {
        line: usize,
        character: usize,
    },
    InvalidSpan(SpanError),
}

impl fmt::Display for PositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteOutOfBounds { offset, len } => {
                write!(
                    formatter,
                    "byte offset {offset} exceeds source length {len}"
                )
            }
            Self::NotUtf8Boundary { offset } => {
                write!(
                    formatter,
                    "byte offset {offset} is not a UTF-8 character boundary"
                )
            }
            Self::LineOutOfBounds { line, line_count } => write!(
                formatter,
                "zero-based line {line} exceeds the source's {line_count} lines"
            ),
            Self::CharacterOutOfBounds {
                line,
                character,
                maximum,
            } => write!(
                formatter,
                "character {character} exceeds line {line}'s maximum {maximum}"
            ),
            Self::InsideUtf16Scalar { line, character } => write!(
                formatter,
                "UTF-16 character {character} on line {line} splits a scalar"
            ),
            Self::InvalidSpan(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PositionError {}
