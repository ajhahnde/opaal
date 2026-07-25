#![forbid(unsafe_code)]

use std::fs;
use std::path::Path;

use flashshell_syntax::{
    LabelStyle, SourceFile, SourceId, SyntaxClassification, classify_tokens, lex,
};

#[test]
fn lexical_manifest_classes_and_reasons_are_reported_directly() {
    let root = workspace_root().join("tests/golden/lexical");
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = row.split('\t').collect();
        assert_eq!(fields.len(), 3, "malformed manifest row: {row}");
        let source = SourceFile::new(
            SourceId::new(index as u32),
            fields[1],
            fs::read_to_string(root.join(fields[1])).unwrap(),
        );
        assert_classification(&source, fields[0], fields[2]);
    }
}

#[test]
fn grammar_complete_and_incomplete_rows_have_structural_classification() {
    let root = workspace_root().join("tests/golden/grammar");
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = row.split('\t').collect();
        assert_eq!(fields.len(), 4, "malformed manifest row: {row}");
        if fields[0] == "invalid" {
            continue;
        }
        let source = SourceFile::new(
            SourceId::new(100 + index as u32),
            fields[2],
            fs::read_to_string(root.join(fields[2])).unwrap(),
        );
        assert_classification(&source, fields[0], fields[3]);
    }
}

#[test]
fn mismatched_and_stray_closing_delimiters_are_invalid_not_incomplete() {
    for (text, offending) in [("echo ([)]", ")"), ("echo }", "}")] {
        let source = SourceFile::new(SourceId::new(500), "delimiter.fsh", text);
        let tokens = lex(&source);
        let SyntaxClassification::Invalid(diagnostic) = classify_tokens(&source, &tokens).unwrap()
        else {
            panic!("expected invalid classification for {text:?}");
        };

        assert_eq!(diagnostic.code(), "FS0002");
        assert_eq!(diagnostic.message(), "unexpected closing delimiter");
        assert_eq!(diagnostic.labels().len(), 1);
        assert_eq!(diagnostic.labels()[0].style(), LabelStyle::Primary);
        assert_eq!(
            source.slice(diagnostic.labels()[0].span()).unwrap(),
            offending
        );
    }
}

#[test]
fn invalid_source_characters_keep_an_exact_diagnostic_span_inside_word_parts() {
    for (text, offending) in [
        ("echo 'before\0after'", "\0"),
        ("# before\0after\n", "\0"),
        ("echo \\\0", "\0"),
        ("echo 'before\rafter'", "\r"),
    ] {
        let source = SourceFile::new(SourceId::new(600), "invalid-source.fsh", text);
        let tokens = lex(&source);
        let SyntaxClassification::Invalid(diagnostic) = classify_tokens(&source, &tokens).unwrap()
        else {
            panic!("expected invalid classification for {text:?}");
        };

        assert_eq!(diagnostic.code(), "FS0001");
        assert_eq!(
            source.slice(diagnostic.labels()[0].span()).unwrap(),
            offending
        );
    }
}

fn assert_classification(source: &SourceFile, expected_class: &str, reason: &str) {
    let tokens = lex(source);
    let classification = classify_tokens(source, &tokens).unwrap();
    match (expected_class, classification) {
        ("complete", SyntaxClassification::Complete) => {}
        ("incomplete", SyntaxClassification::Incomplete(incomplete)) => {
            assert_eq!(incomplete.reason(), reason, "{}", source.name());
            source.slice(incomplete.span()).unwrap();
        }
        ("invalid", SyntaxClassification::Invalid(diagnostic)) => {
            assert_eq!(diagnostic.code(), "FS0001", "{}", source.name());
            assert_eq!(diagnostic.message(), reason, "{}", source.name());
            assert_eq!(diagnostic.labels()[0].style(), LabelStyle::Primary);
            source.slice(diagnostic.labels()[0].span()).unwrap();
        }
        (expected, actual) => panic!(
            "{}: expected {expected} ({reason}), got {actual:?}",
            source.name()
        ),
    }
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
}
