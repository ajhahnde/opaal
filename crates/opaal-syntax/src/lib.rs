#![forbid(unsafe_code)]

//! Source, syntax, and diagnostic types for OPAAL.

mod ast;
mod classification;
mod completion;
mod diagnostic;
mod formatter;
mod language;
mod lexer;
mod parser;
mod source;

/// Explicitly isolated Flash 1 syntax retained for the read-only migration tool.
#[cfg(feature = "flash-v1-migration")]
pub mod migration {
    pub use crate::classification::classify_flash_v1_tokens as classify_tokens;
    pub use crate::formatter::format_flash_v1_source as format_source;
    pub use crate::lexer::lex_flash_v1 as lex;
    pub use crate::parser::{
        parse_flash_v1 as parse, parse_flash_v1_with_control as parse_with_control,
    };
}

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
pub use language::{
    LanguageDetection, LanguageDirective, LanguageIdentity, VersionedScript, detect_source_language,
};
pub use lexer::{
    Delimiter, InvalidTokenKind, Keyword, NumberKind, Operator, Token, TokenKind, lex_opaal,
};
pub use parser::{
    ControlledParseOutcome, ControlledVersionedParseOutcome, ParseOutcome, VersionedParseOutcome,
    parse_opaal, parse_opaal_submission, parse_opaal_submission_with_control,
    parse_opaal_with_control,
};
pub use source::{
    LineColumn, LineIndex, PositionEncoding, PositionError, SourceFile, SourceId, Span, SpanError,
    TextPosition, TextRange,
};

/// Stable package identifier for the syntax crate.
pub const CRATE_NAME: &str = "opaal-syntax";
