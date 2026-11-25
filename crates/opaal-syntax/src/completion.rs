//! Lossless token/cursor classification shared by editor adapters.

use std::ops::Range;

use crate::{
    Delimiter, Diagnostic, IncompleteInput, IncompleteReason, Keyword, Operator, Severity,
    SourceFile, SourceId, Span, SyntaxClassification, Token, TokenKind, lex_opaal,
};

/// The syntax-owned role of one completion replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompletionContext {
    Command {
        forced_external: bool,
    },
    Expression,
    Name,
    Flag {
        command: String,
    },
    Path {
        style: PathCompletionStyle,
        glob_pattern: bool,
        interpolated: bool,
    },
    None,
}

/// Source spelling retained while inserting one path candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathCompletionStyle {
    Bare,
    SingleQuoted,
    DoubleQuoted,
    DoubleQuotedFragment,
}

/// One checked token/cursor target independent of candidate sources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionTarget {
    context: CompletionContext,
    replacement: Range<usize>,
    prefix: String,
}

impl CompletionTarget {
    #[must_use]
    pub const fn context(&self) -> &CompletionContext {
        &self.context
    }

    #[must_use]
    pub fn replacement(&self) -> Range<usize> {
        self.replacement.clone()
    }

    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

/// Classifies a UTF-8 byte cursor through the lossless token stream.
///
/// This deliberately does not require a complete AST, allowing incomplete
/// editor buffers to retain syntax-contextual completion.
#[must_use]
pub fn completion_target(source: &str, cursor: usize) -> Option<CompletionTarget> {
    if cursor > source.len() || !source.is_char_boundary(cursor) {
        return None;
    }
    let source_file = SourceFile::new(SourceId::new(0), "<completion>", source);
    let tokens = lex_opaal(&source_file);
    let prefix = &tokens[..tokens.partition_point(|token| token.span().start() < cursor)];
    let import_group = prefix
        .iter()
        .rposition(|token| token.kind() == TokenKind::Keyword(crate::Keyword::Import))
        .is_some_and(|start| {
            let mut after_import = prefix[start + 1..].iter().filter(|token| {
                !matches!(
                    token.kind(),
                    TokenKind::Whitespace
                        | TokenKind::Comment
                        | TokenKind::DocumentationComment
                        | TokenKind::LineContinuation
                )
            });
            after_import
                .next()
                .is_some_and(|token| token.kind() == TokenKind::Delimiter(Delimiter::LeftBrace))
                && !after_import
                    .any(|token| token.kind() == TokenKind::Delimiter(Delimiter::RightBrace))
        });
    let (in_bracket_operand, separator) =
        crate::parser::bracket_operand_context(&source_file, &tokens, cursor);
    let active = ActiveWord::at(source, &tokens, cursor, import_group, separator)?;
    let prior = significant_before(&tokens, active.range.start);
    let stage = current_stage(&prior);
    let context = if import_group && is_path_spelling(&active) {
        path_context(source, &tokens, &active)
    } else if scan_conditionals(&prior, false).unwrap_or(false) || in_bracket_operand {
        CompletionContext::Expression
    } else if prior.last().is_some_and(|token| {
        matches!(
            token.kind(),
            TokenKind::InterpolationStart | TokenKind::Delimiter(Delimiter::LeftBrace)
        )
    }) {
        CompletionContext::Name
    } else if prior.last().is_some_and(|token| {
        matches!(
            token.kind(),
            TokenKind::Operator(
                Operator::Assign
                    | Operator::AddAssign
                    | Operator::SubtractAssign
                    | Operator::MultiplyAssign
                    | Operator::DivideAssign
                    | Operator::RemainderAssign
            )
        )
    }) || tokens.iter().any(|token| {
        token.span().start() == active.range.end
            && token.kind() == TokenKind::Delimiter(Delimiter::LeftParenthesis)
    }) {
        CompletionContext::Expression
    } else {
        classify_context(source, &tokens, stage, &active)
    };
    Some(CompletionTarget {
        context,
        replacement: active.range,
        prefix: active.text.to_owned(),
    })
}

#[derive(Default)]
struct ConditionalCompletionScope {
    value_arm: bool,
    expression: bool,
    value_stage: bool,
    headers: Vec<ConditionalArm>,
    arm: Option<ConditionalArm>,
    closed_arm: Option<(ConditionalArm, Span)>,
    arm_started: bool,
}

#[derive(Clone, Copy)]
struct ConditionalArm {
    value: bool,
    allows_else: bool,
}

/// Recovers value-if diagnostics without descending through unclosed delimiters.
pub(crate) fn recover_value_if(tokens: &[Token]) -> Option<SyntaxClassification> {
    if !tokens
        .iter()
        .any(|token| token.kind() == TokenKind::Keyword(Keyword::If))
    {
        return None;
    }
    let tokens: Vec<_> = tokens
        .iter()
        .filter(|token| !completion_trivia(token.kind()))
        .collect();
    scan_conditionals(&tokens, true).err()
}

fn scan_conditionals(tokens: &[&Token], recover: bool) -> Result<bool, SyntaxClassification> {
    if !tokens
        .iter()
        .any(|token| token.kind() == TokenKind::Keyword(Keyword::If))
    {
        return Ok(false);
    }
    let mut scopes = vec![ConditionalCompletionScope::default()];
    let mut previous = None;
    let mut value_depth = 0;
    for (index, token) in tokens.iter().enumerate() {
        let scope = scopes.last_mut().unwrap();
        let kind = token.kind();
        if let Some((arm, _)) = scope.closed_arm
            && arm.value
            && kind != TokenKind::Newline
        {
            let is_else = kind == TokenKind::Keyword(Keyword::Else);
            if recover && arm.allows_else != is_else {
                let message = if is_else {
                    "if expression has more than one final else arm"
                } else {
                    IncompleteReason::IfExpressionElse.message()
                };
                return Err(conditional_error(token.span(), message));
            }
            if !arm.allows_else {
                scope.closed_arm = None;
            }
        }
        if scope.arm.is_some_and(|arm| arm.value)
            && !scope.arm_started
            && kind != TokenKind::Newline
        {
            if recover
                && (kind == TokenKind::Delimiter(Delimiter::RightBrace)
                    || matches!(kind, TokenKind::Keyword(keyword)
                        if !matches!(keyword, Keyword::If | Keyword::Null | Keyword::True | Keyword::False)))
            {
                return Err(conditional_error(token.span(), "expected an expression"));
            }
            scope.arm_started = true;
        }
        match kind {
            TokenKind::Keyword(Keyword::If) => {
                if previous == Some(TokenKind::Keyword(Keyword::Else)) {
                    if let Some(arm) = scope.headers.last_mut() {
                        arm.allows_else = true;
                    }
                } else {
                    let stage_start = previous.is_none_or(is_command_boundary)
                        || previous == Some(TokenKind::Delimiter(Delimiter::LeftParenthesis));
                    let value = scope.expression
                        || (scope.value_stage && stage_start)
                        || previous == Some(TokenKind::Keyword(Keyword::If))
                        || previous.is_some_and(accepts_value_after);
                    if value || stage_start {
                        if value {
                            value_depth += 1;
                            if recover && value_depth > crate::parser::MAX_IF_EXPRESSION_DEPTH {
                                return Err(conditional_error(
                                    token.span(),
                                    crate::parser::IF_EXPRESSION_DEPTH_ERROR,
                                ));
                            }
                        }
                        scope.headers.push(ConditionalArm {
                            value,
                            allows_else: true,
                        });
                    }
                }
            }
            TokenKind::Keyword(Keyword::Else) => {
                if let Some((arm, _)) = scope.closed_arm.take() {
                    scope.headers.push(ConditionalArm {
                        allows_else: false,
                        ..arm
                    });
                }
            }
            TokenKind::Delimiter(Delimiter::LeftBrace) => {
                let mut next = tokens[index + 1..]
                    .iter()
                    .copied()
                    .filter(|token| token.kind() != TokenKind::Newline);
                let first = next.next();
                let closure = first.is_some_and(|next| {
                    token.is_adjacent_to(next)
                        && matches!(
                            next.kind(),
                            TokenKind::Operator(Operator::Pipe | Operator::Or)
                        )
                });
                let empty_nominal = previous == Some(TokenKind::Identifier)
                    && first.is_some_and(|token| {
                        token.kind() == TokenKind::Delimiter(Delimiter::RightBrace)
                    })
                    && next
                        .clone()
                        .next()
                        .is_some_and(|token| token.kind() != TokenKind::Keyword(Keyword::Else));
                let nominal = empty_nominal
                    || (first.is_some_and(|token| token.kind() == TokenKind::Identifier)
                        && next.next().is_some_and(|colon| {
                            colon.kind() == TokenKind::Operator(Operator::Colon)
                                && !next.next().is_some_and(|tail| {
                                    tail.kind() == TokenKind::Operator(Operator::Colon)
                                        && colon.is_adjacent_to(tail)
                                })
                        }));
                let arm = if previous == Some(TokenKind::Keyword(Keyword::Else))
                    || (!closure && !nominal && previous != Some(TokenKind::Keyword(Keyword::If)))
                {
                    scope.headers.pop()
                } else {
                    None
                };
                let value_arm = arm.map_or(scope.value_arm && !closure, |arm| arm.value);
                scopes.push(ConditionalCompletionScope {
                    value_arm,
                    expression: value_arm,
                    value_stage: closure,
                    arm,
                    ..Default::default()
                });
            }
            TokenKind::Delimiter(Delimiter::LeftParenthesis | Delimiter::LeftBracket) => {
                let expression = kind == TokenKind::Delimiter(Delimiter::LeftBracket)
                    || matches!(
                        previous,
                        Some(
                            TokenKind::Identifier
                                | TokenKind::Delimiter(
                                    Delimiter::RightParenthesis | Delimiter::RightBracket
                                )
                        )
                    );
                let value_arm = scope.value_arm && expression;
                scopes.push(ConditionalCompletionScope {
                    value_arm,
                    expression,
                    value_stage: !expression,
                    ..Default::default()
                });
            }
            TokenKind::InterpolationStart => {
                let value_arm = scope.value_arm;
                scopes.push(ConditionalCompletionScope {
                    value_arm,
                    expression: true,
                    ..Default::default()
                });
            }
            TokenKind::Delimiter(
                Delimiter::RightBrace | Delimiter::RightParenthesis | Delimiter::RightBracket,
            ) => {
                if scopes.len() > 1 {
                    let closed = scopes.pop().unwrap();
                    if closed.arm.is_some_and(|arm| arm.value && !arm.allows_else) {
                        value_depth -= 1;
                    }
                    scopes.last_mut().unwrap().closed_arm = closed
                        .arm
                        .filter(|arm| arm.allows_else || arm.value)
                        .map(|arm| (arm, token.span()));
                }
            }
            TokenKind::Operator(Operator::Semicolon) => {
                if recover && scope.arm.is_some_and(|arm| arm.value) {
                    return Err(conditional_error(
                        token.span(),
                        "if expression arm requires exactly one expression",
                    ));
                }
                scope.headers.clear();
                scope.closed_arm = None;
            }
            _ => {}
        }
        if kind != TokenKind::Newline || !previous.is_some_and(accepts_continuation) {
            previous = Some(kind);
        }
    }
    let scope = scopes.last().unwrap();
    if recover
        && let Some((arm, span)) = scope.closed_arm
        && arm.value
        && arm.allows_else
    {
        return Err(SyntaxClassification::Incomplete(IncompleteInput::new(
            IncompleteReason::IfExpressionElse,
            span,
        )));
    }
    Ok(scope.value_arm)
}

fn conditional_error(span: Span, message: &str) -> SyntaxClassification {
    SyntaxClassification::Invalid(
        Diagnostic::new(Severity::Error, "OP1000", message).with_primary(span, message),
    )
}

fn accepts_continuation(kind: TokenKind) -> bool {
    accepts_value_after(kind) && !matches!(kind, TokenKind::Keyword(Keyword::Return))
}

fn accepts_value_after(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Keyword(
            Keyword::Return | Keyword::Throw | Keyword::In | Keyword::While | Keyword::Match
        ) | TokenKind::Operator(
            Operator::Assign
                | Operator::AddAssign
                | Operator::SubtractAssign
                | Operator::MultiplyAssign
                | Operator::DivideAssign
                | Operator::RemainderAssign
                | Operator::Colon
                | Operator::Comma
                | Operator::Bang
                | Operator::Plus
                | Operator::Minus
                | Operator::Star
                | Operator::Slash
                | Operator::Percent
                | Operator::Equal
                | Operator::NotEqual
                | Operator::Less
                | Operator::LessEqual
                | Operator::Greater
                | Operator::GreaterEqual
                | Operator::Range
                | Operator::RangeInclusive
                | Operator::Pipe
                | Operator::PipeBoth
                | Operator::And
                | Operator::Or
        )
    )
}

