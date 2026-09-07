#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use opaal_syntax::{
    ExpressionKind, FormatOutcome, ParseOutcome, SourceFile, SourceId, StatementKind,
    format_source_opaal, parse_opaal,
};

#[test]
fn outcome_corpus_parses_and_formats_idempotently() {
    let root = fixture_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "malformed outcome row {}", index + 1);
        let source = fixture(fields[1], 1_300 + index as u32);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Complete(_)),
            "{} must parse through the canonical opaal AST",
            fields[1]
        );
        let FormatOutcome::Complete(formatted) = format_source_opaal(&source) else {
            panic!("{} must format", fields[1]);
        };
        let reparsed = SourceFile::new(
            SourceId::new(1_400 + index as u32),
            fields[1],
            formatted.clone(),
        );
        assert_eq!(
            format_source_opaal(&reparsed),
            FormatOutcome::Complete(formatted),
            "{} must format idempotently",
            fields[1]
        );
    }
}

#[test]
fn domain_error_fixture_retains_a_qualified_generic_constructor() {
    let source = fixture("complete/domain-error.opaal", 1_500);
    let ParseOutcome::Complete(parsed) = parse_opaal(&source) else {
        panic!("the domain-error fixture must parse");
    };
    let StatementKind::Job(job) = parsed.statements().last().unwrap().kind() else {
        panic!("the final outcome expression must remain a job statement");
    };
    let opaal_syntax::StageKind::Expression(expression) =
        job.chain.or_terms()[0].and_terms()[0].stages()[0].kind()
    else {
        panic!("the final stage must remain an expression");
    };
    let ExpressionKind::Call(call) = expression.kind() else {
        panic!("the final outcome expression must remain a call");
    };
    let ExpressionKind::Qualified(name) = call.callee.kind() else {
        panic!("the Result constructor must remain qualified");
    };
    assert_eq!(name.segments.len(), 3);
    assert_eq!(call.type_arguments.len(), 2);
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
        .join("tests/opaal-foundation/language/outcomes")
}
