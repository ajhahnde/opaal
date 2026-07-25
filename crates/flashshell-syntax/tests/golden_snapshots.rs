#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use flashshell_syntax::{ParseOutcome, SourceFile, SourceId, parse, render_diagnostic};

const UPDATE_ENV: &str = "FLASHSHELL_UPDATE_GOLDENS";

#[test]
fn ast_manifest_covers_every_complete_grammar_fixture() {
    let grammar_root = workspace_root().join("tests/golden/grammar");
    let golden_root = workspace_root().join("tests/golden/ast");
    let grammar_rows = manifest_rows(&grammar_root.join("manifest.tsv"), 4);
    let ast_rows = manifest_rows(&golden_root.join("manifest.tsv"), 2);

    let expected: BTreeSet<_> = grammar_rows
        .iter()
        .filter(|fields| fields[0] == "complete")
        .map(|fields| fields[2].clone())
        .collect();
    let actual: BTreeSet<_> = ast_rows.iter().map(|fields| fields[0].clone()).collect();
    assert_eq!(actual.len(), ast_rows.len(), "duplicate AST golden source");
    assert_eq!(
        ast_rows
            .iter()
            .map(|fields| &fields[1])
            .collect::<BTreeSet<_>>()
            .len(),
        ast_rows.len(),
        "duplicate AST golden output"
    );
    assert_eq!(actual, expected, "AST golden manifest coverage changed");

    for (index, fields) in ast_rows.iter().enumerate() {
        let text = fs::read_to_string(grammar_root.join(&fields[0])).unwrap();
        let source = SourceFile::new(SourceId::new(4_000 + index as u32), &fields[0], text);
        let ParseOutcome::Complete(script) = parse(&source) else {
            panic!("AST golden source did not parse: {}", fields[0]);
        };

        assert_or_update(&golden_root.join(&fields[1]), &ast_golden(&script));
    }
}

#[test]
fn diagnostic_manifest_covers_every_invalid_normative_fixture() {
    let tests_root = workspace_root().join("tests/golden");
    let golden_root = tests_root.join("diagnostics");
    let diagnostic_rows = manifest_rows(&golden_root.join("manifest.tsv"), 3);

    let mut expected = BTreeSet::new();
    for corpus in ["grammar", "lexical"] {
        for fields in manifest_rows(
            &tests_root.join(corpus).join("manifest.tsv"),
            if corpus == "grammar" { 4 } else { 3 },
        ) {
            if fields[0] == "invalid" {
                expected.insert((
                    corpus.to_owned(),
                    fields[if corpus == "grammar" { 2 } else { 1 }].clone(),
                ));
            }
        }
    }
    let actual: BTreeSet<_> = diagnostic_rows
        .iter()
        .map(|fields| (fields[0].clone(), fields[1].clone()))
        .collect();
    assert_eq!(
        actual.len(),
        diagnostic_rows.len(),
        "duplicate diagnostic golden source"
    );
    assert_eq!(
        diagnostic_rows
            .iter()
            .map(|fields| &fields[2])
            .collect::<BTreeSet<_>>()
            .len(),
        diagnostic_rows.len(),
        "duplicate diagnostic golden output"
    );
    assert_eq!(
        actual, expected,
        "diagnostic golden manifest coverage changed"
    );

    for (index, fields) in diagnostic_rows.iter().enumerate() {
        let source_path = tests_root.join(&fields[0]).join(&fields[1]);
        let text = fs::read_to_string(&source_path).unwrap();
        let source = SourceFile::new(
            SourceId::new(5_000 + index as u32),
            format!("{}/{}", fields[0], fields[1]),
            text,
        );
        let ParseOutcome::Invalid(diagnostics) = parse(&source) else {
            panic!("diagnostic golden source was not invalid: {source_path:?}");
        };
        let rendered = diagnostics
            .iter()
            .map(|diagnostic| render_diagnostic(&source, diagnostic).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        assert_or_update(&golden_root.join(&fields[2]), &rendered);
    }
}

fn manifest_rows(path: &Path, field_count: usize) -> Vec<Vec<String>> {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
        .lines()
        .filter(|row| !row.is_empty() && !row.starts_with('#'))
        .map(|row| {
            let fields: Vec<_> = row.split('\t').map(str::to_owned).collect();
            assert_eq!(fields.len(), field_count, "malformed manifest row: {row}");
            fields
        })
        .collect()
}

fn assert_or_update(path: &Path, actual: &str) {
    if env::var_os(UPDATE_ENV).as_deref() == Some(OsStr::new("1")) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, actual).unwrap();
        return;
    }

    let expected = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    assert_eq!(
        actual,
        expected,
        "golden output changed: {}",
        path.display()
    );
}

fn ast_golden(script: &flashshell_syntax::Script) -> String {
    let mut output = format!("{script:#?}");
    while let Some(start) = output.find("Span {") {
        let end = output[start..].find('}').unwrap() + start + 1;
        let numbers: Vec<_> = output[start..end]
            .split(|character: char| !character.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .collect();
        assert_eq!(numbers.len(), 3, "unexpected Span debug representation");
        output.replace_range(start..end, &format!("Span({}..{})", numbers[1], numbers[2]));
    }
    output.push('\n');
    output
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}
