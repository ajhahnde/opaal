#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use flash_syntax::{
    ExpressionKind, Pattern, SourceFile, SourceId, StatementKind, VersionedParseOutcome, parse_v2,
};

#[test]
fn complete_v2_type_corpus_has_one_shared_ast() {
    let root = type_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "malformed type row {}", index + 1);
        if fields[0] != "complete" {
            continue;
        }
        let source = fixture(fields[1], 700 + index as u32);
        assert!(
            matches!(parse_v2(&source), VersionedParseOutcome::Complete(_)),
            "{} must parse through the canonical v2 AST",
            fields[1]
        );
    }
}

#[test]
fn domain_fixture_retains_generic_variant_pattern_and_explicit_call_shapes() {
    let source = fixture("complete/domain-types.fsh", 750);
    let VersionedParseOutcome::Complete(parsed) = parse_v2(&source) else {
        panic!("the domain fixture must parse");
    };
    let statements = parsed.script().statements();

    let StatementKind::NominalType(record) = statements[0].kind() else {
        panic!("the first declaration must be a nominal record");
    };
    assert_eq!(record.type_parameters.len(), 1);
    assert_eq!(text(&source, record.type_parameters[0].name.span()), "T");
    assert_eq!(record.type_parameters[0].constraints.len(), 1);

    let StatementKind::VariantType(variant) = statements[1].kind() else {
        panic!("the second declaration must be a nominal variant");
    };
    assert_eq!(variant.variants.len(), 2);
    assert_eq!(text(&source, variant.variants[0].name.span()), "Selected");
    assert_eq!(variant.variants[0].payload.len(), 1);

    let StatementKind::Function(function) = statements[2].kind() else {
        panic!("the third declaration must be a generic function");
    };
    assert_eq!(function.type_parameters.len(), 1);
    assert!(matches!(
        function.parameters[0].pattern,
        Pattern::NominalRecord(_)
    ));

    let StatementKind::Declaration(selection) = statements[5].kind() else {
        panic!("the selection constructor must be retained as a declaration");
    };
    let ExpressionKind::Call(variant_call) = selection.value.kind() else {
        panic!("the variant constructor must be an ordinary typed call");
    };
    let ExpressionKind::Qualified(_) = variant_call.callee.kind() else {
        panic!("the variant constructor must retain its qualified identity");
    };
    let ExpressionKind::Call(generic_call) = variant_call.arguments[0].kind() else {
        panic!("the selected payload must be the explicit generic call");
    };
    assert_eq!(generic_call.type_arguments.len(), 1);

    let StatementKind::Match(selection_match) = statements[6].kind() else {
        panic!("the final statement must be the exhaustive match");
    };
    assert!(matches!(
        selection_match.arms[0].pattern,
        Pattern::Variant(_)
    ));
    assert!(selection_match.arms[0].guard.is_some());
}

#[test]
fn closure_result_and_destructuring_forms_have_dedicated_nodes() {
    let closure_source = fixture("complete/closure-result.fsh", 760);
    let VersionedParseOutcome::Complete(closure_parse) = parse_v2(&closure_source) else {
        panic!("the closure fixture must parse");
    };
    let StatementKind::Declaration(declaration) = closure_parse.script().statements()[0].kind()
    else {
        panic!("the closure must be bound");
    };
    let ExpressionKind::Closure(closure) = declaration.value.kind() else {
        panic!("the declaration value must be a closure");
    };
    assert_eq!(
        text(&closure_source, closure.result_type.as_ref().unwrap().span),
        "Int"
    );

    let pattern_source = fixture("complete/list-and-record-patterns.fsh", 761);
    let VersionedParseOutcome::Complete(pattern_parse) = parse_v2(&pattern_source) else {
        panic!("the destructuring fixture must parse");
    };
    let StatementKind::Declaration(record) = pattern_parse.script().statements()[1].kind() else {
        panic!("the record destructuring must be a declaration");
    };
    assert!(matches!(record.pattern, Pattern::NominalRecord(_)));
    let StatementKind::Declaration(list) = pattern_parse.script().statements()[2].kind() else {
        panic!("the list destructuring must be a declaration");
    };
    let Pattern::List(list) = &list.pattern else {
        panic!("the list destructuring must retain a list pattern");
    };
    assert!(list.rest.is_some());
}

#[test]
fn name_followed_by_brackets_remains_an_index_without_a_call() {
    let source = SourceFile::new(
        SourceId::new(762),
        "index.fsh",
        "language 2\nlet value = name[0]\n",
    );
    let VersionedParseOutcome::Complete(parsed) = parse_v2(&source) else {
        panic!("a name followed by an index must parse as an index expression");
    };
    let StatementKind::Declaration(declaration) = parsed.script().statements()[0].kind() else {
        panic!("the index expression must be retained as a declaration value");
    };
    assert!(matches!(declaration.value.kind(), ExpressionKind::Index(_)));
}

fn fixture(relative: &str, id: u32) -> SourceFile {
    SourceFile::new(
        SourceId::new(id),
        relative,
        fs::read_to_string(type_root().join(relative)).unwrap(),
    )
}

fn text(source: &SourceFile, span: flash_syntax::Span) -> &str {
    source.slice(span).unwrap()
}

fn type_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language/types")
}
