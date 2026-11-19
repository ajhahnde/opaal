#![forbid(unsafe_code)]

use opaal_syntax::{
    ExpressionKind, FormatOutcome, IncompleteReason, ParseOutcome, SourceFile, SourceId, StageKind,
    StatementKind, format_source_opaal, parse_opaal,
};

fn source(text: impl Into<String>) -> SourceFile {
    SourceFile::new(SourceId::new(0), "conditional.opaal", text)
}

#[test]
fn parses_value_arms_with_original_spans_and_postfix_precedence() {
    let conditional = "if first { [1] } else if second { [2] }\nelse { [3] }";
    let source = source(format!("let result = 4 + {conditional}[0]"));
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("{:?}", parse_opaal(&source));
    };
    let StatementKind::Declaration(declaration) = script.statements()[0].kind() else {
        panic!("expected declaration");
    };
    let ExpressionKind::Binary(binary) = declaration.value.kind() else {
        panic!("expected addition");
    };
    let ExpressionKind::Index(index) = binary.right.kind() else {
        panic!("expected index");
    };
    let ExpressionKind::If(branch) = index.target.kind() else {
        panic!("expected value conditional");
    };
    assert_eq!(source.slice(index.target.span()).unwrap(), conditional);
    assert_eq!(branch.arms.len(), 2);
    for (arm, (condition, value)) in branch
        .arms
        .iter()
        .zip([("first", "[1]"), ("second", "[2]")])
    {
        assert_eq!(source.slice(arm.condition.span()).unwrap(), condition);
        assert_eq!(source.slice(arm.value.span()).unwrap(), value);
    }
    assert_eq!(source.slice(branch.fallback.span()).unwrap(), "[3]");
}

#[test]
fn recognizes_value_positions_and_retains_leading_statement_dispatch() {
    let specimens = [
        "if true { return 1 }; if false { break } else if true { continue } else { throw 'error' }",
        "let record = Report {label: if true { 'pass' } else { 'fail' }}",
        "let record = {label: if true { {a: 1} } else { {a: 2} }}",
        "let value = if Report {passed: true}.passed { yes } else { no }",
        "let value = if Empty {} == Empty {} { 1 } else { 2 }",
        "let value = if (^status) && true { (echo answer) } else { 2 }",
        "let text = \"value {if true { 1 } else { 2 }}\"",
        "let f = {|flag: Bool| if flag { 1 } else { 2 }}",
        "echo hi | if true { 1 } else { 2 }",
    ];
    for text in specimens {
        let parsed = parse_opaal(&source(text));
        assert!(
            matches!(parsed, ParseOutcome::Complete(_)),
            "{text}: {parsed:?}"
        );
    }

    let ParseOutcome::Complete(script) =
        parse_opaal(&source("if true { 1 }; (if true { 1 } else { 2 })"))
    else {
        panic!("expected statement and grouped expression");
    };
    assert!(matches!(
        script.statements()[0].kind(),
        StatementKind::If(_)
    ));
    let StatementKind::Job(job) = script.statements()[1].kind() else {
        panic!("expected grouped job");
    };
    let StageKind::Expression(expression) =
        job.chain.or_terms()[0].and_terms()[0].stages()[0].kind()
    else {
        panic!("expected expression stage");
    };
    assert!(matches!(expression.kind(), ExpressionKind::GroupedJob(_)));
}

#[test]
fn distinguishes_missing_else_from_decisive_invalid_tokens() {
    let missing = source("let x = if true { 1 } else if false { 2 }\n# pending\n");
    let ParseOutcome::Incomplete(incomplete) = parse_opaal(&missing) else {
        panic!("expected incomplete input");
    };
    assert_eq!(incomplete.kind(), IncompleteReason::IfExpressionElse);
    assert_eq!(
        incomplete.span().start(),
        missing.text().rfind('}').unwrap()
    );

    for text in [
        "let x = (if true { 1 }",
        "let x = [if true { 1 }",
        "def x() { let y = if true { 1 }",
    ] {
        let input = source(text);
        let ParseOutcome::Incomplete(incomplete) = parse_opaal(&input) else {
            panic!("expected incomplete value arms for {text}");
        };
        assert_eq!(incomplete.kind(), IncompleteReason::IfExpressionElse);
        assert_eq!(incomplete.span().start(), text.rfind('}').unwrap());
    }
    for (text, offending) in [
        ("let x = if true {} else { 2 }", "}"),
        ("let x = if flag {} else { 2 }", "}"),
        ("let x = if true { 1 } else {}", "}"),
        ("let x = if true { 1; 2 } else { 3 }", ";"),
        ("let x = if true { 1\n2 } else { 3 }", "2"),
        ("let x = if true { 1 }; else { 2 }", ";"),
        ("let x = if true { return 1 } else { 2 }", "return"),
        ("let x = if true { 1 } else { break }", "break"),
        ("let x = if true { 1 } else { continue }", "continue"),
        ("let x = if true { 1 } else { throw 2 }", "throw"),
        ("let x = if true { 1 }\nlet y = 2", "let"),
        ("let x = if true { 1 } else { 2 } else { 3 }", "else"),
        ("let x = (if true { 1 };", ";"),
        ("let x = (if true { return 1", "return"),
        ("let x = (if true { }", "}"),
        ("let x = (if true { 1 } else { 2 } else {", "else"),
        ("let x = (if true { 1 } else { while true", "while"),
    ] {
        let source = source(text);
        let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
            panic!("{text}: {:?}", parse_opaal(&source));
        };
        assert_eq!(diagnostics[0].code(), "OP1000", "{text}");
        assert_eq!(
            source.slice(diagnostics[0].labels()[0].span()).unwrap(),
            offending,
            "{text}"
        );
    }
}

#[test]
fn nesting_limit_applies_before_descending_into_headers_and_arms() {
    for (prefix, suffix) in [
        ("if true { ", " } else { true }"),
        ("if (", ") { true } else { false }"),
        ("if true { ({|| ", "})() } else { false }"),
        (
            "if (true) { f[Bool](({field: [",
            "]}.field[0])) } else { false }",
        ),
        ("if f(({field: [", "]}.field[0])) { true } else { false }"),
    ] {
        let exact = source(format!(
            "let value = {}true{}",
            prefix.repeat(32),
            suffix.repeat(32)
        ));
        let excess = source(format!(
            "let value = {}true{}",
            prefix.repeat(33),
            suffix.repeat(33)
        ));
        let ParseOutcome::Complete(_) = parse_opaal(&exact) else {
            panic!("expected 32 conditionals to parse");
        };
        let ParseOutcome::Invalid(diagnostics) = parse_opaal(&excess) else {
            panic!("expected the 33rd conditional to be refused");
        };
        assert_eq!(diagnostics[0].code(), "OP1000");
        let span = diagnostics[0].labels()[0].span();
        assert_eq!(excess.slice(span).unwrap(), "if");
        assert_eq!(span.start(), "let value = ".len() + prefix.len() * 32);
        assert!(matches!(
            format_source_opaal(&exact),
            FormatOutcome::Complete(_)
        ));
        assert!(
            matches!(format_source_opaal(&excess), FormatOutcome::Invalid(ref diagnostics)
            if diagnostics[0].labels()[0].span() == span)
        );
    }
}
