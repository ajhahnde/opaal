#![forbid(unsafe_code)]

use opaal_syntax::{
    FormatOutcome, SourceFile, SourceId, StatementKind, TokenKind, VersionedParseOutcome,
    format_source_opaal, lex_opaal, parse_opaal,
};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(91), "documentation.opaal", text)
}

#[test]
fn documentation_comments_attach_by_physical_adjacency_and_preserve_exact_spans() {
    let text = concat!(
        "language 1\n",
        "## Summary π\n",
        "##\n",
        "## Detail 🚀\n",
        "def documented(value: String) -> String {\n",
        "    ## Nested summary\n",
        "    def nested() { null }\n",
        "    $value\n",
        "}\n",
        "\n",
        "## detached\n",
        "\n",
        "def detached() { null }\n",
        "# ordinary\n",
        "def ordinary() { null }\n",
        "## before binding\n",
        "let value = 1\n",
        "def after_binding() { null }\n",
        "def inline() { null } ## inline\n",
    );
    let file = source(text);
    let tokens = lex_opaal(&file);
    assert_eq!(
        tokens
            .iter()
            .find(|token| token.text(&file).ok() == Some("## Summary π"))
            .unwrap()
            .kind(),
        TokenKind::DocumentationComment,
    );
    assert_eq!(
        tokens
            .iter()
            .find(|token| token.text(&file).ok() == Some("## inline"))
            .unwrap()
            .kind(),
        TokenKind::Comment,
    );
    let script = match parse_opaal(&file) {
        VersionedParseOutcome::Complete(versioned) => versioned.into_script(),
        other => panic!("documentation source did not parse: {other:?}"),
    };

    let StatementKind::Function(documented) = script.statements()[0].kind() else {
        panic!("expected documented function");
    };
    let documentation = documented
        .documentation
        .as_ref()
        .expect("the adjacent block attaches");
    assert_eq!(documentation.lines.len(), 3);
    assert_eq!(
        documentation
            .lines
            .iter()
            .map(|span| file.slice(*span).expect("documentation span"))
            .collect::<Vec<_>>(),
        vec!["## Summary π", "##", "## Detail 🚀"],
    );
    assert_eq!(
        file.slice(script.statements()[0].span()).unwrap(),
        &text[text.find("def documented").unwrap()..text.find("\n\n## detached").unwrap()],
        "the executable node span still starts at `def`",
    );

    let StatementKind::Function(nested) = documented.body.statements[0].kind() else {
        panic!("expected nested function");
    };
    assert_eq!(
        nested
            .documentation
            .as_ref()
            .expect("nested documentation attaches")
            .lines
            .iter()
            .map(|span| file.slice(*span).unwrap())
            .collect::<Vec<_>>(),
        vec!["## Nested summary"],
    );

    for statement in &script.statements()[1..] {
        if let StatementKind::Function(function) = statement.kind() {
            assert!(
                function.documentation.is_none(),
                "detached, ordinary, pre-nonfunction, and inline comments stay inert"
            );
        }
    }

    let formatted = match format_source_opaal(&file) {
        FormatOutcome::Complete(formatted) => formatted,
        other => panic!("documentation source did not format: {other:?}"),
    };
    assert_eq!(
        format_source_opaal(&source(&formatted)),
        FormatOutcome::Complete(formatted),
        "documentation formatting is idempotent",
    );
}