struct ActiveWord {
    range: Range<usize>,
    text: String,
    style: PathCompletionStyle,
    interpolated: bool,
}

impl ActiveWord {
    fn at(
        source: &str,
        tokens: &[Token],
        cursor: usize,
        import_group: bool,
        separator: Option<Span>,
    ) -> Option<Self> {
        let is_component = |token: &Token| {
            is_word_component(token.kind())
                && !(import_group && token.kind() == TokenKind::Operator(Operator::Comma))
                && separator != Some(token.span())
        };
        let containing = tokens.iter().position(|token| {
            token.span().start() <= cursor && cursor <= token.span().end() && is_component(token)
        });

        let Some(index) = containing else {
            let occupied = tokens.iter().any(|token| {
                token.span().start() < cursor
                    && cursor < token.span().end()
                    && !matches!(token.kind(), TokenKind::Whitespace | TokenKind::Newline)
            });
            return (!occupied).then_some(Self {
                range: cursor..cursor,
                text: String::new(),
                style: PathCompletionStyle::Bare,
                interpolated: false,
            });
        };

        let mut first = index;
        while first > 0
            && tokens[first - 1].span().end() == tokens[first].span().start()
            && is_component(&tokens[first - 1])
        {
            first -= 1;
        }
        let mut last = index;
        while last + 1 < tokens.len()
            && tokens[last].span().end() == tokens[last + 1].span().start()
            && is_component(&tokens[last + 1])
        {
            last += 1;
        }
        let mut interpolated = false;
        let start = tokens[first].span().start();
        let end = tokens[last].span().end();
        if first > 0
            && tokens[first - 1].span().end() == start
            && matches!(
                tokens[first - 1].kind(),
                TokenKind::Delimiter(Delimiter::RightBrace)
            )
        {
            interpolated = true;
        }
        let mut style = if tokens[first].kind() == TokenKind::SingleQuoted {
            PathCompletionStyle::SingleQuoted
        } else if (first..=last).any(|token| {
            matches!(
                tokens[token].kind(),
                TokenKind::DoubleQuoteStart
                    | TokenKind::DoubleText
                    | TokenKind::DoubleEscape
                    | TokenKind::DoubleQuoteEnd
            )
        }) {
            PathCompletionStyle::DoubleQuoted
        } else {
            PathCompletionStyle::Bare
        };
        let raw_prefix = &source[start..cursor];
        let closed_double =
            tokens[last].kind() == TokenKind::DoubleQuoteEnd && cursor >= tokens[last].span().end();
        if interpolated
            && style == PathCompletionStyle::DoubleQuoted
            && !raw_prefix.starts_with('"')
        {
            style = PathCompletionStyle::DoubleQuotedFragment;
        }
        let text = decode_path_prefix(raw_prefix, style, closed_double)?;
        Some(Self {
            text,
            range: start..end,
            style,
            interpolated,
        })
    }
}

