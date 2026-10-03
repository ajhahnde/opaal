#![forbid(unsafe_code)]
#[path = "support/numeric.rs"]
mod support;
use opaal_runtime::help::{ModuleHelpCatalog, render_module_operation_help};
use opaal_runtime::operation::OperationPurity;
use support::{load, try_load};

#[test]
fn checker_help_and_catalog_share_exact_numeric_signatures_without_execution() {
    let program = load("def fail() -> Float { return math::sqrt(-1.0) }");
    let catalog = ModuleHelpCatalog::snapshot(&program);
    let expected = [
        (
            "abs",
            vec!["(value: Int) -> Int", "(value: Float) -> Float"],
        ),
        (
            "min",
            vec![
                "(left: Int, right: Int) -> Int",
                "(left: Float, right: Float) -> Float",
            ],
        ),
        (
            "max",
            vec![
                "(left: Int, right: Int) -> Int",
                "(left: Float, right: Float) -> Float",
            ],
        ),
        (
            "clamp",
            vec![
                "(value: Int, min: Int, max: Int) -> Int",
                "(value: Float, min: Float, max: Float) -> Float",
            ],
        ),
        ("floor", vec!["(value: Float) -> Float"]),
        ("ceil", vec!["(value: Float) -> Float"]),
        ("round", vec!["(value: Float) -> Float"]),
        ("sqrt", vec!["(value: Float) -> Float"]),
    ];
    for (name, signatures) in expected {
        let entry = catalog
            .query(program.graph().root(), &format!("math::{name}"))
            .unwrap();
        let descriptor = entry.operation().unwrap();
        descriptor.validate().unwrap();
        assert_eq!(descriptor.purity(), OperationPurity::Pure);
        assert!(descriptor.type_parameters().is_empty());
        assert!(!descriptor.supports_value_pipeline());
        let labels = signatures
            .into_iter()
            .map(|suffix| format!("std::math::{name}{suffix}"))
            .collect::<Vec<_>>();
        assert_eq!(descriptor.signature_labels(), labels);
        let help = String::from_utf8(render_module_operation_help(descriptor)).unwrap();
        for label in labels {
            assert!(help.contains(&label));
        }
        assert!(help.contains("no implicit conversion"));
    }
    for name in ["mean", "sin", "sum", "random"] {
        assert!(
            catalog
                .query(program.graph().root(), &format!("math::{name}"))
                .is_none()
        );
    }
    assert!(try_load("let x: Int = math::floor(1.0)").is_err());
}

#[test]
fn reference_sources_format_idempotently_and_reparse() {
    use opaal_syntax::{
        FormatOutcome, ParseOutcome, SourceFile, SourceId, format_source_opaal, parse_opaal,
    };
    for text in [
        include_str!("../../../examples/numeric-report/report.opaal"),
        include_str!("../../../examples/numeric-report/show.opaal"),
        include_str!("../../../examples/numeric-report/tasks.opaal"),
        include_str!("../../../examples/numeric-report/caught.opaal"),
        include_str!("../../../examples/numeric-report/domain.opaal"),
        include_str!("../../../examples/numeric-report/mixed.opaal"),
    ] {
        let source = SourceFile::new(SourceId::new(1), "numeric.opaal", text);
        let FormatOutcome::Complete(formatted) = format_source_opaal(&source) else {
            panic!("format reference")
        };
        let source = SourceFile::new(SourceId::new(1), "numeric.opaal", &formatted);
        assert!(matches!(parse_opaal(&source), ParseOutcome::Complete(_)));
        assert_eq!(
            format_source_opaal(&source),
            FormatOutcome::Complete(formatted)
        );
    }
}
