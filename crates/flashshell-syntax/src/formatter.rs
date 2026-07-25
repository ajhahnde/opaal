use crate::{
    Delimiter, Diagnostic, IncompleteInput, ParseOutcome, SourceFile, Token, TokenKind, lex, parse,
};

/// The result of formatting one source file through the shared parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormatOutcome {
    Complete(String),
    Incomplete(IncompleteInput),
    Invalid(Vec<Diagnostic>),
}

/// Canonically formats complete source while retaining exact non-trivia spelling.
#[must_use]
pub fn format_source(source: &SourceFile) -> FormatOutcome {
    match parse(source) {
        ParseOutcome::Complete(_) => FormatOutcome::Complete(format_tokens(source, &lex(source))),
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

fn format_tokens(source: &SourceFile, tokens: &[Token]) -> String {
    let mut output = String::new();
    let mut open_delimiters = Vec::new();
    let mut indent = 0usize;
    let mut at_line_start = true;
    let mut pending_space = false;

    for token in tokens {
        match token.kind() {
            TokenKind::Whitespace => {
                if !at_line_start {
                    pending_space = true;
                }
            }
            TokenKind::Newline => {
                output.push('\n');
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
                at_line_start = true;
                pending_space = false;
            }
            kind => {
                if closes_indenting_brace(kind, &mut open_delimiters) {
                    indent = indent.saturating_sub(1);
                }

                write_prefix(&mut output, indent, &mut at_line_start, &mut pending_space);
                output.push_str(
                    token
                        .text(source)
                        .expect("lexer spans belong to their source"),
                );

                if let Some(open) = opening_delimiter(kind) {
                    open_delimiters.push(open);
                    if open == OpenDelimiter::IndentingBrace {
                        indent += 1;
                    }
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