fn classify_context(
    source: &str,
    tokens: &[Token],
    stage: &[&Token],
    active: &ActiveWord,
) -> CompletionContext {
    if stage
        .last()
        .is_some_and(|token| is_file_redirect(token.kind()))
    {
        return path_context(source, tokens, active);
    }

    let forced_external = stage.first().is_some_and(|token| {
        token.kind() == TokenKind::Operator(Operator::Caret)
            && token.span().end() == active.range.start
    });
    let head_tokens = if forced_external { &stage[1..] } else { stage };
    let Some(head_end) = first_word_end(head_tokens) else {
        return if is_path_spelling(active) {
            path_context(source, tokens, active)
        } else {
            CompletionContext::Command { forced_external }
        };
    };
    if active.range.start < head_end {
        return if is_path_spelling(active) {
            path_context(source, tokens, active)
        } else {
            CompletionContext::Command { forced_external }
        };
    }

    let head_start = head_tokens[0].span().start();
    let command = &source[head_start..head_end];
    if active.text.starts_with('-') {
        return CompletionContext::Flag {
            command: command.to_owned(),
        };
    }
    if is_glob_argument(source, tokens, active.range.start) || is_path_spelling(active) {
        path_context(source, tokens, active)
    } else {
        CompletionContext::None
    }
}

