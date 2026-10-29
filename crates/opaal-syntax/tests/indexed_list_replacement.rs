#![forbid(unsafe_code)]

use opaal_syntax::{
    ControlledParseOutcome, ExpressionKind, FormatOutcome, ParseOutcome, SourceFile, SourceId,
    StatementKind, format_source_opaal, parse_opaal, parse_opaal_with_control,
};
use std::cell::Cell;

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(901), "indexed.opaal", text)
}

#[test]
fn indexed_statement_preserves_original_spans_and_nested_expressions() {
    let input = source("items [ other[0] + position() ] =\n    replacement()\n");
    let ParseOutcome::Complete(script) = parse_opaal(&input) else {
        panic!("parse")
    };
    let StatementKind::IndexedAssignment(assignment) = script.statements()[0].kind() else {
        panic!("statement")
    };
    for (span, text) in [
        (assignment.target.span, "items"),
        (assignment.index.span(), "other[0] + position()"),
        (assignment.brackets, "[ other[0] + position() ]"),
        (assignment.operator, "="),
        (assignment.value.span(), "replacement()"),
    ] {
        assert_eq!(input.slice(span).unwrap(), text);
    }
    assert!(matches!(assignment.index.kind(), ExpressionKind::Binary(_)));
}

#[test]
fn selected_grammar_and_incomplete_input_have_one_path() {
    for text in ["items[", "items[0] =", "items[0] =\n", "items[other["] {
        assert!(
            matches!(parse_opaal(&source(text)), ParseOutcome::Incomplete(_)),
            "{text}"
        );
    }
    for text in [
        "items[] = 1",
        "items[0][1] = 1",
        "items.field[0] = 1",
        "items()[0] = 1",
        "(items)[0] = 1",
        "module::items[0] = 1",
        "items[0] += 1",
        "items[0] = other[0] = 1",
        "let x = (items[0] = 1)",
    ] {
        assert!(
            !matches!(parse_opaal(&source(text)), ParseOutcome::Complete(_)),
            "{text}"
        );
    }
    for text in [
        "items[0]",
        "generic[List[Int]]([1])",
        "^echo 'items[0]=value' '[0] = x'",
        "items[\nother[0]\n] =\n1",
    ] {
        assert!(
            matches!(parse_opaal(&source(text)), ParseOutcome::Complete(_)),
            "{text}"
        );
    }
}

#[test]
fn formatting_is_idempotent_with_comments_and_command_words() {
    let input =
        source("mut items = [1, 2]\nitems [ other[0]+1 ]=3 # 界🙂\n^echo 'items[0]=3' '[0] = x'\n");
    let expected =
        "mut items = [1, 2]\nitems[ other[0]+1 ] = 3 # 界🙂\n^echo 'items[0]=3' '[0] = x'\n";
    assert_eq!(
        format_source_opaal(&input),
        FormatOutcome::Complete(expected.into())
    );
    assert_eq!(
        format_source_opaal(&source(expected)),
        FormatOutcome::Complete(expected.into())
    );
}

#[test]
fn nested_lookahead_is_cancellable() {
    let input = source(&format!(
        "items[{}0{}] = 1",
        "other[".repeat(40),
        "]".repeat(40)
    ));
    let polls = Cell::new(0);
    assert!(matches!(
        parse_opaal_with_control(&input, &|| {
            polls.set(polls.get() + 1);
            polls.get() > 30
        }),
        ControlledParseOutcome::Cancelled
    ));
}
