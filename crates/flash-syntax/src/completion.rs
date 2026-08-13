//! Lossless token/cursor classification shared by editor adapters.

use std::ops::Range;

use crate::{Delimiter, Operator, SourceFile, SourceId, Token, TokenKind, lex};

/// The syntax-owned role of one completion replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompletionContext {
    Command { forced_external: bool },
    Variable,
    Flag { command: String },
    Path,
    None,
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
    if active.quoted {
        return None;
    }
    let prior = significant_before(&tokens, active.range.start);
    let stage = current_stage(&prior);
    Some(CompletionTarget {
        context: classify_context(source, stage, &active),
        replacement: active.range,
        prefix: active.text.to_owned(),
    })
}

struct ActiveWord<'source> {
    range: Range<usize>,
    text: &'source str,
    quoted: bool,
}

impl<'source> ActiveWord<'source> {
    fn at(source: &'source str, tokens: &[Token], cursor: usize) -> Option<Self> {
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
                text: "",
                quoted: false,
            });
        };

        if tokens[index].kind() == TokenKind::Variable {
            let prefix = tokens[index].span().start()..cursor;
            return Some(Self {
                text: &source[prefix],
                range: tokens[index].span().start()..tokens[index].span().end(),
                quoted: false,
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
        let start = tokens[first].span().start();
        let end = tokens[last].span().end();
        Some(Self {
            text: &source[start..cursor],
            range: start..end,
            quoted: (first..=last).any(|token| is_quoted(tokens[token].kind())),
        })
    }
}

fn classify_context(source: &str, stage: &[&Token], active: &ActiveWord<'_>) -> CompletionContext {
    if active.text.starts_with('$') {
        return CompletionContext::Variable;
    }
    if stage
        .last()
        .is_some_and(|token| is_file_redirect(token.kind()))
    {
        return CompletionContext::Path;
    }

    let forced_external = stage.first().is_some_and(|token| {
        token.kind() == TokenKind::Operator(Operator::Caret)
            && token.span().end() == active.range.start
    });
    let head_tokens = if forced_external { &stage[1..] } else { stage };
    let Some(head_end) = first_word_end(head_tokens) else {
        return CompletionContext::Command { forced_external };
    };
    if active.range.start < head_end {
        return CompletionContext::Command { forced_external };
    }

    let head_start = head_tokens[0].span().start();
    let command = &source[head_start..head_end];
    if active.text.starts_with('-') {
        return CompletionContext::Flag {
            command: command.to_owned(),
        };
    }
    if active.text.contains('/') || active.text.starts_with('.') || active.text.starts_with('~') {
        CompletionContext::Path
    } else {
        CompletionContext::None
    }
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

fn is_quoted(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::SingleQuoted
            | TokenKind::DoubleQuoteStart
            | TokenKind::DoubleText
            | TokenKind::DoubleEscape
            | TokenKind::DoubleQuoteEnd
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
