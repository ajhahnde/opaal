//! ANSI-free semantic highlighting derived from the shared syntax pipeline.

use std::ops::Range;

use flashshell_syntax::{
    Keyword, LabelStyle, ParseOutcome, SourceFile, SourceId, Token, TokenKind, lex, parse,
};

/// Stable semantic role for one exact interactive source fragment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HighlightKind {
    Plain,
    Comment,
    Keyword,
    Literal,
    String,
    Escape,
    Expansion,
    Operator,
    Delimiter,
    Invalid,
}

/// One nonempty source fragment and its semantic highlighting role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HighlightSegment {
    kind: HighlightKind,
    text: String,
}

impl HighlightSegment {
    #[must_use]
    pub const fn kind(&self) -> HighlightKind {
        self.kind
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Pure parser-driven highlighter for one current interactive edit buffer.
#[derive(Clone, Copy, Debug, Default)]
pub struct SyntaxHighlighter;

impl SyntaxHighlighter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Classifies the complete buffer without evaluating or consulting host state.
    #[must_use]
    pub fn highlight(&self, text: &str) -> Vec<HighlightSegment> {
        let source = SourceFile::new(SourceId::new(0), "<interactive>", text);
        let tokens = lex(&source);
        let invalid_ranges = parser_invalid_ranges(&source);
        let mut segments = Vec::with_capacity(tokens.len());

        for token in tokens {
            append_token_segments(&mut segments, &source, token, &invalid_ranges);
        }
        segments
    }
}

fn parser_invalid_ranges(source: &SourceFile) -> Vec<Range<usize>> {
    let ParseOutcome::Invalid(diagnostics) = parse(source) else {
        return Vec::new();
    };
    diagnostics
        .iter()
        .flat_map(|diagnostic| diagnostic.labels())
        .filter(|label| label.style() == LabelStyle::Primary && !label.span().is_empty())
        .map(|label| label.span().start()..label.span().end())
        .collect()
}

fn append_token_segments(
    output: &mut Vec<HighlightSegment>,
    source: &SourceFile,
    token: Token,
    invalid_ranges: &[Range<usize>],
) {
    let token_span = token.span();
    let mut boundaries = vec![token_span.start(), token_span.end()];
    for range in invalid_ranges {
        if range.start < token_span.end() && range.end > token_span.start() {
            boundaries.push(range.start.max(token_span.start()));
            boundaries.push(range.end.min(token_span.end()));
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let base_kind = token_highlight_kind(token.kind());
    for endpoints in boundaries.windows(2) {
        let range = endpoints[0]..endpoints[1];
        let invalid = base_kind == HighlightKind::Invalid
            || invalid_ranges
                .iter()
                .any(|candidate| candidate.start < range.end && candidate.end > range.start);
        output.push(HighlightSegment {
            kind: if invalid {
                HighlightKind::Invalid
            } else {
                base_kind
            },
            text: source.text()[range].to_owned(),
        });
    }
}

fn token_highlight_kind(kind: TokenKind) -> HighlightKind {
    match kind {
        TokenKind::Whitespace
        | TokenKind::Newline
        | TokenKind::Identifier
        | TokenKind::WordText => HighlightKind::Plain,
        TokenKind::LineContinuation | TokenKind::BareEscape | TokenKind::DoubleEscape => {
            HighlightKind::Escape
        }
        TokenKind::Comment => HighlightKind::Comment,
        TokenKind::Keyword(Keyword::True | Keyword::False | Keyword::Null)
        | TokenKind::Number(_) => HighlightKind::Literal,
        TokenKind::Keyword(_) => HighlightKind::Keyword,
        TokenKind::SingleQuoted
        | TokenKind::DoubleQuoteStart
        | TokenKind::DoubleText
        | TokenKind::DoubleQuoteEnd => HighlightKind::String,
        TokenKind::Variable
        | TokenKind::BracedExpansionStart
        | TokenKind::CommandSubstitutionStart => HighlightKind::Expansion,
        TokenKind::Operator(_) => HighlightKind::Operator,
        TokenKind::Delimiter(_) => HighlightKind::Delimiter,
        TokenKind::Invalid(_) => HighlightKind::Invalid,
    }
}