fn is_path_spelling(active: &ActiveWord) -> bool {
    active.style != PathCompletionStyle::Bare
        || active.text.contains('/')
        || active.text.starts_with('.')
        || active.text.starts_with('~')
        || contains_glob_syntax(&active.text)
}

fn path_context(source: &str, tokens: &[Token], active: &ActiveWord) -> CompletionContext {
    CompletionContext::Path {
        style: active.style,
        glob_pattern: is_glob_argument(source, tokens, active.range.start),
        interpolated: active.interpolated,
    }
}

fn is_glob_argument(source: &str, tokens: &[Token], offset: usize) -> bool {
    let prior: Vec<_> = tokens
        .iter()
        .filter(|token| {
            token.span().end() <= offset
                && !matches!(
                    token.kind(),
                    TokenKind::Whitespace | TokenKind::LineContinuation
                )
        })
        .collect();
    let Some(open) = prior
        .iter()
        .rposition(|token| token.kind() == TokenKind::Delimiter(Delimiter::LeftParenthesis))
    else {
        return false;
    };
    let Some(callee) = open.checked_sub(1).and_then(|index| prior.get(index)) else {
        return false;
    };
    callee.kind() == TokenKind::Identifier
        && source.get(callee.span().start()..callee.span().end()) == Some("glob")
}

