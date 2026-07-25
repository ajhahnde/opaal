#![forbid(unsafe_code)]

use std::fs;
use std::path::Path;

use flashshell_syntax::{
    Delimiter, Keyword, NumberKind, Operator, SourceFile, SourceId, TokenKind, lex,
};

#[test]
fn lexical_corpus_is_covered_exactly_and_makes_progress() {
    let root = workspace_root().join("tests/golden/lexical");
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = row.split('\t').collect();
        assert_eq!(fields.len(), 3, "malformed manifest row: {row}");
        let path = root.join(fields[1]);
        let text = fs::read_to_string(&path).unwrap();
        let source = SourceFile::new(SourceId::new(index as u32), fields[1], text);
        let tokens = lex(&source);

        assert!(!tokens.is_empty(), "{} produced no tokens", fields[1]);
        assert_eq!(
            tokens[0].span().start(),
            0,
            "{} has a leading gap",
            fields[1]
        );
        assert_eq!(
            tokens.last().unwrap().span().end(),
            source.len(),
            "{} has a trailing gap",
            fields[1]
        );

        for pair in tokens.windows(2) {
            assert_eq!(
                pair[0].span().end(),
                pair[1].span().start(),
                "{} has a gap or overlap between {:?} and {:?}",
                fields[1],
                pair[0],
                pair[1]
            );
        }
        assert_eq!(
            tokens
                .iter()
                .map(|token| token.text(&source).unwrap())
                .collect::<String>(),
            source.text(),
            "{} did not retain exact source spelling",
            fields[1]
        );
        assert!(
            tokens.iter().all(|token| !token.span().is_empty()),
            "{} contains a zero-width token",
            fields[1]
        );
        assert_eq!(
            tokens
                .iter()
                .any(|token| matches!(token.kind(), TokenKind::Invalid(_))),
            fields[0] == "invalid",
            "{} has the wrong lexical-error marker",
            fields[1]
        );
    }
}

#[test]
fn word_parts_comments_and_continuations_remain_distinct_and_adjacent() {
    let word_parts = lex_fixture("complete/word-parts.fsh");
    assert_token(&word_parts, "pre", TokenKind::Identifier);
    assert_token(&word_parts, "\"", TokenKind::DoubleQuoteStart);
    assert_token(&word_parts, "$name", TokenKind::Variable);
    assert_token(&word_parts, "'post'", TokenKind::SingleQuoted);
    assert_token(&word_parts, "\\ ", TokenKind::BareEscape);
    assert_token(&word_parts, "''", TokenKind::SingleQuoted);
    assert_token(&word_parts, "\\|", TokenKind::BareEscape);

    let pre = token_with_text(&word_parts, "pre");
    let quote = token_with_text(&word_parts, "\"");
    assert!(pre.is_adjacent_to(quote));

    let comments = lex_fixture("complete/comments.fsh");
    assert_eq!(
        token_with_text(&comments, "# a whole-line comment").kind(),
        TokenKind::Comment
    );
    assert_eq!(
        token_with_text(&comments, "# a trailing comment").kind(),
        TokenKind::Comment
    );
    assert_ne!(token_with_text(&comments, "#42").kind(), TokenKind::Comment);

    let continuations = lex_fixture("complete/continuation.fsh");
    assert_eq!(
        continuations
            .tokens
            .iter()
            .filter(|token| token.kind() == TokenKind::LineContinuation)
            .count(),
        2
    );
}

#[test]
fn expansions_operators_numbers_keywords_and_delimiters_have_stable_kinds() {
    let interpolation = lex_fixture("complete/interpolation.fsh");
    assert_token(&interpolation, "$name", TokenKind::Variable);
    assert_token(&interpolation, "${", TokenKind::BracedExpansionStart);
    assert_token(&interpolation, "$(", TokenKind::CommandSubstitutionStart);
    assert_token(&interpolation, "\\$", TokenKind::DoubleEscape);

    let reserved = lex_fixture("complete/reserved-words.fsh");
    assert_token(&reserved, "let", TokenKind::Keyword(Keyword::Let));
    assert_token(&reserved, "null", TokenKind::Keyword(Keyword::Null));

    let grammar = fixture_source("grammar/complete/literals-and-collections.fsh");
    let tokens = lex(&grammar);
    assert_token_in(
        &grammar,
        &tokens,
        "42",
        TokenKind::Number(NumberKind::Integer),
    );
    assert_token_in(
        &grammar,
        &tokens,
        "0xff",
        TokenKind::Number(NumberKind::Integer),
    );
    assert_token_in(
        &grammar,
        &tokens,
        "3.5",
        TokenKind::Number(NumberKind::Float),
    );
    assert_token_in(
        &grammar,
        &tokens,
        "[",
        TokenKind::Delimiter(Delimiter::LeftBracket),
    );

    let operators = fixture_source("grammar/complete/operators.fsh");
    let tokens = lex(&operators);
    assert_token_in(
        &operators,
        &tokens,
        "..=",
        TokenKind::Operator(Operator::RangeInclusive),
    );
    assert_token_in(
        &operators,
        &tokens,
        "==",
        TokenKind::Operator(Operator::Equal),
    );

    let redirects = fixture_source("grammar/complete/redirections.fsh");
    let tokens = lex(&redirects);
    assert_token_in(
        &redirects,
        &tokens,
        ">>",
        TokenKind::Operator(Operator::Append),
    );
    assert_token_in(
        &redirects,
        &tokens,
        ">&",
        TokenKind::Operator(Operator::Duplicate),
    );
}

struct LexFixture {
    source: SourceFile,
    tokens: Vec<flashshell_syntax::Token>,
}

fn lex_fixture(relative: &str) -> LexFixture {
    let source = fixture_source(&format!("lexical/{relative}"));
    let tokens = lex(&source);
    LexFixture { source, tokens }
}

fn fixture_source(relative: &str) -> SourceFile {
    let path = workspace_root().join("tests/golden").join(relative);
    SourceFile::new(
        SourceId::new(91),
        relative,
        fs::read_to_string(path).unwrap(),
    )
}

fn assert_token(fixture: &LexFixture, text: &str, kind: TokenKind) {
    assert_token_in(&fixture.source, &fixture.tokens, text, kind);
}

fn assert_token_in(
    source: &SourceFile,
    tokens: &[flashshell_syntax::Token],
    text: &str,
    kind: TokenKind,
) {
    assert_eq!(
        tokens
            .iter()
            .find(|token| token.text(source).unwrap() == text)
            .unwrap_or_else(|| panic!("missing token {text:?}"))
            .kind(),
        kind
    );
}

fn token_with_text<'a>(fixture: &'a LexFixture, text: &str) -> &'a flashshell_syntax::Token {
    fixture
        .tokens
        .iter()
        .find(|token| token.text(&fixture.source).unwrap() == text)
        .unwrap_or_else(|| panic!("missing token {text:?}"))
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
}
