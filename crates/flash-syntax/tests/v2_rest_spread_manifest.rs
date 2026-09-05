#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use flash_syntax::{
    CommandItemKind, FormatOutcome, Pattern, SourceFile, SourceId, StageKind, StatementKind,
    VersionedParseOutcome, format_source_v2, parse_v2,
};

#[test]
fn rest_spread_corpus_parses_and_formats_idempotently() {
    let root = fixture_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "malformed rest/spread row {}", index + 1);
        let source = fixture(fields[1], 1_000 + index as u32);
        assert!(
            matches!(parse_v2(&source), VersionedParseOutcome::Complete(_)),
            "{} must parse through the canonical v2 AST",
            fields[1]
        );
        let FormatOutcome::Complete(formatted) = format_source_v2(&source) else {
            panic!("{} must format", fields[1]);
        };
        let reparsed = SourceFile::new(
            SourceId::new(1_100 + index as u32),
            fields[1],
            formatted.clone(),
        );
        assert_eq!(
            format_source_v2(&reparsed),
            FormatOutcome::Complete(formatted),
            "{} must format idempotently",
            fields[1]
        );
    }
}

#[test]
fn build_fixture_keeps_rest_and_spread_as_dedicated_nodes() {
    let build = fixture("complete/build-arguments.fsh", 1_200);
    let VersionedParseOutcome::Complete(parsed) = parse_v2(&build) else {
        panic!("the build-argument fixture must parse");
    };
    let StatementKind::Function(function) = parsed.script().statements()[0].kind() else {
        panic!("the build-argument fixture must begin with a function");
    };
    let StatementKind::Match(statement) = function.body.statements[0].kind() else {
        panic!("the build-argument function must match its input");
    };
    let Pattern::List(nonempty) = &statement.arms[1].pattern else {
        panic!("the nonempty arm must retain a list pattern");
    };
    assert_eq!(nonempty.elements.len(), 1);
    assert!(nonempty.rest.is_some());

    let spread = fixture("complete/explicit-spread.fsh", 1_201);
    let VersionedParseOutcome::Complete(parsed) = parse_v2(&spread) else {
        panic!("the explicit-spread fixture must parse");
    };
    let StatementKind::Job(job) = parsed.script().statements()[1].kind() else {
        panic!("the final statement must be a command job");
    };
    let StageKind::Command(command) = job.chain.or_terms()[0].and_terms()[0].stages()[0].kind()
    else {
        panic!("the final stage must remain a command");
    };
    assert!(matches!(
        command.items[0].kind(),
        CommandItemKind::Spread(_)
    ));
}

fn fixture(relative: &str, id: u32) -> SourceFile {
    SourceFile::new(
        SourceId::new(id),
        relative,
        fs::read_to_string(fixture_root().join(relative)).unwrap(),
    )
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language/rest-spread")
}