fn contains_glob_syntax(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(character, '*' | '?' | '['))
}

fn decode_path_prefix(
    raw: &str,
    style: PathCompletionStyle,
    closed_double: bool,
) -> Option<String> {
    match style {
        PathCompletionStyle::Bare => decode_bare(raw),
        PathCompletionStyle::SingleQuoted => Some(
            raw.strip_prefix('\'')
                .unwrap_or(raw)
                .strip_suffix('\'')
                .unwrap_or_else(|| raw.strip_prefix('\'').unwrap_or(raw))
                .to_owned(),
        ),
        PathCompletionStyle::DoubleQuoted | PathCompletionStyle::DoubleQuotedFragment => {
            decode_double(raw, closed_double)
        }
    }
}

fn decode_bare(raw: &str) -> Option<String> {
    let mut decoded = String::new();
    let mut characters = raw.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            decoded.push(characters.next()?);
        } else {
            decoded.push(character);
        }
    }
    Some(decoded)
}

fn decode_double(raw: &str, closed: bool) -> Option<String> {
    let content = raw.strip_prefix('"').unwrap_or(raw);
    let content = if closed {
        content.strip_suffix('"').unwrap_or(content)
    } else {
        content
    };
    let mut decoded = String::new();
    let mut characters = content.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next()? {
            '\\' => decoded.push('\\'),
            '"' => decoded.push('"'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            '0' => return None,
            'u' => {
                if characters.next()? != '{' {
                    return None;
                }
                let mut digits = String::new();
                loop {
                    let next = characters.next()?;
                    if next == '}' {
                        break;
                    }
                    digits.push(next);
                }
                decoded.push(char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(decoded)
}

fn significant_before(tokens: &[Token], offset: usize) -> Vec<&Token> {
    tokens
        .iter()
        .filter(|token| token.span().end() <= offset && !completion_trivia(token.kind()))
        .collect()
}

fn completion_trivia(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Whitespace
            | TokenKind::Comment
            | TokenKind::DocumentationComment
            | TokenKind::LineContinuation
    )
}

fn current_stage<'tokens>(tokens: &'tokens [&Token]) -> &'tokens [&'tokens Token] {
    let start = tokens
        .iter()
        .rposition(|token| is_command_boundary(token.kind()))
        .map_or(0, |position| position + 1);
    &tokens[start..]
}

fn first_word_end(tokens: &[&Token]) -> Option<usize> {
    let first = tokens.first()?;
    if !is_word_component(first.kind()) {
        return None;
    }
    let mut end = first.span().end();
    for token in &tokens[1..] {
        if token.span().start() != end || !is_word_component(token.kind()) {
            break;
        }
        end = token.span().end();
    }
    Some(end)
}

fn is_command_boundary(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Newline
            | TokenKind::InterpolationStart
            | TokenKind::Operator(
                Operator::Semicolon
                    | Operator::Pipe
                    | Operator::PipeBoth
                    | Operator::And
                    | Operator::Or
                    | Operator::Background
            )
            | TokenKind::Delimiter(Delimiter::LeftBrace | Delimiter::RightBrace)
    )
}

fn is_file_redirect(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Operator(Operator::Less | Operator::Greater | Operator::Append)
    )
}

fn is_word_component(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Keyword(_)
            | TokenKind::Number(_)
            | TokenKind::WordText
            | TokenKind::BareEscape
            | TokenKind::SingleQuoted
            | TokenKind::DoubleQuoteStart
            | TokenKind::DoubleText
            | TokenKind::DoubleEscape
            | TokenKind::EscapedBrace
            | TokenKind::DoubleQuoteEnd
            | TokenKind::Operator(
                Operator::Assign
                    | Operator::AddAssign
                    | Operator::SubtractAssign
                    | Operator::MultiplyAssign
                    | Operator::DivideAssign
                    | Operator::RemainderAssign
                    | Operator::Equal
                    | Operator::NotEqual
                    | Operator::Plus
                    | Operator::Minus
                    | Operator::Star
                    | Operator::Slash
                    | Operator::Percent
                    | Operator::Bang
                    | Operator::Range
                    | Operator::RangeInclusive
                    | Operator::Arrow
                    | Operator::MatchArrow
                    | Operator::Dot
                    | Operator::Comma
                    | Operator::Colon
            )
    )
}
