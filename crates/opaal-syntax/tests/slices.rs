use opaal_syntax::{
    CompletionContext, Expression, ExpressionKind, FormatOutcome, ParseOutcome, Script, SourceFile,
    SourceId, StatementKind, SyntaxClassification, classify_opaal_tokens, completion_target,
    format_source_opaal, lex_opaal, parse_opaal,
};

fn source(text: impl Into<String>) -> SourceFile {
    SourceFile::new(SourceId::new(1), "slices.opaal", text.into())
}

fn parse(input: &SourceFile) -> Script {
    let result = parse_opaal(input);
    let ParseOutcome::Complete(script) = result else {
        panic!("{}\n{result:?}", input.text());
    };
    script
}

fn binding(script: &Script, index: usize) -> &Expression {
    let StatementKind::Declaration(declaration) = script.statements()[index].kind() else {
        panic!("expected declaration");
    };
    &declaration.value
}

#[test]
fn slice_keeps_spans_and_composes_with_postfix_calls_and_reads() {
    let input =
        source("let view = rows[limits::first:limits::last][0]\nlet invalid_call = rows[0:1]()");
    let script = parse(&input);
    let ExpressionKind::Index(index) = binding(&script, 0).kind() else {
        panic!("expected a postfix index");
    };
    let ExpressionKind::Slice(slice) = index.target.kind() else {
        panic!("expected a slice");
    };
    for (span, text) in [
        (index.target.span(), "rows[limits::first:limits::last]"),
        (slice.target.span(), "rows"),
        (slice.open, "["),
        (slice.start.span(), "limits::first"),
        (slice.separator, ":"),
        (slice.end.span(), "limits::last"),
        (slice.close, "]"),
    ] {
        assert_eq!(input.slice(span).unwrap(), text);
    }
    let ExpressionKind::Call(call) = binding(&script, 1).kind() else {
        panic!("expected an ordinary call");
    };
    assert!(call.type_arguments.is_empty());
    assert!(matches!(call.callee.kind(), ExpressionKind::Slice(_)));
    parse(&source("(rows[1:2]) | consume"));
}

#[test]
fn other_grammars_keep_their_own_colons() {
    let input = source(concat!(
        "let nested = id[List[types :: Row]]([Row { value: 1 }])\n",
        "let qualified = rows[limits :: first]\n",
        "let range = rows[0..2]\n",
        "let read = rows[if ^true a:b:c && true { 1 } else { 0 }]\n",
        "rows[if ^true a:b:c && true { 1 } else { 0 }] = 9\n",
        "let slice = rows[if ^true a:b:c && true { 1 } else { 0 }:2]\n",
        "let literal = [\"a:b[0:1]\", {limit: 1}]\n",
        "let comprehension = [row[0:1] for row in nested]\n",
        "let [first, ...rest] = nested\n",
    ));
    let script = parse(&input);
    let ExpressionKind::Call(call) = binding(&script, 0).kind() else {
        panic!("expected a generic call");
    };
    assert_eq!(call.type_arguments.len(), 1);
    for index in [1, 2, 3] {
        assert!(matches!(
            binding(&script, index).kind(),
            ExpressionKind::Index(_)
        ));
    }
    assert!(matches!(
        script.statements()[4].kind(),
        StatementKind::IndexedAssignment(_)
    ));
    assert!(matches!(
        binding(&script, 5).kind(),
        ExpressionKind::Slice(_)
    ));
    let FormatOutcome::Complete(formatted) = format_source_opaal(&input) else {
        panic!("expected formatted source");
    };
    assert_eq!(formatted.matches("^true a:b:c && true").count(), 3);
    assert!(formatted.contains("\"a:b[0:1]\""));
    parse(&source(formatted));
}

#[test]
fn completion_replaces_only_the_chosen_endpoint() {
    for text in [r#"let x = "é🙂"[first:last]"#, "let x = rows[first:last"] {
        for name in ["first", "last"] {
            let start = text.find(name).unwrap();
            let target = completion_target(text, start + 1).unwrap();
            assert_eq!(target.context(), &CompletionContext::Expression);
            assert_eq!(target.prefix(), &name[..1]);
            assert_eq!(target.replacement(), start..start + name.len());
        }
    }
    let text = "let x = rows[0:if ^true word:part && true { 1 } else { 0 }]";
    let start = text.find("word:part").unwrap();
    let target = completion_target(text, start + 1).unwrap();
    assert_ne!(target.context(), &CompletionContext::Expression);
    assert_eq!(target.replacement(), start..start + "word:part".len());
}

#[test]
fn formatting_binds_the_separator_without_relexing_comments() {
    let input = source("let a = rows [ 1 : 2 ]\nlet b = rows[1 # start\n : # end\n 2]\n");
    let formatted = "let a = rows[1:2]\nlet b = rows[1 # start\n: # end\n2]\n";
    assert_eq!(
        format_source_opaal(&input),
        FormatOutcome::Complete(formatted.into())
    );
    assert_eq!(
        format_source_opaal(&source(formatted)),
        FormatOutcome::Complete(formatted.into())
    );
}

