#![forbid(unsafe_code)]

//! Source, syntax, and diagnostic types for OPAAL.

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
    IncompleteInput, IncompleteReason, SyntaxClassification, classify_opaal_tokens,
};
pub use completion::{CompletionContext, CompletionTarget, PathCompletionStyle, completion_target};
pub use diagnostic::{
    Diagnostic, Label, LabelStyle, RenderError, Severity, render_diagnostic,
    render_diagnostic_sources,
};
pub use formatter::{FormatOutcome, format_source_opaal};
pub use lexer::{
    Delimiter, InvalidTokenKind, Keyword, NumberKind, Operator, Token, TokenKind, lex_opaal,
};
pub use parser::{ControlledParseOutcome, ParseOutcome, parse_opaal, parse_opaal_with_control};
pub use source::{
    LineColumn, LineIndex, PositionEncoding, PositionError, SourceFile, SourceId, Span, SpanError,
    TextPosition, TextRange,
};

/// Stable package identifier for the syntax crate.
pub const CRATE_NAME: &str = "opaal-syntax";
