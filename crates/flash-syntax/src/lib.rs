#![forbid(unsafe_code)]

//! Source, syntax, and diagnostic types for Flash.

mod ast;
mod classification;
mod completion;
mod diagnostic;
mod formatter;
mod lexer;
mod parser;
mod source;

pub use ast::*;
pub use classification::{
    IncompleteInput, IncompleteReason, SyntaxClassification, classify_tokens,
};
pub use completion::{CompletionContext, CompletionTarget, PathCompletionStyle, completion_target};
pub use diagnostic::{
    Diagnostic, Label, LabelStyle, RenderError, Severity, render_diagnostic,
    render_diagnostic_sources,
};
pub use formatter::{FormatOutcome, format_source};
pub use lexer::{
    Delimiter, InvalidTokenKind, Keyword, NumberKind, Operator, Token, TokenKind, lex,
};
pub use parser::{ControlledParseOutcome, ParseOutcome, parse, parse_with_control};
pub use source::{
    LineColumn, LineIndex, PositionEncoding, PositionError, SourceFile, SourceId, Span, SpanError,
    TextPosition, TextRange,
};

/// Stable package identifier for the syntax crate.
pub const CRATE_NAME: &str = "flash-syntax";