#[test]
fn missing_and_extra_bounds_and_slice_writes_point_to_the_offending_token() {
    for (text, bad) in [
        ("let x = rows[:2]", 13),
        ("let x = rows[1:]", 15),
        ("let x = rows[:]", 13),
        ("let x = rows[1:2:3]", 16),
        ("rows[1:2] = []", 6),
    ] {
        let input = source(text);
        let ParseOutcome::Invalid(diagnostics) = parse_opaal(&input) else {
            panic!("expected invalid input for {text}");
        };
        assert_eq!(diagnostics[0].code(), "OP1000");
        assert_eq!(
            diagnostics[0].labels()[0].span(),
            input.span(bad..bad + 1).unwrap()
        );
        assert_eq!(
            classify_opaal_tokens(&input, &lex_opaal(&input)).unwrap(),
            SyntaxClassification::Invalid(diagnostics[0].clone())
        );
        assert_eq!(
            format_source_opaal(&input),
            FormatOutcome::Invalid(diagnostics)
        );
    }
    for text in ["let x = rows[1:", "let x = rows[1:2", "let x = rows[1:2 +"] {
        let input = source(text);
        assert!(
            matches!(parse_opaal(&input), ParseOutcome::Incomplete(_)),
            "{text}"
        );
        assert!(
            matches!(
                classify_opaal_tokens(&input, &lex_opaal(&input)).unwrap(),
                SyntaxClassification::Incomplete(_)
            ),
            "{text}"
        );
        assert!(
            matches!(format_source_opaal(&input), FormatOutcome::Incomplete(_)),
            "{text}"
        );
    }
}

#[test]
fn slice_paths_apply_the_structural_limit_at_the_first_excess() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            for depth in [32, 33, 2000] {
                for (text, excess) in [
                    (
                        format!("let x = {}0{}", "rows[0:".repeat(depth), "]".repeat(depth)),
                        "let x = ".len() + 32 * "rows[0:".len() + "rows".len(),
                    ),
                    (
                        format!("let x = rows{}[0:1]", "[0]".repeat(depth - 1)),
                        "let x = rows".len() + 32 * "[0]".len(),
                    ),
                    (
                        format!(
                            "{}let x = rows[0:1]{}",
                            "if true {\n".repeat(depth - 1),
                            "\n}".repeat(depth - 1)
                        ),
                        32 * "if true {\n".len()
                            + if depth == 33 {
                                "let x = rows".len()
                            } else {
                                "if true ".len()
                            },
                    ),
                ] {
                    let input = source(text);
                    match parse_opaal(&input) {
                        ParseOutcome::Complete(_) => {
                            assert_eq!(depth, 32);
                            assert!(matches!(
                                format_source_opaal(&input),
                                FormatOutcome::Complete(_)
                            ));
                        }
                        ParseOutcome::Invalid(diagnostics) => {
                            assert!(depth > 32, "{diagnostics:?}");
                            assert_eq!(diagnostics[0].code(), "OP1000");
                            assert_eq!(
                                diagnostics[0].labels()[0].span().start(),
                                excess,
                                "{diagnostics:?}"
                            );
                            assert_eq!(
                                classify_opaal_tokens(&input, &lex_opaal(&input)).unwrap(),
                                SyntaxClassification::Invalid(diagnostics[0].clone())
                            );
                            assert_eq!(
                                format_source_opaal(&input),
                                FormatOutcome::Invalid(diagnostics)
                            );
                        }
                        outcome => panic!("unexpected outcome {outcome:?}"),
                    }
                }
            }
            for depth in [32, 33, 2000] {
                let input = source(format!(
                    "{}if true {{ let x = rows[0:1] }}",
                    "if false {} else ".repeat(depth - 2)
                ));
                assert_eq!(
                    matches!(parse_opaal(&input), ParseOutcome::Complete(_)),
                    depth == 32
                );
                assert_eq!(
                    matches!(format_source_opaal(&input), FormatOutcome::Complete(_)),
                    depth == 32
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn receiver_payloads_are_bounded_before_a_late_slice_is_parsed() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            for depth in [32, 33, 2000] {
                let opens = "(".repeat(depth - 2);
                let closes = ")".repeat(depth - 2);
                let if_opens = "(".repeat(depth - 3);
                let if_closes = ")".repeat(depth - 3);
                for text in [
                    format!("let x = {opens}[0]{closes}[0:1]"),
                    format!("let x = f({opens}0{closes})[0:1]"),
                    format!("let x = rows[{opens}0{closes}][0:1]"),
                    format!("let x = if true {{ {if_opens}[0]{if_closes} }} else {{ [0] }}[0:1]"),
                ] {
                    let input = source(text);
                    if depth == 32 {
                        parse(&input);
                    } else {
                        let ParseOutcome::Invalid(diagnostics) = parse_opaal(&input) else {
                            panic!("expected a depth refusal at {depth}");
                        };
                        assert_eq!(diagnostics[0].code(), "OP1000");
                        assert!(
                            diagnostics[0]
                                .message()
                                .contains("a path containing a slice")
                        );
                    }
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
