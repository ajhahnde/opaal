use opaal_syntax::{
    CompletionContext, ExpressionKind, FormatOutcome, ParseOutcome, SourceFile, SourceId,
    StatementKind, SyntaxClassification, classify_opaal_tokens, completion_target,
    format_source_opaal, lex_opaal, parse_opaal,
};

fn source(text: impl Into<String>) -> SourceFile {
    SourceFile::new(SourceId::new(1), "comprehensions.opaal", text.into())
}

#[test]
fn clauses_keep_original_spans_and_format_idempotently() {
    let text =
        "let rows = [if n > 1 { n } else { 0 }\n# generator\nfor i,(n,_) in [(2,3)] if n in [1,2]]";
    let input = source(text);
    let ParseOutcome::Complete(script) = parse_opaal(&input) else {
        panic!("{:?}", parse_opaal(&input));
    };
    let StatementKind::Declaration(declaration) = script.statements()[0].kind() else {
        panic!("expected let");
    };
    let ExpressionKind::ListComprehension(node) = declaration.value.kind() else {
        panic!("expected comprehension");
    };
    for (span, expected) in [
        (node.open, "["),
        (node.for_span, "for"),
        (node.index.unwrap().span(), "i"),
        (node.in_span, "in"),
        (node.filter.as_ref().unwrap().0, "if"),
        (node.close, "]"),
    ] {
        assert_eq!(input.slice(span).unwrap(), expected);
    }
    let FormatOutcome::Complete(formatted) = format_source_opaal(&input) else {
        panic!("expected formatted source");
    };
    assert!(formatted.contains("# generator\n"), "{formatted}");
    assert!(formatted.contains("for i, (n, _) in "), "{formatted}");
    assert_eq!(
        format_source_opaal(&source(&formatted)),
        FormatOutcome::Complete(formatted)
    );
    assert_eq!(
        format_source_opaal(&source("let rows = [n*2 for i,n in 1..=3 if n>1][0]")),
        FormatOutcome::Complete("let rows = [n*2 for i, n in 1..=3 if n>1][0]\n".into()),
    );
}

#[test]
fn present_conflicts_are_invalid_and_missing_clauses_are_incomplete() {
    for text in [
        "let x = [n for",
        "let x = [n for n",
        "let x = [n for n in",
        "let x = [n for n in [] if",
        "let x = [n for n in [] if true",
    ] {
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
    }
    for (text, bad) in [
        ("let x = [n for n in [] if true && false]", "&&"),
        ("let x = [n for n in [] if true || false", "||"),
        ("let x = [n for n in [] if true | false]", "|"),
        ("let x = [n for n in [] for n in []]", "for"),
        ("let x = [n for n in [] if true if false]", "if"),
        ("let x = [n for n in [],]", ","),
        ("let x = [...n for n in []]", "for"),
        ("let x = [n for n of []]", "of"),
    ] {
        let input = source(text);
        let ParseOutcome::Invalid(diagnostics) = parse_opaal(&input) else {
            panic!("{text}: {:?}", parse_opaal(&input));
        };
        assert_eq!(input.slice(diagnostics[0].labels()[0].span()).unwrap(), bad);
        assert!(matches!(
            format_source_opaal(&input),
            FormatOutcome::Invalid(_)
        ));
    }
}

#[test]
fn the_three_operands_use_expression_completion() {
    let text = "let rows = [projection for i, item in iterable if filter]";
    for spelling in ["projection", "iterable", "filter"] {
        let cursor = text.find(spelling).unwrap() + 2;
        assert_eq!(
            completion_target(text, cursor).unwrap().context(),
            &CompletionContext::Expression
        );
    }
    let both = "let rows = [if ^true for item in rows && true { 1 } else { 0 } for item in rows]";
    let command_word = both.find("for").unwrap() + 2;
    let iterable = both.rfind("rows").unwrap() + 2;
    assert_ne!(
        completion_target(both, command_word).unwrap().context(),
        &CompletionContext::Expression
    );
    assert_eq!(
        completion_target(both, iterable).unwrap().context(),
        &CompletionContext::Expression
    );
    let nested = r#"let rows = [if ^true "{[inner for inner in [1]]}" && true { 1 } else { 0 } for n in [1]]"#;
    let nested_projection = nested.find("inner").unwrap() + 2;
    assert_eq!(
        completion_target(nested, nested_projection)
            .unwrap()
            .context(),
        &CompletionContext::Expression
    );
}

