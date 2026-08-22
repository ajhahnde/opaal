//! Lossless token/cursor classification shared by editor adapters.

use std::ops::Range;

use crate::{Delimiter, Operator, SourceFile, SourceId, Token, TokenKind, lex};

/// The syntax-owned role of one completion replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompletionContext {
    Command {
        forced_external: bool,
    },
    CommandSubstitutionModifier,
    Expression,
    Variable {
        braced: bool,
    },
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
    let tokens = lex(&source_file);
    let active = ActiveWord::at(source, &tokens, cursor)?;
    let prior = significant_before(&tokens, active.range.start);
    let stage = strip_command_substitution_modifier(source, current_stage(&prior));
    let context = if prior
        .last()
        .is_some_and(|token| token.kind() == TokenKind::CommandSubstitutionStart)
    {
        CompletionContext::CommandSubstitutionModifier
    } else if prior
        .last()
        .is_some_and(|token| token.kind() == TokenKind::BracedExpansionStart)
    {
        CompletionContext::Variable { braced: true }
    } else if tokens.iter().any(|token| {
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

fn strip_command_substitution_modifier<'tokens>(
    source: &str,
    stage: &'tokens [&'tokens Token],
) -> &'tokens [&'tokens Token] {
    let [identifier, colon, rest @ ..] = stage else {
        return stage;
    };
    let contextual = identifier.kind() == TokenKind::Identifier
        && colon.kind() == TokenKind::Operator(Operator::Colon)
        && identifier.is_adjacent_to(colon)
        && matches!(
            source.get(identifier.span().start()..identifier.span().end()),
            Some("text" | "bytes")
        );
    if contextual { rest } else { stage }
}

struct ActiveWord {
    range: Range<usize>,
    text: String,
    style: PathCompletionStyle,
    interpolated: bool,
}

impl ActiveWord {
    fn at(source: &str, tokens: &[Token], cursor: usize) -> Option<Self> {
        let containing = tokens.iter().position(|token| {
            token.span().start() <= cursor
                && cursor <= token.span().end()
                && is_word_component(token.kind())
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

        if tokens[index].kind() == TokenKind::Variable {
            let prefix = tokens[index].span().start()..cursor;
            return Some(Self {
                text: source[prefix].to_owned(),
                range: tokens[index].span().start()..tokens[index].span().end(),
                style: PathCompletionStyle::Bare,
                interpolated: false,
            });
        }

        let mut first = index;
        while first > 0
            && tokens[first - 1].span().end() == tokens[first].span().start()
            && is_word_component(tokens[first - 1].kind())
        {
            first -= 1;
        }
        let mut last = index;
        while last + 1 < tokens.len()
            && tokens[last].span().end() == tokens[last + 1].span().start()
            && is_word_component(tokens[last + 1].kind())
        {
            last += 1;
        }
        let mut interpolated = false;
        if let Some(variable) = (first..index)
            .rev()
            .find(|position| tokens[*position].kind() == TokenKind::Variable)
        {
            first = variable + 1;
            interpolated = true;
        }
        let start = tokens[first].span().start();
        let end = tokens[last].span().end();
        if first > 0
            && tokens[first - 1].span().end() == start
            && matches!(
                tokens[first - 1].kind(),
                TokenKind::Delimiter(Delimiter::RightBrace | Delimiter::RightParenthesis)
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
    if active.text.starts_with('$') {
        return CompletionContext::Variable { braced: false };
    }
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
            if character == '$' {
                return None;
            }
            decoded.push(character);
            continue;
        }
        match characters.next()? {
            '\\' => decoded.push('\\'),
            '"' => decoded.push('"'),
            '$' => decoded.push('$'),
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
        .filter(|token| {
            token.span().end() <= offset
                && !matches!(
                    token.kind(),
                    TokenKind::Whitespace
                        | TokenKind::Comment
                        | TokenKind::DocumentationComment
                        | TokenKind::LineContinuation
                )
        })
        .collect()
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
            | TokenKind::CommandSubstitutionStart
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
            | TokenKind::DoubleQuoteEnd
            | TokenKind::Variable
            | TokenKind::Operator(
                Operator::Assign
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
