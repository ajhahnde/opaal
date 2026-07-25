#![forbid(unsafe_code)]

use std::fs;
use std::path::Path;

use flashshell_syntax::{
    BinaryOperator, CommandItemKind, Expression, ExpressionKind, ParseOutcome, SourceFile,
    SourceId, StageKind, StatementKind, parse,
};

#[test]
fn grammar_manifest_boundaries_are_parsed_directly() {
    let root = workspace_root().join("tests/golden/grammar");
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = row.split('\t').collect();
        assert_eq!(fields.len(), 4, "malformed manifest row: {row}");
        let source = SourceFile::new(
            SourceId::new(800 + index as u32),
            fields[2],
            fs::read_to_string(root.join(fields[2])).unwrap(),
        );

        match (fields[0], parse(&source)) {
            ("complete", ParseOutcome::Complete(script)) => {
                assert_eq!(script.span(), source.span(0..source.len()).unwrap());
                assert!(!script.statements().is_empty(), "{}", fields[2]);
            }
            ("incomplete", ParseOutcome::Incomplete(incomplete)) => {
                assert_eq!(incomplete.reason(), fields[3], "{}", fields[2]);
                source.slice(incomplete.span()).unwrap();
            }
            ("invalid", ParseOutcome::Invalid(diagnostics)) => {
                assert!(!diagnostics.is_empty(), "{}", fields[2]);
                assert_eq!(diagnostics[0].message(), fields[3], "{}", fields[2]);
                for diagnostic in diagnostics {
                    assert!(!diagnostic.labels().is_empty(), "{}", fields[2]);
                    source.slice(diagnostic.labels()[0].span()).unwrap();
                }
            }
            (expected, actual) => panic!("{}: expected {expected}, got {actual:?}", fields[2]),
        }
    }
}

#[test]
fn command_control_precedence_has_distinct_ast_layers() {
    let script = complete("^a | ^b && ^c || ^d\n");
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("expected job");
    };

    assert_eq!(job.chain.or_terms().len(), 2);
    assert_eq!(job.chain.operators().len(), 1);
    assert_eq!(job.chain.or_terms()[0].and_terms().len(), 2);
    assert_eq!(job.chain.or_terms()[0].operators().len(), 1);
    assert_eq!(job.chain.or_terms()[0].and_terms()[0].stages().len(), 2);
    assert_eq!(job.chain.or_terms()[0].and_terms()[0].operators().len(), 1);
}

#[test]
fn expression_precedence_builds_postfix_unary_and_binary_shapes() {
    let script = complete("let value = -compute($items)[0].size + 2 * 3 == 5\n");
    let StatementKind::Declaration(declaration) = script.statements()[0].kind() else {
        panic!("expected declaration");
    };

    let equality = binary(&declaration.value, BinaryOperator::Equal);
    let addition = binary(&equality.left, BinaryOperator::Add);
    assert!(matches!(addition.left.kind(), ExpressionKind::Unary(_)));
    let ExpressionKind::Unary(unary) = addition.left.kind() else {
        unreachable!()
    };
    assert!(matches!(unary.operand.kind(), ExpressionKind::Member(_)));
    let multiplication = binary(&addition.right, BinaryOperator::Multiply);
    assert!(matches!(
        multiplication.left.kind(),
        ExpressionKind::Literal(_)
    ));
    assert!(matches!(
        multiplication.right.kind(),
        ExpressionKind::Literal(_)
    ));
}

#[test]
fn parsed_command_items_retain_argument_and_redirection_order() {
    let script = complete("^build first 2>errors second >output 2>&1\n");
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("expected job");
    };
    let StageKind::Command(stage) = job.chain.or_terms()[0].and_terms()[0].stages()[0].kind()
    else {
        panic!("expected command stage");
    };

    assert!(matches!(stage.items[0].kind(), CommandItemKind::Word(_)));
    assert!(matches!(
        stage.items[1].kind(),
        CommandItemKind::Redirection(_)
    ));
    assert!(matches!(stage.items[2].kind(), CommandItemKind::Word(_)));
    assert_eq!(
        stage
            .redirections()
            .map(|redirection| source_text(
                "^build first 2>errors second >output 2>&1\n",
                redirection.span()
            ))
            .collect::<Vec<_>>(),
        vec!["2>errors", ">output", "2>&1"]
    );
}

#[test]
fn mode_boundaries_and_newline_continuation_are_syntax_driven() {
    let script = complete("let value = (1\n    + 2)\nlet call = compute(\n    $value,\n)\n");
    assert_eq!(script.statements().len(), 2);

    for invalid in ["$(let value = 1)\n", "^ spaced\n"] {
        let source = SourceFile::new(SourceId::new(901), "invalid-mode.fsh", invalid);
        assert!(
            matches!(parse(&source), ParseOutcome::Invalid(_)),
            "{invalid:?}"
        );
    }
}

#[test]
fn independent_statement_errors_are_reported_without_cascades() {
    let text = concat!(
        "let first = ;\n",
        "echo valid\n",
        "| broken\n",
        "let second = 2\n",
        "let third = 1 < 2 < 3\n",
        "echo after\n",
    );
    let source = SourceFile::new(SourceId::new(902), "recovery.fsh", text);
    let ParseOutcome::Invalid(diagnostics) = parse(&source) else {
        panic!("expected invalid parse");
    };

    assert_eq!(diagnostics.len(), 3, "{diagnostics:#?}");
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message())
            .collect::<Vec<_>>(),
        vec![
            "expected an expression",
            "pipeline operator cannot begin a stage",
            "comparison operators are non-associative",
        ]
    );
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| source.slice(diagnostic.labels()[0].span()).unwrap())
            .collect::<Vec<_>>(),
        vec![";", "|", "<"]
    );
}

#[test]
fn recovery_respects_block_and_match_arm_boundaries() {
    let text = concat!(
        "def demo() {\n",
        "    let local = ;\n",
        "    echo valid\n",
        "    | broken\n",
        "}\n",
        "match $value {\n",
        "    bad if => { echo no }\n",
        "    ok => { echo yes }\n",
        "    broken => echo no\n",
        "}\n",
        "echo final\n",
    );
    let source = SourceFile::new(SourceId::new(903), "nested-recovery.fsh", text);
    let ParseOutcome::Invalid(diagnostics) = parse(&source) else {
        panic!("expected invalid parse");
    };

    assert_eq!(diagnostics.len(), 4, "{diagnostics:#?}");
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| source.slice(diagnostic.labels()[0].span()).unwrap())
            .collect::<Vec<_>>(),
        vec![";", "|", "=>", "echo"]
    );
}

fn binary(
    expression: &Expression,
    operator: BinaryOperator,
) -> &flashshell_syntax::BinaryExpression {
    let ExpressionKind::Binary(binary) = expression.kind() else {
        panic!("expected {operator:?}, got {:?}", expression.kind());
    };
    assert_eq!(*binary.operator.kind(), operator);
    binary
}

fn complete(text: &str) -> flashshell_syntax::Script {
    let source = SourceFile::new(SourceId::new(900), "parser.fsh", text);
    let ParseOutcome::Complete(script) = parse(&source) else {
        panic!("expected complete parse for {text:?}");
    };
    script
}

fn source_text(text: &str, span: flashshell_syntax::Span) -> &str {
    &text[span.start()..span.end()]
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
}
