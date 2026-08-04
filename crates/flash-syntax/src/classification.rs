use std::fmt;

use crate::{
    Delimiter, Diagnostic, InvalidTokenKind, Keyword, Operator, Severity, SourceFile, Span,
    SpanError, Token, TokenKind,
};

/// The structural state of source input before full parsing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyntaxClassification {
    Complete,
    Incomplete(IncompleteInput),
    Invalid(Diagnostic),
}

/// Valid source that requires more input to finish its current construct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncompleteInput {
    reason: IncompleteReason,
    span: Span,
}

impl IncompleteInput {
    pub(crate) const fn new(reason: IncompleteReason, span: Span) -> Self {
        Self { reason, span }
    }

    #[must_use]
    pub const fn kind(&self) -> IncompleteReason {
        self.reason
    }

    #[must_use]
    pub const fn reason(&self) -> &'static str {
        self.reason.message()
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// Why otherwise valid input requires another token or closing delimiter.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IncompleteReason {
    UnmatchedSingleQuote,
    UnmatchedDoubleQuote,
    TrailingBackslash,
    UnmatchedBracedInterpolation,
    UnmatchedCommandSubstitution,
    PipelineStage,
    RedirectionTarget,
    FunctionBlock,
    ClosureBody,
    Call,
    Parenthesis,
    Bracket,
    Brace,
    Expression,
    TypeReference,
    ConditionalOperand,
}

impl IncompleteReason {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::UnmatchedSingleQuote => "unmatched single quote",
            Self::UnmatchedDoubleQuote => "unmatched double quote",
            Self::TrailingBackslash => "explicit continuation requires more source",
            Self::UnmatchedBracedInterpolation => "unmatched braced interpolation",
            Self::UnmatchedCommandSubstitution => "unmatched command substitution",
            Self::PipelineStage => "pipeline operator requires another stage",
            Self::RedirectionTarget => "redirection operator requires one target word",
            Self::FunctionBlock => "function block requires a closing brace",
            Self::ClosureBody => "closure requires a body and closing brace",
            Self::Call => "call requires a closing parenthesis",
            Self::Parenthesis => "opening parenthesis requires a closing parenthesis",
            Self::Bracket => "opening bracket requires a closing bracket",
            Self::Brace => "opening brace requires a closing brace",
            Self::Expression => "expression requires another operand",
            Self::TypeReference => "type annotation requires a type",
            Self::ConditionalOperand => "conditional operator requires another pipeline",
        }
    }
}

