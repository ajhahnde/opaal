#![forbid(unsafe_code)]

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use flash_syntax::{
    BinaryOperator, CommandCaptureKind, CommandItemKind, ControlledParseOutcome, Expression,
    ExpressionKind, ParseOutcome, SourceFile, SourceId, StageKind, StatementKind, parse,
    parse_with_control,
};

#[test]
fn controlled_parsing_cancels_without_exposing_a_partial_parse_outcome() {
    let text = (0..512)
        .map(|index| format!("let value_{index} = [{index}, {index}]\n"))
        .collect::<String>();
    let source = SourceFile::new(SourceId::new(799), "cancelled.fsh", text);
    let polls = AtomicUsize::new(0);

    let outcome = parse_with_control(&source, &|| polls.fetch_add(1, Ordering::Relaxed) >= 32);

    assert_eq!(outcome, ControlledParseOutcome::Cancelled);
    assert!(polls.load(Ordering::Relaxed) >= 33);
    assert!(matches!(parse(&source), ParseOutcome::Complete(_)));
}

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
fn a_static_import_retains_its_exact_path_span() {
    let text = "import './lib/math.fsh'\n";
    let script = complete(text);
    let StatementKind::Import(import) = script.statements()[0].kind() else {
        panic!("expected import declaration");
    };

    assert!(import.names.is_empty());
    assert_eq!(source_text(text, import.path), "'./lib/math.fsh'");
}

#[test]
fn explicit_module_exports_and_imports_retain_name_spans() {
    let text = concat!(
        "let answer = 42\n",
        "export { answer, add }\n",
        "import { answer, add, } from './lib/math.fsh'\n",
        "export EDITOR = 'fsh'\n",
    );
    let script = complete(text);
    let StatementKind::ModuleExport(export) = script.statements()[1].kind() else {
        panic!("expected module export");
    };
    let StatementKind::Import(import) = script.statements()[2].kind() else {
        panic!("expected named import");
    };

    assert_eq!(
        export
            .names
            .iter()
            .map(|name| source_text(text, name.span()))
            .collect::<Vec<_>>(),
        ["answer", "add"]
    );
    assert_eq!(
        import
            .names
            .iter()
            .map(|name| source_text(text, name.span()))
            .collect::<Vec<_>>(),
        ["answer", "add"]
    );
    assert_eq!(source_text(text, import.path), "'./lib/math.fsh'");
    assert!(matches!(
        script.statements()[3].kind(),
        StatementKind::Environment(_)
    ));
}

#[test]
fn imports_reject_empty_dynamic_and_nested_paths() {
    for (text, message) in [
        ("import ''\n", "import path cannot be empty"),
        (
            "import \"./dynamic.fsh\"\n",
            "import requires a single-quoted path",
        ),
        (
            "if true {\n    import './nested.fsh'\n}\n",
            "imports are allowed only at module top level",
        ),
    ] {
        let source = SourceFile::new(SourceId::new(899), "invalid-import.fsh", text);
        let ParseOutcome::Invalid(diagnostics) = parse(&source) else {
            panic!("expected invalid import for {text:?}");
        };
        assert_eq!(diagnostics[0].message(), message);
    }
}

#[test]
fn module_name_lists_reject_empty_wildcard_dynamic_and_nested_forms() {
    for (text, message) in [
        ("export {}\n", "module export list cannot be empty"),
        (
            "import {} from './lib.fsh'\n",
            "import name list cannot be empty",
        ),
        ("import { * } from './lib.fsh'\n", "expected an identifier"),
        (
            "import { answer } from \"./dynamic.fsh\"\n",
            "import requires a single-quoted path",
        ),
        (
            "if true {\n    export { answer }\n}\n",
            "module exports are allowed only at module top level",
        ),
    ] {
        let source = SourceFile::new(SourceId::new(899), "invalid-module-name.fsh", text);
        let ParseOutcome::Invalid(diagnostics) = parse(&source) else {
            panic!("expected invalid module name form for {text:?}");
        };
        assert_eq!(diagnostics[0].message(), message, "{text:?}");
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
fn command_substitution_modifiers_select_capture_only_in_the_leading_slot() {
    let text = concat!(
        "let binary = $(bytes: ^tool)\n",
        "let explicit_text = $(text: ^tool)\n",
        "let shorthand = $(^tool)\n",
        "let ordinary = $(; bytes: ^tool)\n",
        "^bytes bytes: text:\n",
    );
    let script = complete(text);

    for (index, expected) in [
        (0, CommandCaptureKind::Bytes),
        (1, CommandCaptureKind::Text),
        (2, CommandCaptureKind::Text),
        (3, CommandCaptureKind::Text),
    ] {
        let StatementKind::Declaration(declaration) = script.statements()[index].kind() else {
            panic!("expected declaration");
        };
        let ExpressionKind::CommandSubstitution(substitution) = declaration.value.kind() else {
            panic!("expected command substitution");
        };
        assert_eq!(substitution.capture(), expected);
        assert_eq!(
            substitution
                .modifier_span()
                .map(|span| source_text(text, span)),
            match index {
                0 => Some("bytes:"),
                1 => Some("text:"),
                _ => None,
            }
        );
        assert_eq!(substitution.chain().or_terms().len(), 1);
    }

    assert!(matches!(
        script.statements()[4].kind(),
        StatementKind::Job(_)
    ));
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

fn binary(expression: &Expression, operator: BinaryOperator) -> &flash_syntax::BinaryExpression {
    let ExpressionKind::Binary(binary) = expression.kind() else {
        panic!("expected {operator:?}, got {:?}", expression.kind());
    };
    assert_eq!(*binary.operator.kind(), operator);
    binary
}

fn complete(text: &str) -> flash_syntax::Script {
    let source = SourceFile::new(SourceId::new(900), "parser.fsh", text);
    let ParseOutcome::Complete(script) = parse(&source) else {
        panic!("expected complete parse for {text:?}");
    };
    script
}

fn source_text(text: &str, span: flash_syntax::Span) -> &str {
    &text[span.start()..span.end()]
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
}
