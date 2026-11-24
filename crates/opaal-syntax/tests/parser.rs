#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_syntax::{
    BinaryOperator, CommandItemKind, ControlledParseOutcome, Expression, ExpressionKind,
    ParseOutcome, Pattern, SourceFile, SourceId, StageKind, StatementKind, parse_opaal,
    parse_opaal_with_control,
};

#[test]
fn inline_exports_keep_declaration_shapes_spans_and_recovery() {
    let text = concat!(
        "export let [first, _, ...rest] = [1, 2, 3]\n",
        "export mut count = 1\n",
        "export type Item = { value: Int }\n",
        "export enum Choice { Yes(Int), No }\n",
        "## Original documentation.\nexport def total() -> Int { return first }\n",
        "export action ready() -> Int { effects {} return total() }\n",
        "export { total }\n",
        "export NAME = 'value'\n",
        "if true { export NAME = 'value' }\n",
    );
    let script = complete(text);
    let plain = complete(&text.replacen("export ", "       ", 6));
    assert_eq!(script.statements(), plain.statements());
    for (index, statement) in script.statements().iter().enumerate() {
        let modifier = script.inline_export_span(statement);
        assert_eq!(
            modifier.map(|span| source_text(text, span)),
            (index < 6).then_some("export")
        );
    }
    assert_eq!(script.clone(), script);
    let StatementKind::Function(function) = script.statements()[4].kind() else {
        panic!("expected ordinary function");
    };
    assert_eq!(source_text(text, function.name.span()), "total");

    for text in [
        "if true { export let value = 1 }",
        "export export let value = 1",
        "export import std::math",
        "export task run = ready",
        "export\nlet value = 1",
        "export let [_, 1] = [0, 1]",
        "export let value 1",
        "export NAME 'value'",
    ] {
        let source = SourceFile::new(SourceId::new(802), "export.opaal", text);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Invalid(errors) if errors[0].code() == "OP1000"),
            "{text}"
        );
    }
    for text in [
        "export",
        "export def item() -> Int",
        "export let name =",
        "export mut",
        "export enum Item {",
        "export action ready() -> Int {",
        "export type Item =",
    ] {
        let source = SourceFile::new(SourceId::new(803), "export.opaal", text);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Incomplete(_)),
            "{text}"
        );
    }
}

#[test]
fn loop_headers_distinguish_an_index_from_tuple_slots() {
    let text = "for item in rows { item }\nfor index, (item, (_, label)) in rows {}\nfor (left, right) in rows {}\n";
    let script = complete(text);
    let loops = script
        .statements()
        .iter()
        .map(|statement| {
            let StatementKind::For(statement) = statement.kind() else {
                panic!("expected loop");
            };
            statement
        })
        .collect::<Vec<_>>();
    assert_eq!(loops[0].index, None);
    assert!(
        matches!(loops[0].binding, Pattern::Binding(item) if source_text(text, item.span()) == "item")
    );
    assert_eq!(source_text(text, loops[1].index.unwrap().span()), "index");
    let Pattern::Tuple(tuple) = &loops[1].binding else {
        panic!("expected tuple item");
    };
    assert_eq!(source_text(text, tuple.span), "(item, (_, label))");
    assert!(
        matches!(&tuple.elements[1], Pattern::Tuple(nested) if matches!(nested.elements[0], Pattern::Wildcard(_)))
    );
    assert_eq!(loops[2].index, None);
    assert!(matches!(loops[2].binding, Pattern::Tuple(_)));

    for text in [
        "for (left), item in rows {}",
        "for (left, right), item in rows {}",
        "for index, [left, right] in rows {}",
        "for (1, item) in rows {}",
        "for (Some(item), _) in rows {}",
        "for (left, ...rest) in rows {}",
    ] {
        let source = SourceFile::new(SourceId::new(800), "loops.opaal", text);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Invalid(_)),
            "{text}"
        );
    }
    for text in ["for index,", "for index, (item,", "for (item, _) in rows {"] {
        let source = SourceFile::new(SourceId::new(801), "loops.opaal", text);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Incomplete(_)),
            "{text}"
        );
    }
}

