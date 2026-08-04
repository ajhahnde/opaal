use crate::{SourceFile, Span, SpanError};

/// A reserved source word.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Keyword {
    Break,
    Continue,
    Def,
    Else,
    Export,
    False,
    For,
    If,
    In,
    Let,
    Match,
    Mut,
    Null,
    Return,
    True,
    Unset,
    While,
}

/// The lexical shape of a numeric literal.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NumberKind {
    Integer,
    Float,
}

/// A source operator or punctuation token.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Operator {
    Semicolon,
    Pipe,
    PipeBoth,
    Background,
    And,
    Or,
    Append,
    Duplicate,
    Assign,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Range,
    RangeInclusive,
    Spread,
    Arrow,
    MatchArrow,
    Dot,
    Comma,
    Colon,
    Caret,
}

/// A paired structural delimiter.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Delimiter {
    LeftParenthesis,
    RightParenthesis,
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
}

/// A locally recognizable source error retained in the lossless token stream.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InvalidTokenKind {
    Nul,
    LoneCarriageReturn,
    UnknownDoubleEscape,
    EmptyUnicodeEscape,
    UnicodeSurrogate,
    UnicodeOutOfRange,
    MalformedUnicodeEscape,
}

/// The lexical role of an exact source range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TokenKind {
    Whitespace,
    Newline,
    LineContinuation,
    Comment,
    Identifier,
    Keyword(Keyword),
    Number(NumberKind),
    WordText,
    BareEscape,
    SingleQuoted,
    DoubleQuoteStart,
    DoubleText,
    DoubleEscape,
    DoubleQuoteEnd,
    Variable,
    BracedExpansionStart,
    CommandSubstitutionStart,
    Operator(Operator),
    Delimiter(Delimiter),
    Invalid(InvalidTokenKind),
}

/// One nonempty token referring to its exact bytes in a [`SourceFile`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Token {
    kind: TokenKind,
    span: Span,
}

impl Token {
    #[must_use]
    pub const fn kind(&self) -> TokenKind {
        self.kind
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    pub fn text<'source>(&self, source: &'source SourceFile) -> Result<&'source str, SpanError> {
        source.slice(self.span)
    }

    #[must_use]
    pub fn is_adjacent_to(&self, next: &Self) -> bool {
        self.span.source_id() == next.span.source_id() && self.span.end() == next.span.start()
    }
}

/// Converts a source file into nonempty, ordered, lossless tokens.
#[must_use]
pub fn lex(source: &SourceFile) -> Vec<Token> {
    Lexer::new(source).run()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Context {
    Normal,
    DoubleQuoted,
    BracedExpansion { nested: usize },
    CommandSubstitution { nested: usize },
}

struct Lexer<'source> {
    source: &'source SourceFile,
    position: usize,
    tokens: Vec<Token>,
    contexts: Vec<Context>,
}

impl<'source> Lexer<'source> {
    fn new(source: &'source SourceFile) -> Self {
        Self {
            source,
            position: 0,
            tokens: Vec::new(),
            contexts: vec![Context::Normal],
        }
    }

    fn run(mut self) -> Vec<Token> {
        while self.position < self.source.len() {
            let before = self.position;
            match self.contexts.last().copied().unwrap_or(Context::Normal) {
                Context::DoubleQuoted => self.lex_double_quoted(),
                Context::Normal
                | Context::BracedExpansion { .. }
                | Context::CommandSubstitution { .. } => self.lex_normal(),
            }
            assert!(
                self.position > before,
                "lexer must consume source on every step"
            );
        }
        self.tokens
    }

    fn lex_normal(&mut self) {
        if self.lex_invalid_source_character()
            || self.lex_horizontal_trivia()
            || self.lex_newline()
            || self.lex_bare_escape()
            || self.lex_comment()
            || self.lex_single_quoted()
            || self.lex_double_quote_start()
            || self.lex_expansion_start()
            || self.lex_operator_or_delimiter()
            || self.lex_identifier()
            || self.lex_number()
        {
            return;
        }
        self.lex_word_text();
    }

