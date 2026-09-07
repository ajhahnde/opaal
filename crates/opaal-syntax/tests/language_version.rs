#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_syntax::{
    ControlledVersionedParseOutcome, FormatOutcome, Keyword, LanguageDetection, LanguageIdentity,
    ParseOutcome, SourceFile, SourceId, StatementKind, TokenKind, VersionedParseOutcome,
    detect_source_language, format_source_opaal, lex_opaal, parse_opaal, parse_opaal_submission,
    parse_opaal_with_control,
};

#[test]
fn opaal_language_one_is_detected_after_leading_trivia() {
    let source =
        source("# module documentation\n\n    language 1  \t # version\nlet answer = 42\n");

    let LanguageDetection::Complete(directive) = detect_source_language(&source) else {
        panic!("an exact leading language directive should be accepted");
    };

    assert_eq!(directive.major(), LanguageIdentity::OpaalV1);
    assert_eq!(directive.major().get(), 1);
    assert_eq!(source.slice(directive.span()).unwrap(), "language 1");
}

#[test]
fn opaal_lexing_reserves_the_complete_language_one_keyword_set() {
    let source = source("language type enum action task\n");
    let tokens = lex_opaal(&source);

    assert_eq!(
        tokens
            .iter()
            .filter(|token| token.kind() != TokenKind::Whitespace)
            .take(5)
            .map(|token| token.kind())
            .collect::<Vec<_>>(),
        [
            TokenKind::Keyword(Keyword::Language),
            TokenKind::Keyword(Keyword::Type),
            TokenKind::Keyword(Keyword::Enum),
            TokenKind::Keyword(Keyword::Action),
            TokenKind::Keyword(Keyword::Task),
        ]
    );
}

#[test]
fn missing_or_late_language_directives_are_rejected_before_parsing() {
    for text in ["", "let answer = 42\n", "let answer = 42\nlanguage 1\n"] {
        let LanguageDetection::Invalid(diagnostics) = detect_source_language(&source(text)) else {
            panic!("missing or late directive should be invalid: {text:?}");
        };

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code(), "OP2001");
        assert_eq!(
            diagnostics[0].message(),
            "OPAAL source requires `language 1` as its first statement"
        );
        assert_eq!(diagnostics[0].labels().len(), 1);
    }
}

#[test]
fn malformed_and_unsupported_language_directives_are_distinct() {
    for (text, code, message) in [
        (
            "language\n",
            "OP2002",
            "language directive must be exactly `language 1`",
        ),
        (
            "language two\n",
            "OP2002",
            "language directive must be exactly `language 1`",
        ),
        (
            "language 02\n",
            "OP2002",
            "language directive must be exactly `language 1`",
        ),
        ("language 3\n", "OP2003", "unsupported language major `3`"),
    ] {
        let LanguageDetection::Invalid(diagnostics) = detect_source_language(&source(text)) else {
            panic!("invalid directive should be rejected: {text:?}");
        };

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code(), code);
        assert_eq!(diagnostics[0].message(), message);
    }
}

#[test]
fn a_second_language_directive_is_rejected() {
    let source = source("language 1\nlet answer = 42\nlanguage 1\n");
    let LanguageDetection::Invalid(diagnostics) = detect_source_language(&source) else {
        panic!("duplicate directive should be invalid");
    };

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code(), "OP2004");
    assert_eq!(
        diagnostics[0].message(),
        "language directive may appear only once"
    );
    assert_eq!(
        source.slice(diagnostics[0].labels()[0].span()).unwrap(),
        "language"
    );
}

#[test]
fn versioned_parsing_retains_one_directive_and_one_body_ast() {
    let source = source("# leading trivia\nlanguage 1\nlet answer = 42\n");
    let VersionedParseOutcome::Complete(versioned) = parse_opaal(&source) else {
        panic!("valid versioned source should parse");
    };

    assert_eq!(versioned.language(), LanguageIdentity::OpaalV1);
    assert_eq!(
        source.slice(versioned.directive().span()).unwrap(),
        "language 1"
    );
    assert_eq!(
        versioned.script().span(),
        source.span(0..source.len()).unwrap()
    );
    assert_eq!(versioned.script().statements().len(), 1);
    assert!(matches!(
        versioned.script().statements()[0].kind(),
        StatementKind::Declaration(_)
    ));
}

#[test]
fn versioned_parsing_requires_the_opaal_directive() {
    let versioned_source = source("language 1\nlet answer = 42\n");

    assert!(matches!(
        parse_opaal(&versioned_source),
        VersionedParseOutcome::Complete(_)
    ));

    let missing = source("let answer = 42\n");
    let VersionedParseOutcome::Invalid(diagnostics) = parse_opaal(&missing) else {
        panic!("opaal parsing must reject an unversioned source before body parsing");
    };
    assert_eq!(diagnostics[0].code(), "OP2001");
}

#[test]
fn opaal_repl_submissions_use_opaal_grammar_without_a_file_directive() {
    let accepted = source("let answer = 42\n");
    assert!(matches!(
        parse_opaal_submission(&accepted),
        ParseOutcome::Complete(_)
    ));

    let reserved = source("let language = 2\n");
    let ParseOutcome::Invalid(diagnostics) = parse_opaal_submission(&reserved) else {
        panic!("the preselected opaal submission grammar must reserve `language`");
    };
    assert_eq!(diagnostics[0].code(), "OP1000");
}

#[test]
fn controlled_opaal_parsing_cancels_without_exposing_a_partial_outcome() {
    let text = format!(
        "language 1\n{}",
        (0..512)
            .map(|index| format!("let value_{index} = [{index}, {index}]\n"))
            .collect::<String>()
    );
    let source = source(&text);
    let polls = AtomicUsize::new(0);

    let outcome =
        parse_opaal_with_control(&source, &|| polls.fetch_add(1, Ordering::Relaxed) >= 32);

    assert_eq!(outcome, ControlledVersionedParseOutcome::Cancelled);
    assert!(polls.load(Ordering::Relaxed) >= 33);
    assert!(matches!(
        parse_opaal(&source),
        VersionedParseOutcome::Complete(_)
    ));
}

#[test]
fn opaal_formatter_observes_and_retains_the_source_language() {
    let versioned_source = source("language   1\nlet answer =  {   value:42 }\n");

    assert_eq!(
        format_source_opaal(&versioned_source),
        FormatOutcome::Complete("language 1\nlet answer = { value:42 }\n".to_owned())
    );

    let missing = source("let answer = 42\n");
    let FormatOutcome::Invalid(diagnostics) = format_source_opaal(&missing) else {
        panic!("the opaal formatter must validate the file's own language directive");
    };
    assert_eq!(diagnostics[0].code(), "OP2001");
}

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(200), "versioned.opaal", text)
}