#[test]
fn command_for_words_do_not_turn_legacy_lists_into_comprehensions() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let arm = format!("{}1{}", "[".repeat(33), "]".repeat(33));
            for condition in [
                "^true for && true",
                "^true for item in rows && true",
                "true { 0 } else if true | echo for item in rows && true",
                r#"^true "quoted {1}" for item in rows && true"#,
            ] {
                let text = format!("let rows = [if {condition} {{ {arm} }} else {{ {arm} }}]");
                let input = source(&text);
                assert!(
                    matches!(parse_opaal(&input), ParseOutcome::Complete(_)),
                    "{text}"
                );
                assert_eq!(
                    classify_opaal_tokens(&input, &lex_opaal(&input)).unwrap(),
                    SyntaxClassification::Complete,
                );
                let FormatOutcome::Complete(formatted) = format_source_opaal(&input) else {
                    panic!("{text}");
                };
                assert!(matches!(
                    parse_opaal(&source(&formatted)),
                    ParseOutcome::Complete(_)
                ));
                let cursor = text.find("for").unwrap() + 2;
                assert_ne!(
                    completion_target(&text, cursor).unwrap().context(),
                    &CompletionContext::Expression,
                );
            }
            let partial = source("let rows = [if ^true for && true { 1 }");
            assert!(matches!(parse_opaal(&partial), ParseOutcome::Incomplete(_)));
            assert!(matches!(
                classify_opaal_tokens(&partial, &lex_opaal(&partial)).unwrap(),
                SyntaxClassification::Incomplete(_)
            ));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn mixed_depth_is_bounded_before_recursive_source_observers() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            for depth in [31, 32, 33, 2000] {
                let conditions = (depth - 1).min(32);
                let enclosing = depth - conditions - 1;
                for body in [
                    format!(
                        "{}[i for i in []]{}",
                        "(".repeat(depth - 1),
                        ")".repeat(depth - 1)
                    ),
                    format!(
                        "[{}i{} for i in []]",
                        "[".repeat(depth - 1),
                        "]".repeat(depth - 1)
                    ),
                    format!("[{}i for i in []]", "!".repeat(depth - 1)),
                    format!("[{}i for i in []]", "1 + ".repeat(depth - 1)),
                    format!("[i for i in []]{}", "[0]".repeat(depth - 1)),
                    format!(
                        "{}[i for i in []]{}",
                        "{field: ".repeat(depth - 1),
                        "}".repeat(depth - 1)
                    ),
                    format!(
                        "[{}i{} for i in []]",
                        "if true { ".repeat(depth - 1),
                        " } else { 0 }".repeat(depth - 1)
                    ),
                    format!(
                        "[if ^true for item in rows && true {{ {}1{} }} else {{ 0 }} for i in []]",
                        "[".repeat(depth - 2),
                        "]".repeat(depth - 2)
                    ),
                    format!(
                        "[{}{}true{}{} for i in []]",
                        "[".repeat(enclosing),
                        "if ".repeat(conditions),
                        " { true } else { false }".repeat(conditions),
                        "]".repeat(enclosing)
                    ),
                    format!(
                        "[(^true {{ {}1{} }}) for i in []]",
                        "[".repeat(depth - 2),
                        "]".repeat(depth - 2)
                    ),
                    format!(
                        "{}[i for i in []]{}",
                        "f(".repeat(depth - 1),
                        ")".repeat(depth - 1)
                    ),
                    format!(
                        "[{}[i for i in []]{} for i in []]",
                        "[".repeat(depth - 2),
                        "]".repeat(depth - 2)
                    ),
                ] {
                    let input = source(format!("let rows = {body}"));
                    assert_eq!(
                        matches!(parse_opaal(&input), ParseOutcome::Complete(_)),
                        depth <= 32,
                        "{depth}"
                    );
                    assert_eq!(
                        matches!(
                            classify_opaal_tokens(&input, &lex_opaal(&input)).unwrap(),
                            SyntaxClassification::Complete
                        ),
                        depth <= 32,
                        "{depth}"
                    );
                    assert_eq!(
                        matches!(format_source_opaal(&input), FormatOutcome::Complete(_)),
                        depth <= 32,
                        "{depth}"
                    );
                }
                for text in [
                    format!(
                        "{}let rows = [i for i in []]{}",
                        "if true {\n".repeat(depth - 1),
                        "\n}".repeat(depth - 1)
                    ),
                    format!(
                        "{}if true {{ let rows = [i for i in []] }}",
                        "if false {} else ".repeat(depth - 2)
                    ),
                ] {
                    let input = source(text);
                    let parsed = parse_opaal(&input);
                    assert_eq!(
                        matches!(parsed, ParseOutcome::Complete(_)),
                        depth <= 32,
                        "{depth}"
                    );
                    assert_eq!(
                        matches!(format_source_opaal(&input), FormatOutcome::Complete(_)),
                        depth <= 32,
                        "{depth}"
                    );
                }
            }
            let input = source(format!(
                "let rows = [{}true{} for _item in []]",
                "if (".repeat(30),
                ") { true } else { false }".repeat(30)
            ));
            assert!(matches!(parse_opaal(&input), ParseOutcome::Invalid(_)));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn long_unary_chains_report_the_first_excessive_operator() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            for (prefix, operand, suffix, valid, excessive) in [
                ("let values = [", "true", " for item in [0]]", 31, 31),
                ("let values = [f((", "true", ")) for item in [0]]", 29, 29),
                (
                    "let values = [true == ",
                    "false",
                    " for item in [0]]",
                    30,
                    30,
                ),
                ("let values = (", "[true for item in []]", ")", 30, 31),
            ] {
                let input = source(format!("{prefix}{}{operand}{suffix}", "!".repeat(valid)));
                let parsed = parse_opaal(&input);
                assert!(
                    matches!(parsed, ParseOutcome::Complete(_)),
                    "{prefix}: {parsed:?}"
                );
                for count in [excessive + 1, 33, 40, 2000] {
                    let input = source(format!("{prefix}{}{operand}{suffix}", "!".repeat(count)));
                    let ParseOutcome::Invalid(diagnostics) = parse_opaal(&input) else {
                        panic!("expected depth refusal at {count} unary operators");
                    };
                    let diagnostic = &diagnostics[0];
                    let start = prefix.len() + excessive;
                    assert_eq!(diagnostic.code(), "OP1000");
                    assert_eq!(
                        diagnostic.message(),
                        "a path containing a list comprehension can nest at most 32 levels"
                    );
                    assert_eq!(
                        diagnostic.labels()[0].span(),
                        input.span(start..start + 1).unwrap()
                    );
                    assert_eq!(
                        classify_opaal_tokens(&input, &lex_opaal(&input)).unwrap(),
                        SyntaxClassification::Invalid(diagnostic.clone())
                    );
                    assert_eq!(
                        format_source_opaal(&input),
                        FormatOutcome::Invalid(diagnostics)
                    );
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
