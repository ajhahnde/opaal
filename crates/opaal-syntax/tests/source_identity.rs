#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_syntax::{
    ControlledParseOutcome, FormatOutcome, ParseOutcome, SourceFile, SourceId, StatementKind,
    TokenKind, format_source_opaal, lex_opaal, parse_opaal, parse_opaal_with_control,
};

#[test]
fn directive_free_sources_accept_every_root_shape() {
    for text in [
        "",
        "# module note\n",
        "import './library.opaal' as library\n",
        "let answer = 42\n",
        "42\n",
        "## documented\ndef answer() { 42 }\n",
    ] {
        assert!(
            matches!(parse_opaal(&source(text)), ParseOutcome::Complete(_)),
            "directive-free root should parse: {text:?}"
        );
    }
}

#[test]
fn language_is_an_ordinary_identifier() {
    let file = source("language type enum action task\n");
    let tokens = lex_opaal(&file)
        .into_iter()
        .filter(|token| token.kind() != TokenKind::Whitespace)
        .take(5)
        .map(|token| token.kind())
        .collect::<Vec<_>>();

    assert_eq!(tokens[0], TokenKind::Identifier);
    assert!(
        tokens[1..]
            .iter()
            .all(|kind| matches!(kind, TokenKind::Keyword(_)))
    );
}

#[test]
fn a_former_header_has_only_ordinary_program_behavior() {
    let file = source("language 1\nlet answer = 42\n");
    let ParseOutcome::Complete(script) = parse_opaal(&file) else {
        panic!("former header text should use the ordinary parser");
    };

    assert_eq!(script.span(), file.span(0..file.len()).unwrap());
    assert_eq!(script.statements().len(), 2);
    assert!(matches!(
        script.statements()[0].kind(),
        StatementKind::Job(_)
    ));
    assert!(matches!(
        script.statements()[1].kind(),
        StatementKind::Declaration(_)
    ));
}

#[test]
fn repeated_former_headers_are_ordinary_statements() {
    let file = source("language 1\nlanguage 2\nlet answer = 42\n");
    let ParseOutcome::Complete(script) = parse_opaal(&file) else {
        panic!("repeated former headers should not activate directive diagnostics");
    };

    assert_eq!(script.statements().len(), 3);
    assert!(
        script.statements()[..2]
            .iter()
            .all(|statement| matches!(statement.kind(), StatementKind::Job(_)))
    );
}

#[test]
fn controlled_opaal_parsing_cancels_without_exposing_a_partial_outcome() {
    let text = (0..512)
        .map(|index| format!("let value_{index} = [{index}, {index}]\n"))
        .collect::<String>();
    let file = source(&text);
    let polls = AtomicUsize::new(0);

    let outcome = parse_opaal_with_control(&file, &|| polls.fetch_add(1, Ordering::Relaxed) >= 32);

    assert_eq!(outcome, ControlledParseOutcome::Cancelled);
    assert!(polls.load(Ordering::Relaxed) >= 33);
    assert!(matches!(parse_opaal(&file), ParseOutcome::Complete(_)));
}

#[test]
fn formatter_never_inserts_a_source_header() {
    let file = source("let answer =  {   value:42 }\n");
    assert_eq!(
        format_source_opaal(&file),
        FormatOutcome::Complete("let answer = { value:42 }\n".to_owned())
    );

    let former_header = source("language   1\nlet answer =  {   value:42 }\n");
    assert_eq!(
        format_source_opaal(&former_header),
        FormatOutcome::Complete("language 1\nlet answer = { value:42 }\n".to_owned())
    );
}

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(200), "source.opaal", text)
}
