use crate::{
    Diagnostic, Keyword, NumberKind, Operator, Script, Severity, SourceFile, Span, Token,
    TokenKind, lex,
};

/// A source-language major understood by the current Flash toolchain.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LanguageMajor {
    #[default]
    V1,
    V2,
}

impl LanguageMajor {
    #[must_use]
    pub const fn get(self) -> u16 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }
}

/// The exact leading directive that selects a source language.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LanguageDirective {
    major: LanguageMajor,
    span: Span,
}

impl LanguageDirective {
    #[must_use]
    pub const fn major(self) -> LanguageMajor {
        self.major
    }

    #[must_use]
    pub const fn span(self) -> Span {
        self.span
    }
}

/// The result of detecting the required source-language directive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LanguageDetection {
    Complete(LanguageDirective),
    Invalid(Vec<Diagnostic>),
}

/// One parsed source module paired with its explicit language identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionedScript {
    directive: LanguageDirective,
    script: Script,
}

impl VersionedScript {
    pub(crate) const fn new(directive: LanguageDirective, script: Script) -> Self {
        Self { directive, script }
    }

    #[must_use]
    pub const fn language(&self) -> LanguageMajor {
        self.directive.major()
    }

    #[must_use]
    pub const fn directive(&self) -> LanguageDirective {
        self.directive
    }

    #[must_use]
    pub const fn script(&self) -> &Script {
        &self.script
    }

    #[must_use]
    pub fn into_script(self) -> Script {
        self.script
    }
}

/// Detects an exact leading `language 2` statement from the lossless v1 token
/// stream without changing the frozen v1 lexer or parser vocabulary.
#[must_use]
pub fn detect_source_language(source: &SourceFile) -> LanguageDetection {
    let tokens = lex(source);
    detect_source_language_tokens(source, &tokens)
}

pub(crate) fn detect_source_language_tokens(
    source: &SourceFile,
    tokens: &[Token],
) -> LanguageDetection {
    let Some(language_index) = tokens.iter().position(|token| !is_leading_trivia(token)) else {
        return LanguageDetection::Invalid(vec![missing_directive(source, None)]);
    };
    let language = tokens[language_index];
    if !matches!(
        language.kind(),
        TokenKind::Identifier | TokenKind::Keyword(Keyword::Language)
    ) || token_text(source, &language) != "language"
    {
        return LanguageDetection::Invalid(vec![missing_directive(source, Some(language.span()))]);
    }

    let Some(major_index) = next_inline_token(tokens, language_index + 1) else {
        return LanguageDetection::Invalid(vec![malformed_directive(language.span())]);
    };
    let major = tokens[major_index];
    let major_text = token_text(source, &major);
    if major.kind() != TokenKind::Number(NumberKind::Integer) || major_text != "2" {
        let span = source
            .span(language.span().start()..major.span().end())
            .expect("directive tokens belong to the same source");
        if is_canonical_decimal_major(major_text) {
            return LanguageDetection::Invalid(vec![unsupported_major(major.span(), major_text)]);
        }
        return LanguageDetection::Invalid(vec![malformed_directive(span)]);
    }

    let directive_span = source
        .span(language.span().start()..major.span().end())
        .expect("directive tokens belong to the same source");
    if let Some(next) = tokens[major_index + 1..]
        .iter()
        .find(|token| token.kind() != TokenKind::Whitespace)
        && !is_directive_terminator(next)
    {
        let span = source
            .span(language.span().start()..next.span().end())
            .expect("directive tokens belong to the same source");
        return LanguageDetection::Invalid(vec![malformed_directive(span)]);
    }

    if let Some(duplicate) = duplicate_directive(source, &tokens[major_index + 1..]) {
        return LanguageDetection::Invalid(vec![duplicate]);
    }

    LanguageDetection::Complete(LanguageDirective {
        major: LanguageMajor::V2,
        span: directive_span,
    })
}

fn is_leading_trivia(token: &Token) -> bool {
    matches!(
        token.kind(),
        TokenKind::Whitespace
            | TokenKind::Newline
            | TokenKind::Comment
            | TokenKind::DocumentationComment
            | TokenKind::Operator(Operator::Semicolon)
    )
}

fn next_inline_token(tokens: &[Token], start: usize) -> Option<usize> {
    let index = tokens[start..]
        .iter()
        .position(|token| token.kind() != TokenKind::Whitespace)?
        + start;
    (!matches!(
        tokens[index].kind(),
        TokenKind::Newline
            | TokenKind::Comment
            | TokenKind::DocumentationComment
            | TokenKind::Operator(Operator::Semicolon)
    ))
    .then_some(index)
}

fn is_directive_terminator(token: &Token) -> bool {
    matches!(
        token.kind(),
        TokenKind::Newline
            | TokenKind::Comment
            | TokenKind::DocumentationComment
            | TokenKind::Operator(Operator::Semicolon)
    )
}

fn duplicate_directive(source: &SourceFile, tokens: &[Token]) -> Option<Diagnostic> {
    let mut at_statement_start = false;
    for token in tokens {
        match token.kind() {
            TokenKind::Whitespace | TokenKind::Comment | TokenKind::DocumentationComment => {}
            TokenKind::Newline | TokenKind::Operator(Operator::Semicolon) => {
                at_statement_start = true;
            }
            TokenKind::Identifier | TokenKind::Keyword(Keyword::Language)
                if at_statement_start && token_text(source, token) == "language" =>
            {
                return Some(
                    Diagnostic::new(
                        Severity::Error,
                        "FS2004",
                        "language directive may appear only once",
                    )
                    .with_primary(token.span(), "duplicate language directive"),
                );
            }
            _ => at_statement_start = false,
        }
    }
    None
}

fn is_canonical_decimal_major(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text.len() == 1 || !text.starts_with('0'))
}

fn token_text<'source>(source: &'source SourceFile, token: &Token) -> &'source str {
    token
        .text(source)
        .expect("tokens produced from a source have source-local spans")
}

fn missing_directive(source: &SourceFile, span: Option<Span>) -> Diagnostic {
    let span = span.unwrap_or_else(|| {
        source
            .span(0..0)
            .expect("the beginning of every source is a valid span")
    });
    Diagnostic::new(
        Severity::Error,
        "FS2001",
        "Flash 2 source requires `language 2` as its first statement",
    )
    .with_primary(span, "expected `language 2` here")
}

fn malformed_directive(span: Span) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        "FS2002",
        "language directive must be exactly `language 2`",
    )
    .with_primary(span, "invalid language directive")
}

fn unsupported_major(span: Span, major: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        "FS2003",
        format!("unsupported language major `{major}`"),
    )
    .with_primary(span, "unsupported language major")
}
