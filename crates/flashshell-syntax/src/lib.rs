#![forbid(unsafe_code)]

//! Source, syntax, and diagnostic types for FlashShell.

mod ast;
mod classification;
mod diagnostic;
mod formatter;
mod lexer;
mod parser;
mod source;

pub use ast::*;
pub use classification::{
    IncompleteInput, IncompleteReason, SyntaxClassification, classify_tokens,
};
pub use diagnostic::{Diagnostic, Label, LabelStyle, RenderError, Severity, render_diagnostic};
pub use formatter::{FormatOutcome, format_source};
pub use lexer::{
    Delimiter, InvalidTokenKind, Keyword, NumberKind, Operator, Token, TokenKind, lex,
};
pub use parser::{ParseOutcome, parse};
pub use source::{LineColumn, LineIndex, SourceFile, SourceId, Span, SpanError};

/// Stable package identifier for the syntax crate.
pub const CRATE_NAME: &str = "flashshell-syntax";
