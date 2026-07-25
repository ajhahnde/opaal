#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use flashshell_syntax::{
    FormatOutcome, ParseOutcome, SourceFile, SourceId, TokenKind, format_source, lex, parse,
};

#[test]
fn complete_grammar_fixtures_format_idempotently_without_structural_changes() {
    let root = workspace_root().join("tests/golden/grammar");
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = row.split('\t').collect();
        if fields[0] != "complete" {
            continue;
        }

        let text = fs::read_to_string(root.join(fields[2])).unwrap();
        let source = SourceFile::new(SourceId::new(1_000 + index as u32), fields[2], text);
        let ParseOutcome::Complete(original) = parse(&source) else {
            panic!("fixture did not parse before formatting: {}", fields[2]);
        };

        let FormatOutcome::Complete(formatted) = format_source(&source) else {
            panic!("complete fixture did not format: {}", fields[2]);
        };
        let formatted_source = SourceFile::new(
            SourceId::new(2_000 + index as u32),
            format!("formatted/{}", fields[2]),
            formatted.clone(),
        );
        let ParseOutcome::Complete(reparsed) = parse(&formatted_source) else {
            panic!(
                "formatted fixture did not reparse: {}\n{formatted}",
                fields[2]
            );
        };

        assert_eq!(
            ast_shape(&original),
            ast_shape(&reparsed),
            "formatted AST changed structure for {}",
            fields[2]
        );
        assert_eq!(
            significant_tokens(&source),
            significant_tokens(&formatted_source),
            "formatter changed a comment, literal, word part, or operator in {}",
            fields[2]
        );
        assert_eq!(
            complete_format(&formatted_source),
            formatted,
            "formatter was not idempotent for {}",
            fields[2]
        );
    }
}

#[test]
fn horizontal_trivia_and_block_indentation_are_canonical() {
    let source = SourceFile::new(
        SourceId::new(3_000),
        "spacing.fsh",
        "def demo(name: string) {\n\techo   \"$name\"   >   output # kept\n}\n",
    );

    assert_eq!(
        complete_format(&source),
        "def demo(name: string) {\n    echo \"$name\" > output # kept\n}\n"
    );
}

#[test]
fn incomplete_and_invalid_inputs_keep_their_parse_outcomes() {
    let incomplete = SourceFile::new(SourceId::new(3_001), "incomplete.fsh", "echo \"");
    let FormatOutcome::Incomplete(reason) = format_source(&incomplete) else {
        panic!("expected incomplete format outcome");
    };
    assert_eq!(reason.reason(), "unmatched double quote");

    let invalid = SourceFile::new(SourceId::new(3_002), "invalid.fsh", "| broken\n");
    let FormatOutcome::Invalid(diagnostics) = format_source(&invalid) else {
        panic!("expected invalid format outcome");
    };
    assert_eq!(
        diagnostics[0].message(),
        "pipeline operator cannot begin a stage"
    );
}

fn complete_format(source: &SourceFile) -> String {
    let FormatOutcome::Complete(formatted) = format_source(source) else {
        panic!("expected complete format outcome");
    };
    formatted
}

fn significant_tokens(source: &SourceFile) -> Vec<(TokenKind, String)> {
    lex(source)
        .into_iter()
        .filter(|token| !matches!(token.kind(), TokenKind::Whitespace | TokenKind::Newline))
        .map(|token| (token.kind(), token.text(source).unwrap().to_owned()))
        .collect()
}

fn ast_shape(script: &flashshell_syntax::Script) -> String {
    let mut shape = format!("{script:#?}");
    while let Some(start) = shape.find("Span {") {
        let end = shape[start..].find('}').unwrap() + start + 1;
        shape.replace_range(start..end, "Span");
    }
    shape
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}