    fn lex_double_quoted(&mut self) {
        if self.starts_with("\"") {
            let start = self.position;
            self.position += 1;
            self.push(TokenKind::DoubleQuoteEnd, start);
            self.contexts.pop();
            return;
        }
        if self.lex_invalid_source_character() || self.lex_double_escape() {
            return;
        }
        if self.lex_expansion_start() {
            return;
        }

        let start = self.position;
        while self.position < self.source.len() {
            if self.starts_with("\"")
                || self.starts_with("\\")
                || self.is_expansion_start()
                || self.starts_with("\0")
                || self.is_lone_carriage_return()
            {
                break;
            }
            self.advance_scalar();
        }
        self.push(TokenKind::DoubleText, start);
    }

    fn lex_invalid_source_character(&mut self) -> bool {
        let (kind, width) = if self.starts_with("\0") {
            (InvalidTokenKind::Nul, 1)
        } else if self.is_lone_carriage_return() {
            (InvalidTokenKind::LoneCarriageReturn, 1)
        } else {
            return false;
        };
        let start = self.position;
        self.position += width;
        self.push(TokenKind::Invalid(kind), start);
        true
    }

    fn lex_horizontal_trivia(&mut self) -> bool {
        if !matches!(self.current_byte(), Some(b' ' | b'\t')) {
            return false;
        }
        let start = self.position;
        while matches!(self.current_byte(), Some(b' ' | b'\t')) {
            self.position += 1;
        }
        self.push(TokenKind::Whitespace, start);
        true
    }

    fn lex_newline(&mut self) -> bool {
        let width = if self.starts_with("\r\n") {
            2
        } else if self.starts_with("\n") {
            1
        } else {
            return false;
        };
        let start = self.position;
        self.position += width;
        self.push(TokenKind::Newline, start);
        true
    }

    fn lex_bare_escape(&mut self) -> bool {
        if !self.starts_with("\\") {
            return false;
        }
        let start = self.position;
        self.position += 1;
        if self.starts_with("\r\n") {
            self.position += 2;
            self.push(TokenKind::LineContinuation, start);
        } else if self.starts_with("\n") {
            self.position += 1;
            self.push(TokenKind::LineContinuation, start);
        } else {
            if self.position < self.source.len()
                && !self.starts_with("\0")
                && !self.is_lone_carriage_return()
            {
                self.advance_scalar();
            }
            self.push(TokenKind::BareEscape, start);
        }
        true
    }

    fn lex_comment(&mut self) -> bool {
        if !self.starts_with("#") || !self.is_token_boundary() {
            return false;
        }
        let start = self.position;
        while self.position < self.source.len()
            && !self.starts_with("\n")
            && !self.starts_with("\r\n")
            && !self.is_lone_carriage_return()
            && !self.starts_with("\0")
        {
            self.advance_scalar();
        }
        self.push(TokenKind::Comment, start);
        true
    }

    fn lex_single_quoted(&mut self) -> bool {
        if !self.starts_with("'") {
            return false;
        }
        let start = self.position;
        self.position += 1;
        while self.position < self.source.len() {
            if self.starts_with("'") {
                self.position += 1;
                break;
            }
            if self.starts_with("\0") || self.is_lone_carriage_return() {
                break;
            }
            self.advance_scalar();
        }
        self.push(TokenKind::SingleQuoted, start);
        true
    }

    fn lex_double_quote_start(&mut self) -> bool {
        if !self.starts_with("\"") {
            return false;
        }
        let start = self.position;
        self.position += 1;
        self.push(TokenKind::DoubleQuoteStart, start);
        self.contexts.push(Context::DoubleQuoted);
        true
    }

    fn lex_double_escape(&mut self) -> bool {
        if !self.starts_with("\\") {
            return false;
        }
        let start = self.position;
        self.position += 1;

        if self.starts_with("\r\n") {
            self.position += 2;
            self.push(TokenKind::LineContinuation, start);
            return true;
        }
        if self.starts_with("\n") {
            self.position += 1;
            self.push(TokenKind::LineContinuation, start);
            return true;
        }
        if self.starts_with("u{") {
            self.position += 2;
            while matches!(self.current_byte(), Some(byte) if byte.is_ascii_hexdigit()) {
                self.position += 1;
            }
            if self.starts_with("}") {
                self.position += 1;
            }
            let spelling = &self.source.text()[start..self.position];
            let kind =
                unicode_escape_error(spelling).map_or(TokenKind::DoubleEscape, TokenKind::Invalid);
            self.push(kind, start);
            return true;
        }

        let kind = match self.current_byte() {
            Some(b'\\' | b'"' | b'$' | b'n' | b'r' | b't' | b'0') => {
                self.position += 1;
                TokenKind::DoubleEscape
            }
            Some(_) => {
                self.advance_scalar();
                TokenKind::Invalid(InvalidTokenKind::UnknownDoubleEscape)
            }
            None => TokenKind::Invalid(InvalidTokenKind::UnknownDoubleEscape),
        };
        self.push(kind, start);
        true
    }