#[test]
fn loop_tuple_depth_is_bounded_even_with_space_before_each_comma() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            for (depth, accepted) in [(32, true), (33, false), (10_000, false)] {
                let text = format!(
                    "for i, {}_{} in [] {{}}",
                    "(_ , ".repeat(depth),
                    ")".repeat(depth)
                );
                let source = SourceFile::new(SourceId::new(802), "loop-depth.opaal", text);
                if accepted {
                    assert!(matches!(parse_opaal(&source), ParseOutcome::Complete(_)));
                    assert!(matches!(
                        opaal_syntax::format_source_opaal(&source),
                        opaal_syntax::FormatOutcome::Complete(_)
                    ));
                } else {
                    let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
                        panic!("loop pattern should exceed the depth limit");
                    };
                    assert_eq!(
                        diagnostics[0].message(),
                        "a path containing a tuple can nest at most 32 levels"
                    );
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn controlled_parsing_cancels_without_exposing_a_partial_parse_outcome() {
    let text = (0..512)
        .map(|index| format!("let value_{index} = [{index}, {index}]\n"))
        .collect::<String>();
    let source = SourceFile::new(SourceId::new(799), "cancelled.opaal", text);
    let polls = AtomicUsize::new(0);

    let outcome =
        parse_opaal_with_control(&source, &|| polls.fetch_add(1, Ordering::Relaxed) >= 32);

    assert_eq!(outcome, ControlledParseOutcome::Cancelled);
    assert!(polls.load(Ordering::Relaxed) >= 33);
    assert!(matches!(parse_opaal(&source), ParseOutcome::Complete(_)));
}

#[test]
fn tuple_paths_count_wrappers_on_both_sides_on_a_two_mib_stack() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            for (outside, inside) in [
                (31, 0),
                (0, 31),
                (14, 17),
                (32, 0),
                (0, 32),
                (14, 18),
                (10_000, 0),
                (0, 10_000),
            ] {
                for (open, close, leaf) in [("[", "]", "[]"), ("{field: ", "}", "{}")] {
                    let value = format!(
                        "{}({}{leaf}{}, 2){}",
                        open.repeat(outside),
                        open.repeat(inside),
                        close.repeat(inside),
                        close.repeat(outside),
                    );
                    let source = SourceFile::new(SourceId::new(1), "tuples.opaal", value);
                    match parse_opaal(&source) {
                        ParseOutcome::Complete(_) => assert_eq!(outside + inside, 31),
                        ParseOutcome::Invalid(diagnostics) => {
                            assert!(outside + inside > 31);
                            assert_eq!(
                                diagnostics[0].message(),
                                "a path containing a tuple can nest at most 32 levels"
                            );
                        }
                        ParseOutcome::Incomplete(input) => {
                            panic!("unexpected incomplete input: {input:?}")
                        }
                    }
                }
            }
            for value in [
                format!("{}(1, 2)", "!".repeat(10_000)),
                format!("(1, {}1)", "!".repeat(10_000)),
                format!("(1, 2){}", "[0]".repeat(10_000)),
                format!("(1, 2){}", " + 1".repeat(10_000)),
                format!("(0{}, 2)", " + 1".repeat(10_000)),
            ] {
                let source = SourceFile::new(SourceId::new(1), "tuple-chains.opaal", value);
                let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
                    panic!("the tuple path exceeds the construct limit");
                };
                assert_eq!(
                    diagnostics[0].message(),
                    "a path containing a tuple can nest at most 32 levels"
                );
            }
            for (inside, accepted) in [(17, true), (18, false)] {
                let value = format!(
                    "{}({}0{}, 2){}",
                    "[{field: ".repeat(7),
                    "[".repeat(inside),
                    "]".repeat(inside),
                    "}]".repeat(7),
                );
                let source = SourceFile::new(SourceId::new(1), "mixed-tuple-depth.opaal", value);
                assert_eq!(
                    matches!(parse_opaal(&source), ParseOutcome::Complete(_)),
                    accepted
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn structured_error_statements_retain_blocks_bindings_and_operands() {
    let text = "try { throw \"boom\" } catch error { throw error }\n";
    let script = complete(text);
    let StatementKind::Try(statement) = script.statements()[0].kind() else {
        panic!("expected try statement");
    };

    assert_eq!(source_text(text, statement.catch_binding.span()), "error");
    assert!(matches!(
        statement.try_block.statements[0].kind(),
        StatementKind::Throw(_)
    ));
    let StatementKind::Throw(expression) = statement.catch_block.statements[0].kind() else {
        panic!("expected rethrow statement");
    };
    assert_eq!(source_text(text, expression.span()), "error");
}

#[test]
fn catch_without_try_is_rejected_at_its_reserved_keyword() {
    let source = SourceFile::new(SourceId::new(91), "catch.opaal", "catch error { null }");
    let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
        panic!("orphan catch should be invalid");
    };
    assert_eq!(
        diagnostics[0].message(),
        "catch requires a preceding try statement"
    );
    assert_eq!(
        source.slice(diagnostics[0].labels()[0].span()).unwrap(),
        "catch"
    );
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
    let script = complete("let value = -compute(items)[0].size + 2 * 3 == 5\n");
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
fn a_bare_command_head_with_a_long_option_is_not_parsed_as_subtraction() {
    let script = complete("kill --kill %1\n");
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("expected a job statement");
    };
    let StageKind::Command(command) = job.chain.or_terms()[0].and_terms()[0].stages()[0].kind()
    else {
        panic!("expected a command stage");
    };
    assert_eq!(command.items.len(), 2);
}

#[test]
fn bare_name_conditions_stop_before_control_flow_blocks() {
    complete(
        "def invalid(flag: Bool) -> String {\n    if flag { return 'flagged' }\n    return 'done'\n}\n",
    );
    complete("let value = 1\nmatch value {\n    selected => { selected }\n}\n");
}

#[test]
fn named_function_bodies_accept_layout_without_widening_result_arrow_binding() {
    for signature in [
        "def identity(value: String)",
        "def identity(value: String) -> String",
        "def identity[T](value: T) -> T",
    ] {
        for trivia in [" ", "\n", " # signature\n\n# before body\n"] {
            let text = format!("{signature}{trivia}{{ return value }}\nlet next = 1\n");
            let script = complete(&text);
            assert_eq!(script.statements().len(), 2, "{text:?}");
            let StatementKind::Function(function) = script.statements()[0].kind() else {
                panic!("expected function");
            };
            assert_eq!(function.return_type.is_some(), signature.contains("->"));
            assert_eq!(source_text(&text, function.body.span), "{ return value }");
        }
        for suffix in ["\n", " # signature\n", "\n{\n", "\n{\nreturn value\n"] {
            let text = format!("{signature}{suffix}");
            let source = SourceFile::new(SourceId::new(904), "pending.opaal", &text);
            assert!(
                matches!(parse_opaal(&source), ParseOutcome::Incomplete(_)),
                "{text:?}"
            );
        }
        for suffix in [
            "\nlet next = 1\n",
            "\ndef next() {}\n",
            "\n-> String {}\n",
            "; {}\n",
        ] {
            let text = format!("{signature}{suffix}");
            let source = SourceFile::new(SourceId::new(905), "missing-body.opaal", &text);
            let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
                panic!("expected invalid function body: {text:?}");
            };
            assert_eq!(diagnostics[0].message(), "expected a block", "{text:?}");
        }
    }
}