impl fmt::Display for IncompleteReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameKind {
    DoubleQuote,
    BracedExpansion,
    CommandSubstitution,
    Parenthesis { call: bool },
    Bracket,
    Brace { closure: bool, function: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Frame {
    kind: FrameKind,
    span: Span,
}

/// Classifies an existing lossless token stream without reparsing source text.
pub fn classify_tokens(
    source: &SourceFile,
    tokens: &[Token],
) -> Result<SyntaxClassification, SpanError> {
    for token in tokens {
        token.text(source)?;
        if let TokenKind::Invalid(kind) = token.kind() {
            return Ok(SyntaxClassification::Invalid(invalid_token_diagnostic(
                token, kind,
            )));
        }
    }

    let mut frames = Vec::new();
    let mut previous_significant = None;
    for (index, token) in tokens.iter().enumerate() {
        let kind = token.kind();
        match kind {
            TokenKind::SingleQuoted => {
                let text = token.text(source)?;
                if text.len() < 2 || !text.ends_with('\'') {
                    return Ok(incomplete(
                        IncompleteReason::UnmatchedSingleQuote,
                        token.span(),
                    ));
                }
            }
            TokenKind::DoubleQuoteStart => frames.push(Frame {
                kind: FrameKind::DoubleQuote,
                span: token.span(),
            }),
            TokenKind::DoubleQuoteEnd => {
                if !pop_matching(&mut frames, FrameKind::DoubleQuote) {
                    return Ok(unexpected_closer(token.span()));
                }
            }
            TokenKind::BracedExpansionStart => frames.push(Frame {
                kind: FrameKind::BracedExpansion,
                span: token.span(),
            }),
            TokenKind::CommandSubstitutionStart => frames.push(Frame {
                kind: FrameKind::CommandSubstitution,
                span: token.span(),
            }),
            TokenKind::Delimiter(delimiter) => {
                if let Some(classification) =
                    classify_delimiter(tokens, index, delimiter, previous_significant, &mut frames)
                {
                    return Ok(classification);
                }
            }
            _ => {}
        }
        if is_significant(kind) {
            previous_significant = Some(index);
        }
    }

    if let Some(frame) = frames.last().copied() {
        return Ok(incomplete(frame_reason(frame.kind), frame.span));
    }

    if let Some(token) = previous_significant.map(|index| &tokens[index])
        && let Some(reason) = trailing_requirement(source, token)?
    {
        return Ok(incomplete(reason, token.span()));
    }

    Ok(SyntaxClassification::Complete)
}

fn classify_delimiter(
    tokens: &[Token],
    index: usize,
    delimiter: Delimiter,
    previous_significant: Option<usize>,
    frames: &mut Vec<Frame>,
) -> Option<SyntaxClassification> {
    let token = &tokens[index];
    match delimiter {
        Delimiter::LeftParenthesis => {
            let call = previous_significant
                .map(|previous| tokens[previous].kind())
                .is_some_and(can_end_callee);
            frames.push(Frame {
                kind: FrameKind::Parenthesis { call },
                span: token.span(),
            });
        }
        Delimiter::LeftBracket => frames.push(Frame {
            kind: FrameKind::Bracket,
            span: token.span(),
        }),
        Delimiter::LeftBrace => {
            let closure = tokens.get(index + 1).is_some_and(|next| {
                token.is_adjacent_to(next)
                    && matches!(
                        next.kind(),
                        TokenKind::Operator(Operator::Pipe | Operator::Or)
                    )
            });
            let function = statement_contains_keyword(tokens, index, Keyword::Def);
            frames.push(Frame {
                kind: FrameKind::Brace { closure, function },
                span: token.span(),
            });
        }
        Delimiter::RightParenthesis => {
            if !pop_where(frames, |kind| {
                matches!(
                    kind,
                    FrameKind::Parenthesis { .. } | FrameKind::CommandSubstitution
                )
            }) {
                return Some(unexpected_closer(token.span()));
            }
        }
        Delimiter::RightBracket => {
            if !pop_matching(frames, FrameKind::Bracket) {
                return Some(unexpected_closer(token.span()));
            }
        }
        Delimiter::RightBrace => {
            if !pop_where(frames, |kind| {
                matches!(kind, FrameKind::Brace { .. } | FrameKind::BracedExpansion)
            }) {
                return Some(unexpected_closer(token.span()));
            }
        }
    }
    None
}

fn pop_matching(frames: &mut Vec<Frame>, expected: FrameKind) -> bool {
    pop_where(frames, |kind| kind == expected)
}

fn pop_where(frames: &mut Vec<Frame>, matches: impl FnOnce(FrameKind) -> bool) -> bool {
    if frames.last().is_some_and(|frame| matches(frame.kind)) {
        frames.pop();
        true
    } else {
        false
    }
}

fn statement_contains_keyword(tokens: &[Token], before: usize, keyword: Keyword) -> bool {
    tokens[..before]
        .iter()
        .rev()
        .take_while(|token| {
            !matches!(
                token.kind(),
                TokenKind::Newline | TokenKind::Operator(Operator::Semicolon)
            )
        })
        .any(|token| token.kind() == TokenKind::Keyword(keyword))
}

fn can_end_callee(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Variable
            | TokenKind::Delimiter(Delimiter::RightParenthesis | Delimiter::RightBracket)
    )
}

fn is_significant(kind: TokenKind) -> bool {
    !matches!(
        kind,
        TokenKind::Whitespace | TokenKind::Newline | TokenKind::Comment
    )
}

fn frame_reason(kind: FrameKind) -> IncompleteReason {
    match kind {
        FrameKind::DoubleQuote => IncompleteReason::UnmatchedDoubleQuote,
        FrameKind::BracedExpansion => IncompleteReason::UnmatchedBracedInterpolation,
        FrameKind::CommandSubstitution => IncompleteReason::UnmatchedCommandSubstitution,
        FrameKind::Parenthesis { call: true } => IncompleteReason::Call,
        FrameKind::Parenthesis { call: false } => IncompleteReason::Parenthesis,
        FrameKind::Bracket => IncompleteReason::Bracket,
        FrameKind::Brace { closure: true, .. } => IncompleteReason::ClosureBody,
        FrameKind::Brace { function: true, .. } => IncompleteReason::FunctionBlock,
        FrameKind::Brace { .. } => IncompleteReason::Brace,
    }
}

fn trailing_requirement(
    source: &SourceFile,
    token: &Token,
) -> Result<Option<IncompleteReason>, SpanError> {
    let reason = match token.kind() {
        TokenKind::BareEscape if token.text(source)? == "\\" => IncompleteReason::TrailingBackslash,
        TokenKind::LineContinuation if token.span().end() == source.len() => {
            IncompleteReason::TrailingBackslash
        }
        TokenKind::Operator(Operator::Pipe | Operator::PipeBoth) => IncompleteReason::PipelineStage,
        TokenKind::Operator(
            Operator::Less | Operator::Greater | Operator::Append | Operator::Duplicate,
        ) => IncompleteReason::RedirectionTarget,
        _ => return Ok(None),
    };
    Ok(Some(reason))
}

fn invalid_token_diagnostic(token: &Token, kind: InvalidTokenKind) -> Diagnostic {
    let message = match kind {
        InvalidTokenKind::Nul => "NUL byte is not valid source",
        InvalidTokenKind::LoneCarriageReturn => "lone carriage return is not a valid line ending",
        InvalidTokenKind::UnknownDoubleEscape => "unknown double-quoted escape",
        InvalidTokenKind::EmptyUnicodeEscape => "Unicode escape requires hexadecimal digits",
        InvalidTokenKind::UnicodeSurrogate => "Unicode escape cannot encode a surrogate",
        InvalidTokenKind::UnicodeOutOfRange => "Unicode escape exceeds the scalar range",
        InvalidTokenKind::MalformedUnicodeEscape => "malformed Unicode escape",
    };
    Diagnostic::new(Severity::Error, "FS0001", message).with_primary(token.span(), message)
}

fn unexpected_closer(span: Span) -> SyntaxClassification {
    let message = "unexpected closing delimiter";
    SyntaxClassification::Invalid(
        Diagnostic::new(Severity::Error, "FS0002", message).with_primary(span, message),
    )
}

fn incomplete(reason: IncompleteReason, span: Span) -> SyntaxClassification {
    SyntaxClassification::Incomplete(IncompleteInput { reason, span })
}