    fn lex_expansion_start(&mut self) -> bool {
        if !self.starts_with("$") {
            return false;
        }
        let start = self.position;
        if self.starts_with("${") {
            self.position += 2;
            self.push(TokenKind::BracedExpansionStart, start);
            self.contexts.push(Context::BracedExpansion { nested: 0 });
            return true;
        }
        if self.starts_with("$(") {
            self.position += 2;
            self.push(TokenKind::CommandSubstitutionStart, start);
            self.contexts
                .push(Context::CommandSubstitution { nested: 0 });
            return true;
        }
        if self
            .source
            .text()
            .get(self.position + 1..)
            .and_then(|rest| rest.as_bytes().first())
            .is_some_and(|byte| is_identifier_start(*byte))
        {
            self.position += 2;
            while self.current_byte().is_some_and(is_identifier_continue) {
                self.position += 1;
            }
            self.push(TokenKind::Variable, start);
            return true;
        }
        false
    }

    fn lex_operator_or_delimiter(&mut self) -> bool {
        if self.close_interpolation_if_needed() {
            return true;
        }

        const OPERATORS: &[(&str, Operator)] = &[
            ("...", Operator::Spread),
            ("..=", Operator::RangeInclusive),
            ("&&", Operator::And),
            ("||", Operator::Or),
            ("|&", Operator::PipeBoth),
            (">>", Operator::Append),
            (">&", Operator::Duplicate),
            ("->", Operator::Arrow),
            ("=>", Operator::MatchArrow),
            ("==", Operator::Equal),
            ("!=", Operator::NotEqual),
            ("<=", Operator::LessEqual),
            (">=", Operator::GreaterEqual),
            ("..", Operator::Range),
            (";", Operator::Semicolon),
            ("|", Operator::Pipe),
            ("&", Operator::Background),
            ("<", Operator::Less),
            (">", Operator::Greater),
            ("=", Operator::Assign),
            ("+", Operator::Plus),
            ("-", Operator::Minus),
            ("*", Operator::Star),
            ("/", Operator::Slash),
            ("%", Operator::Percent),
            ("!", Operator::Bang),
            (".", Operator::Dot),
            (",", Operator::Comma),
            (":", Operator::Colon),
            ("^", Operator::Caret),
        ];
        if let Some((spelling, operator)) = OPERATORS
            .iter()
            .find(|(spelling, _)| self.starts_with(spelling))
        {
            let start = self.position;
            self.position += spelling.len();
            self.push(TokenKind::Operator(*operator), start);
            return true;
        }

        let delimiter = match self.current_byte() {
            Some(b'(') => Delimiter::LeftParenthesis,
            Some(b')') => Delimiter::RightParenthesis,
            Some(b'{') => Delimiter::LeftBrace,
            Some(b'}') => Delimiter::RightBrace,
            Some(b'[') => Delimiter::LeftBracket,
            Some(b']') => Delimiter::RightBracket,
            _ => return false,
        };
        self.note_nested_interpolation(delimiter);
        let start = self.position;
        self.position += 1;
        self.push(TokenKind::Delimiter(delimiter), start);
        true
    }

    fn close_interpolation_if_needed(&mut self) -> bool {
        let closes = matches!(
            (self.contexts.last(), self.current_byte()),
            (Some(Context::BracedExpansion { nested: 0 }), Some(b'}'))
                | (Some(Context::CommandSubstitution { nested: 0 }), Some(b')'))
        );
        if !closes {
            return false;
        }
        let delimiter = if self.current_byte() == Some(b'}') {
            Delimiter::RightBrace
        } else {
            Delimiter::RightParenthesis
        };
        let start = self.position;
        self.position += 1;
        self.push(TokenKind::Delimiter(delimiter), start);
        self.contexts.pop();
        true
    }