#[test]
fn mode_boundaries_and_newline_continuation_are_syntax_driven() {
    let script = complete("let value = (1\n    + 2)\nlet call = compute(\n    value,\n)\n");
    assert_eq!(script.statements().len(), 2);

    for invalid in ["$(let value = 1)\n", "^ spaced\n"] {
        let source = SourceFile::new(SourceId::new(901), "invalid-mode.opaal", invalid);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Invalid(_)),
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
    let source = SourceFile::new(SourceId::new(902), "recovery.opaal", text);
    let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
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
        "match value {\n",
        "    bad if => { echo no }\n",
        "    ok => { echo yes }\n",
        "    broken => echo no\n",
        "}\n",
        "echo final\n",
    );
    let source = SourceFile::new(SourceId::new(903), "nested-recovery.opaal", text);
    let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
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

fn binary(expression: &Expression, operator: BinaryOperator) -> &opaal_syntax::BinaryExpression {
    let ExpressionKind::Binary(binary) = expression.kind() else {
        panic!("expected {operator:?}, got {:?}", expression.kind());
    };
    assert_eq!(*binary.operator.kind(), operator);
    binary
}

fn complete(text: &str) -> opaal_syntax::Script {
    let source = SourceFile::new(SourceId::new(900), "parser.opaal", text);
    match parse_opaal(&source) {
        ParseOutcome::Complete(script) => script,
        other => panic!("expected complete parse for {text:?}, got {other:#?}"),
    }
}

