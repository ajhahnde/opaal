use std::collections::BTreeSet;

use crate::{
    Delimiter, Diagnostic, IncompleteInput, Operator, ParseOutcome, Script, SourceFile,
    StatementKind, Token, TokenKind, lex_opaal, parse_opaal,
};

/// The result of formatting one source file through the shared parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormatOutcome {
    Complete(String),
    Incomplete(IncompleteInput),
    Invalid(Vec<Diagnostic>),
}

/// Canonically formats one complete OPAAL source.
#[must_use]
pub fn format_source_opaal(source: &SourceFile) -> FormatOutcome {
    match parse_opaal(source) {
        ParseOutcome::Complete(script) => {
            let tokens = lex_opaal(source);
            let layout = FormatLayout::for_script(&script, &tokens);
            FormatOutcome::Complete(format_tokens(source, &tokens, &layout))
        }
        ParseOutcome::Incomplete(incomplete) => FormatOutcome::Incomplete(incomplete),
        ParseOutcome::Invalid(diagnostics) => FormatOutcome::Invalid(diagnostics),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenDelimiter {
    IndentingBrace,
    ExpansionBrace,
    Parenthesis,
    Bracket,
}

#[derive(Default)]
struct FormatLayout {
    newline_before: BTreeSet<usize>,
    newline_after: BTreeSet<usize>,
    space_before: BTreeSet<usize>,
    space_after: BTreeSet<usize>,
    no_space_before: BTreeSet<usize>,
    no_space_after: BTreeSet<usize>,
    suppress_source_newline: BTreeSet<usize>,
}

impl FormatLayout {
    fn for_script(script: &Script, tokens: &[Token]) -> Self {
        let mut layout = Self::default();
        for statement in script.statements() {
            match statement.kind() {
                StatementKind::Action(action) => {
                    let signature_tokens = tokens
                        .iter()
                        .filter(|token| {
                            token.span().start() >= statement.span().start()
                                && token.span().end() <= action.effects_span.start()
                        })
                        .collect::<Vec<_>>();
                    let compact_length = signature_tokens
                        .iter()
                        .filter(|token| {
                            !matches!(token.kind(), TokenKind::Whitespace | TokenKind::Newline)
                        })
                        .map(|token| token.span().len().saturating_add(1))
                        .sum::<usize>();
                    let compactable = compact_length <= 100
                        && signature_tokens.iter().all(|token| {
                            !matches!(
                                token.kind(),
                                TokenKind::Comment
                                    | TokenKind::DocumentationComment
                                    | TokenKind::LineContinuation
                            )
                        });
                    if compactable {
                        layout.suppress_source_newline.extend(
                            signature_tokens
                                .iter()
                                .filter(|token| token.kind() == TokenKind::Newline)
                                .map(|token| token.span().start()),
                        );
                    }
                    layout.newline_before.insert(action.effects_span.start());
                    layout.newline_before.insert(action.effects_span.end() - 1);
                    layout.newline_before.insert(action.body.span.start());
                    layout.newline_before.insert(action.body.span.end() - 1);
                    layout.newline_after.insert(action.effects_span.end());
                    layout.newline_after.insert(action.body.span.start() + 1);
                    layout.newline_after.insert(action.body.span.end());
                    for request in &action.effects {
                        layout.newline_after.insert(request.span.end());
                    }
                    for (index, token) in tokens.iter().enumerate().filter(|(_, token)| {
                        token.span().start() >= statement.span().start()
                            && token.span().end() <= statement.span().end()
                    }) {
                        match token.kind() {
                            TokenKind::Operator(Operator::Arrow) => {
                                layout.space_before.insert(token.span().start());
                                layout.space_after.insert(token.span().end());
                            }
                            TokenKind::Operator(Operator::Colon)
                                if !tokens.get(index.wrapping_sub(1)).is_some_and(|token| {
                                    token.kind() == TokenKind::Operator(Operator::Colon)
                                }) && !tokens.get(index + 1).is_some_and(|token| {
                                    token.kind() == TokenKind::Operator(Operator::Colon)
                                }) =>
                            {
                                layout.no_space_before.insert(token.span().start());
                                layout.space_after.insert(token.span().end());
                            }
                            TokenKind::Operator(Operator::Comma) => {
                                layout.space_after.insert(token.span().end());
                            }
                            TokenKind::Delimiter(Delimiter::LeftParenthesis)
                                if token.span().end() <= action.effects_span.start() =>
                            {
                                layout.no_space_after.insert(token.span().end());
                                if let Some(next) = tokens[index + 1..].iter().find(|next| {
                                    !matches!(
                                        next.kind(),
                                        TokenKind::Whitespace | TokenKind::Newline
                                    )
                                }) {
                                    layout.no_space_before.insert(next.span().start());
                                }
                            }
                            TokenKind::Delimiter(Delimiter::RightParenthesis)
                                if token.span().end() <= action.effects_span.start() =>
                            {
                                layout.no_space_before.insert(token.span().start());
                            }
                            TokenKind::Delimiter(Delimiter::LeftBrace)
                                if token.span().start() > action.effects_span.start()
                                    && token.span().end() < action.effects_span.end() =>
                            {
                                layout.space_before.insert(token.span().start());
                                layout.newline_after.insert(token.span().end());
                            }
                            _ => {}
                        }
                    }
                }
                StatementKind::Task(_) => {
                    for token in tokens.iter().filter(|token| {
                        token.span().start() >= statement.span().start()
                            && token.span().end() <= statement.span().end()
                    }) {
                        if token.kind() == TokenKind::Operator(Operator::Assign) {
                            layout.space_before.insert(token.span().start());
                            layout.space_after.insert(token.span().end());
                        }
                    }
                }
                _ => {}
            }
        }
        layout
    }
}

fn format_tokens(source: &SourceFile, tokens: &[Token], layout: &FormatLayout) -> String {
    let mut output = String::new();
    let mut open_delimiters = Vec::new();
    let mut indent = 0usize;
    let mut at_line_start = true;
    let mut pending_space = false;
    let mut suppress_source_newline = false;
    let mut previous_syntax_token_end = None;

    for token in tokens {
        match token.kind() {
            TokenKind::Whitespace => {
                if !at_line_start {
                    pending_space = true;
                }
            }
            TokenKind::Newline => {
                if layout
                    .suppress_source_newline
                    .contains(&token.span().start())
                {
                    if !at_line_start
                        && !previous_syntax_token_end
                            .is_some_and(|end| layout.no_space_after.contains(&end))
                    {
                        pending_space = true;
                    }
                    continue;
                }
                if !suppress_source_newline {
                    output.push('\n');
                }
                at_line_start = true;
                pending_space = false;
            }
            TokenKind::LineContinuation => {
                write_prefix(&mut output, indent, &mut at_line_start, &mut pending_space);
                output.push_str(
                    token
                        .text(source)
                        .expect("lexer spans belong to their source"),
                );
                previous_syntax_token_end = Some(token.span().end());
                at_line_start = true;
                pending_space = false;
            }
            kind => {
                suppress_source_newline = false;
                if closes_indenting_brace(kind, &mut open_delimiters) {
                    indent = indent.saturating_sub(1);
                }

                if layout.newline_before.contains(&token.span().start()) && !at_line_start {
                    output.push('\n');
                    at_line_start = true;
                    pending_space = false;
                }
                if layout.no_space_before.contains(&token.span().start()) {
                    pending_space = false;
                } else if layout.space_before.contains(&token.span().start()) && !at_line_start {
                    pending_space = true;
                }

                write_prefix(&mut output, indent, &mut at_line_start, &mut pending_space);
                output.push_str(
                    token
                        .text(source)
                        .expect("lexer spans belong to their source"),
                );
                previous_syntax_token_end = Some(token.span().end());

                if let Some(open) = opening_delimiter(kind) {
                    open_delimiters.push(open);
                    if open == OpenDelimiter::IndentingBrace {
                        indent += 1;
                    }
                }
                if layout.space_after.contains(&token.span().end()) {
                    pending_space = true;
                }
                if layout.no_space_after.contains(&token.span().end()) {
                    pending_space = false;
                }
                if layout.newline_after.contains(&token.span().end()) {
                    output.push('\n');
                    at_line_start = true;
                    pending_space = false;
                    suppress_source_newline = true;
                }
            }
        }
    }

    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn write_prefix(
    output: &mut String,
    indent: usize,
    at_line_start: &mut bool,
    pending_space: &mut bool,
) {
    if *at_line_start {
        for _ in 0..indent {
            output.push_str("    ");
        }
        *at_line_start = false;
    } else if *pending_space {
        output.push(' ');
    }
    *pending_space = false;
}

fn opening_delimiter(kind: TokenKind) -> Option<OpenDelimiter> {
    match kind {
        TokenKind::Delimiter(Delimiter::LeftBrace) => Some(OpenDelimiter::IndentingBrace),
        TokenKind::BracedExpansionStart => Some(OpenDelimiter::ExpansionBrace),
        TokenKind::Delimiter(Delimiter::LeftParenthesis) | TokenKind::CommandSubstitutionStart => {
            Some(OpenDelimiter::Parenthesis)
        }
        TokenKind::Delimiter(Delimiter::LeftBracket) => Some(OpenDelimiter::Bracket),
        _ => None,
    }
}

fn closes_indenting_brace(kind: TokenKind, open: &mut Vec<OpenDelimiter>) -> bool {
    let expected = match kind {
        TokenKind::Delimiter(Delimiter::RightBrace) => {
            Some((OpenDelimiter::IndentingBrace, OpenDelimiter::ExpansionBrace))
        }
        TokenKind::Delimiter(Delimiter::RightParenthesis) => {
            pop_expected(open, OpenDelimiter::Parenthesis);
            return false;
        }
        TokenKind::Delimiter(Delimiter::RightBracket) => {
            pop_expected(open, OpenDelimiter::Bracket);
            return false;
        }
        _ => None,
    };

    let Some((block, expansion)) = expected else {
        return false;
    };
    match open.pop() {
        Some(actual) if actual == block => true,
        Some(actual) if actual == expansion => false,
        Some(actual) => {
            open.push(actual);
            false
        }
        None => false,
    }
}

fn pop_expected(open: &mut Vec<OpenDelimiter>, expected: OpenDelimiter) {
    match open.pop() {
        Some(actual) if actual == expected => {}
        Some(actual) => {
            open.push(actual);
        }
        None => {}
    }
}