    fn note_nested_interpolation(&mut self, delimiter: Delimiter) {
        let Some(context) = self.contexts.last_mut() else {
            return;
        };
        match (context, delimiter) {
            (Context::BracedExpansion { nested }, Delimiter::LeftBrace)
            | (Context::CommandSubstitution { nested }, Delimiter::LeftParenthesis) => {
                *nested += 1;
            }
            (Context::BracedExpansion { nested }, Delimiter::RightBrace)
            | (Context::CommandSubstitution { nested }, Delimiter::RightParenthesis)
                if *nested > 0 =>
            {
                *nested -= 1;
            }
            _ => {}
        }
    }

    fn lex_identifier(&mut self) -> bool {
        if !self.current_byte().is_some_and(is_identifier_start) {
            return false;
        }
        let start = self.position;
        self.position += 1;
        while self.current_byte().is_some_and(is_identifier_continue) {
            self.position += 1;
        }
        let text = &self.source.text()[start..self.position];
        let kind = keyword(text).map_or(TokenKind::Identifier, TokenKind::Keyword);
        self.push(kind, start);
        true
    }

    fn lex_number(&mut self) -> bool {
        if !self
            .current_byte()
            .is_some_and(|byte| byte.is_ascii_digit())
        {
            return false;
        }
        let start = self.position;
        if self.starts_with("0x") || self.starts_with("0X") {
            self.position += 2;
            self.consume_digits(|byte| byte.is_ascii_hexdigit());
            self.push(TokenKind::Number(NumberKind::Integer), start);
            return true;
        }
        if self.starts_with("0o") || self.starts_with("0O") {
            self.position += 2;
            self.consume_digits(|byte| matches!(byte, b'0'..=b'7'));
            self.push(TokenKind::Number(NumberKind::Integer), start);
            return true;
        }
        if self.starts_with("0b") || self.starts_with("0B") {
            self.position += 2;
            self.consume_digits(|byte| matches!(byte, b'0' | b'1'));
            self.push(TokenKind::Number(NumberKind::Integer), start);
            return true;
        }

        self.consume_digits(|byte| byte.is_ascii_digit());
        let mut kind = NumberKind::Integer;
        if self.current_byte() == Some(b'.')
            && self.peek_byte(1).is_some_and(|byte| byte.is_ascii_digit())
        {
            kind = NumberKind::Float;
            self.position += 1;
            self.consume_digits(|byte| byte.is_ascii_digit());
        }
        if matches!(self.current_byte(), Some(b'e' | b'E')) && self.exponent_has_digits() {
            kind = NumberKind::Float;
            self.position += 1;
            if matches!(self.current_byte(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            self.consume_digits(|byte| byte.is_ascii_digit());
        }
        self.push(TokenKind::Number(kind), start);
        true
    }

    fn lex_word_text(&mut self) {
        let start = self.position;
        while self.position < self.source.len() {
            let byte = self.current_byte().expect("position is in bounds");
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\\' | b'\'' | b'"')
                || self.is_expansion_start()
                || self.operator_or_delimiter_starts_here()
                || (byte == b'#' && self.is_token_boundary())
                || byte == b'\0'
            {
                break;
            }
            self.advance_scalar();
        }
        if self.position == start {
            self.advance_scalar();
        }
        self.push(TokenKind::WordText, start);
    }

    fn consume_digits(&mut self, accepts: impl Fn(u8) -> bool) {
        while self
            .current_byte()
            .is_some_and(|byte| accepts(byte) || byte == b'_')
        {
            self.position += 1;
        }
    }

    fn exponent_has_digits(&self) -> bool {
        let mut offset = 1;
        if matches!(self.peek_byte(offset), Some(b'+' | b'-')) {
            offset += 1;
        }
        self.peek_byte(offset)
            .is_some_and(|byte| byte.is_ascii_digit())
    }

    fn is_expansion_start(&self) -> bool {
        if !self.starts_with("$") {
            return false;
        }
        self.starts_with("${")
            || self.starts_with("$(")
            || self.peek_byte(1).is_some_and(is_identifier_start)
    }

    fn operator_or_delimiter_starts_here(&self) -> bool {
        self.current_byte().is_some_and(|byte| {
            matches!(
                byte,
                b';' | b'|'
                    | b'&'
                    | b'<'
                    | b'>'
                    | b'('
                    | b')'
                    | b'{'
                    | b'}'
                    | b'['
                    | b']'
                    | b'='
                    | b'+'
                    | b'-'
                    | b'*'
                    | b'/'
                    | b'%'
                    | b'!'
                    | b'.'
                    | b','
                    | b':'
                    | b'^'
            )
        })
    }

    fn is_token_boundary(&self) -> bool {
        self.position == 0
            || self.tokens.last().is_some_and(|token| {
                matches!(
                    token.kind,
                    TokenKind::Whitespace
                        | TokenKind::Newline
                        | TokenKind::Operator(Operator::Semicolon)
                        | TokenKind::Operator(Operator::Pipe)
                        | TokenKind::Operator(Operator::PipeBoth)
                        | TokenKind::Operator(Operator::Background)
                        | TokenKind::Operator(Operator::And)
                        | TokenKind::Operator(Operator::Or)
                        | TokenKind::Operator(Operator::Less)
                        | TokenKind::Operator(Operator::Greater)
                        | TokenKind::Operator(Operator::Append)
                        | TokenKind::Operator(Operator::Duplicate)
                        | TokenKind::BracedExpansionStart
                        | TokenKind::CommandSubstitutionStart
                        | TokenKind::Delimiter(_)
                )
            })
    }

    fn is_lone_carriage_return(&self) -> bool {
        self.starts_with("\r") && !self.starts_with("\r\n")
    }

    fn current_byte(&self) -> Option<u8> {
        self.source.text().as_bytes().get(self.position).copied()
    }

    fn peek_byte(&self, offset: usize) -> Option<u8> {
        self.source
            .text()
            .as_bytes()
            .get(self.position + offset)
            .copied()
    }

    fn starts_with(&self, spelling: &str) -> bool {
        self.source.text()[self.position..].starts_with(spelling)
    }

    fn advance_scalar(&mut self) {
        let character = self.source.text()[self.position..]
            .chars()
            .next()
            .expect("position is in bounds");
        self.position += character.len_utf8();
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        let span = self
            .source
            .span(start..self.position)
            .expect("lexer advances only across UTF-8 character boundaries");
        debug_assert!(!span.is_empty());
        self.tokens.push(Token { kind, span });
    }
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn keyword(text: &str) -> Option<Keyword> {
    Some(match text {
        "break" => Keyword::Break,
        "continue" => Keyword::Continue,
        "def" => Keyword::Def,
        "else" => Keyword::Else,
        "export" => Keyword::Export,
        "false" => Keyword::False,
        "for" => Keyword::For,
        "if" => Keyword::If,
        "in" => Keyword::In,
        "let" => Keyword::Let,
        "match" => Keyword::Match,
        "mut" => Keyword::Mut,
        "null" => Keyword::Null,
        "return" => Keyword::Return,
        "true" => Keyword::True,
        "unset" => Keyword::Unset,
        "while" => Keyword::While,
        _ => return None,
    })
}

fn unicode_escape_error(spelling: &str) -> Option<InvalidTokenKind> {
    let Some(hexadecimal) = spelling
        .strip_prefix("\\u{")
        .and_then(|body| body.strip_suffix('}'))
    else {
        return Some(InvalidTokenKind::MalformedUnicodeEscape);
    };
    if hexadecimal.is_empty() {
        return Some(InvalidTokenKind::EmptyUnicodeEscape);
    }
    if hexadecimal.len() > 6 {
        return Some(InvalidTokenKind::MalformedUnicodeEscape);
    }
    let Ok(value) = u32::from_str_radix(hexadecimal, 16) else {
        return Some(InvalidTokenKind::MalformedUnicodeEscape);
    };
    if value > 0x10_FFFF {
        return Some(InvalidTokenKind::UnicodeOutOfRange);
    }
    if (0xD800..=0xDFFF).contains(&value) {
        return Some(InvalidTokenKind::UnicodeSurrogate);
    }
    None
}