fn source_text(text: &str, span: opaal_syntax::Span) -> &str {
    &text[span.start()..span.end()]
}

#[test]
fn list_literal_spreads_keep_marker_operand_and_precedence_spans() {
    use opaal_syntax::ListItem;
    let text = "let values = [1, ...make()[0] + [2], ...[],]\n";
    let script = complete(text);
    let StatementKind::Declaration(declaration) = script.statements()[0].kind() else {
        panic!("declaration")
    };
    let ExpressionKind::List(items) = declaration.value.kind() else {
        panic!("list")
    };
    assert_eq!(items.len(), 3);
    assert!(matches!(&items[0], ListItem::Element(_)));
    let ListItem::Spread { marker, operand } = &items[1] else {
        panic!("spread")
    };
    assert_eq!(source_text(text, *marker), "...");
    assert_eq!(source_text(text, operand.span()), "make()[0] + [2]");
    assert!(matches!(operand.kind(), ExpressionKind::Binary(_)));
    for text in ["[...7]", "[...{x: 1}]", "[...[], 1, ...[2]]"] {
        complete(text);
    }
    for text in ["[...]", "[......items]", "[...items other]", "[1 ...items]"] {
        assert!(
            matches!(
                parse_opaal(&SourceFile::new(SourceId::new(0), "invalid", text)),
                ParseOutcome::Invalid(_)
            ),
            "{text}"
        );
    }
    for text in ["[...", "[...items", "[1,\n..."] {
        assert!(
            matches!(
                parse_opaal(&SourceFile::new(SourceId::new(0), "incomplete", text)),
                ParseOutcome::Incomplete(_)
            ),
            "{text}"
        );
    }
}
#[test]
fn tuple_group_commit_preserves_command_heads_and_reports_bad_slots() {
    use opaal_syntax::{
        ExpressionKind, ParseOutcome, SourceFile, SourceId, StatementKind, parse_opaal,
    };

    let parse = |text: &str| parse_opaal(&SourceFile::new(SourceId::new(987), "tuples", text));
    for (text, tuple) in [
        ("(a, b)", true),
        ("(a,\"b\")", true),
        ("(a + 1 , b)", true),
        ("(a,)", false),
        ("(a , b)", false),
        (r"(a\, b)", false),
        (r"(a\,b)", false),
        ("(which ,)", false),
        ("(echo {item}, b)", false),
        ("(Row {item: 1}, b)", true),
        ("(^printf \"%s\" ,)", false),
    ] {
        let ParseOutcome::Complete(script) = parse(&format!("let value = {text}")) else {
            panic!("expected complete source for {text}");
        };
        let StatementKind::Declaration(declaration) = script.statements()[0].kind() else {
            panic!("expected declaration");
        };
        assert_eq!(
            matches!(declaration.value.kind(), ExpressionKind::Tuple(_)),
            tuple,
            "{text}"
        );
    }
    for text in ["(a, b c)", "(1,)", "(1,,2)", "(1,...item)"] {
        assert!(
            matches!(
                parse(&format!("let value = {text}")),
                ParseOutcome::Invalid(_)
            ),
            "{text}"
        );
    }
    assert!(matches!(
        parse("let value = (a,\n"),
        ParseOutcome::Incomplete(_)
    ));
}
