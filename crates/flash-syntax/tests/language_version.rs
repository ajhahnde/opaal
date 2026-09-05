#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use flash_syntax::{
    ControlledVersionedParseOutcome, FormatOutcome, Keyword, LanguageDetection, LanguageMajor,
    ParseOutcome, SourceFile, SourceId, StatementKind, TokenKind, VersionedParseOutcome,
    detect_source_language, format_source, format_source_v2, lex, lex_v2, parse, parse_v2,
    parse_v2_submission, parse_v2_with_control,
};

#[test]
fn language_two_is_detected_after_leading_trivia() {
    let source =
        source("# module documentation\n\n    language 2  \t # version\nlet answer = 42\n");

    let LanguageDetection::Complete(directive) = detect_source_language(&source) else {
        panic!("an exact leading language directive should be accepted");
    };

    assert_eq!(directive.major(), LanguageMajor::V2);
    assert_eq!(LanguageMajor::V1.get(), 1);
    assert_eq!(directive.major().get(), 2);
    assert_eq!(source.slice(directive.span()).unwrap(), "language 2");
}

#[test]
fn frozen_v1_lexing_does_not_reserve_the_v2_language_word() {
    let v1_source = source("language type enum action task\n");
    let tokens = lex(&v1_source);

    assert_eq!(
        tokens
            .iter()
            .filter(|token| token.kind() != TokenKind::Whitespace)
            .take(5)
            .map(|token| token.kind())
            .collect::<Vec<_>>(),
        vec![TokenKind::Identifier; 5]
    );
    assert!(matches!(
        detect_source_language(&source("language 2\n")),
        LanguageDetection::Complete(_)
    ));
}

#[test]
fn v2_lexing_reserves_the_complete_major_two_keyword_set() {
    let source = source("language type enum action task\n");
    let tokens = lex_v2(&source);

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
    for text in ["", "let answer = 42\n", "let answer = 42\nlanguage 2\n"] {
        let LanguageDetection::Invalid(diagnostics) = detect_source_language(&source(text)) else {
            panic!("missing or late directive should be invalid: {text:?}");
        };

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code(), "FS2001");
        assert_eq!(
            diagnostics[0].message(),
            "Flash 2 source requires `language 2` as its first statement"
        );
        assert_eq!(diagnostics[0].labels().len(), 1);
    }
}

#[test]
fn malformed_and_unsupported_language_directives_are_distinct() {
    for (text, code, message) in [
        (
            "language\n",
            "FS2002",
            "language directive must be exactly `language 2`",
        ),
        (
            "language two\n",
            "FS2002",
            "language directive must be exactly `language 2`",
        ),
        (
            "language 02\n",
            "FS2002",
            "language directive must be exactly `language 2`",
        ),
        ("language 3\n", "FS2003", "unsupported language major `3`"),
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
    let source = source("language 2\nlet answer = 42\nlanguage 2\n");
    let LanguageDetection::Invalid(diagnostics) = detect_source_language(&source) else {
        panic!("duplicate directive should be invalid");
    };

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code(), "FS2004");
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
    let source = source("# leading trivia\nlanguage 2\nlet answer = 42\n");
    let VersionedParseOutcome::Complete(versioned) = parse_v2(&source) else {
        panic!("valid versioned source should parse");
    };

    assert_eq!(versioned.language(), LanguageMajor::V2);
    assert_eq!(
        source.slice(versioned.directive().span()).unwrap(),
        "language 2"
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
fn versioned_and_frozen_v1_parsing_keep_separate_entry_contracts() {
    let versioned_source = source("language 2\nlet answer = 42\n");

    assert!(matches!(
        parse(&versioned_source),
        ParseOutcome::Complete(_)
    ));
    assert!(matches!(
        parse_v2(&versioned_source),
        VersionedParseOutcome::Complete(_)
    ));

    let missing = source("let answer = 42\n");
    let VersionedParseOutcome::Invalid(diagnostics) = parse_v2(&missing) else {
        panic!("v2 parsing must reject an unversioned source before body parsing");
    };
    assert_eq!(diagnostics[0].code(), "FS2001");
}

#[test]
fn v2_repl_submissions_use_v2_grammar_without_a_file_directive() {
    let accepted = source("let answer = 42\n");
    assert!(matches!(
        parse_v2_submission(&accepted),
        ParseOutcome::Complete(_)
    ));

    let reserved = source("let language = 2\n");
    let ParseOutcome::Invalid(diagnostics) = parse_v2_submission(&reserved) else {
        panic!("the preselected v2 submission grammar must reserve `language`");
    };
    assert_eq!(diagnostics[0].code(), "FS1000");

    assert!(matches!(parse(&reserved), ParseOutcome::Complete(_)));
}

#[test]
fn controlled_v2_parsing_cancels_without_exposing_a_partial_outcome() {
    let text = format!(
        "language 2\n{}",
        (0..512)
            .map(|index| format!("let value_{index} = [{index}, {index}]\n"))
            .collect::<String>()
    );
    let source = source(&text);
    let polls = AtomicUsize::new(0);

    let outcome = parse_v2_with_control(&source, &|| polls.fetch_add(1, Ordering::Relaxed) >= 32);

    assert_eq!(outcome, ControlledVersionedParseOutcome::Cancelled);
    assert!(polls.load(Ordering::Relaxed) >= 33);
    assert!(matches!(
        parse_v2(&source),
        VersionedParseOutcome::Complete(_)
    ));
}

#[test]
fn v2_formatter_observes_and_retains_the_source_language() {
    let versioned_source = source("language   2\nlet answer =  {   value:42 }\n");

    assert_eq!(
        format_source_v2(&versioned_source),
        FormatOutcome::Complete("language 2\nlet answer = { value:42 }\n".to_owned())
    );

    let missing = source("let answer = 42\n");
    let FormatOutcome::Invalid(diagnostics) = format_source_v2(&missing) else {
        panic!("the v2 formatter must validate the file's own language directive");
    };
    assert_eq!(diagnostics[0].code(), "FS2001");

    assert!(matches!(
        format_source(&source("language 2\n")),
        FormatOutcome::Complete(_)
    ));
}

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(200), "versioned.fsh", text)
}
