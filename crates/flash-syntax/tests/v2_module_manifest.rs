#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use flash_syntax::{
    FormatOutcome, ModuleImportSource, SourceFile, SourceId, StatementKind, VersionedParseOutcome,
    format_source_v2, parse_v2,
};

#[test]
fn v2_module_manifest_freezes_the_new_forms_and_rejects_v1_imports() {
    let root = module_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 2, "malformed module row {}", index + 1);
        let class = fields[0];
        let relative = fields[1];
        let text = fs::read_to_string(root.join(relative)).unwrap();
        let source = SourceFile::new(SourceId::new(201), relative, text);

        match class {
            "complete" => assert!(
                matches!(parse_v2(&source), VersionedParseOutcome::Complete(_)),
                "{relative} must parse as canonical v2 module syntax"
            ),
            "invalid" => assert!(
                matches!(parse_v2(&source), VersionedParseOutcome::Invalid(_)),
                "{relative} must not preserve a v1 import form"
            ),
            _ => panic!("unknown module fixture class {class}"),
        }
    }
}

#[test]
fn v2_module_forms_have_one_exact_ast_shape() {
    let aliases_source = module_source("complete/qualified-aliases.fsh", 301);
    let VersionedParseOutcome::Complete(aliases) = parse_v2(&aliases_source) else {
        panic!("qualified aliases must parse");
    };
    let statements = aliases.script().statements();
    assert_eq!(statements.len(), 3);

    let StatementKind::ModuleImport(local) = statements[0].kind() else {
        panic!("the first statement must be a v2 module import");
    };
    assert_eq!(text(&aliases_source, local.alias.span()), "model");
    let ModuleImportSource::Local { path } = local.source else {
        panic!("the model import must retain a local origin");
    };
    assert_eq!(text(&aliases_source, path), "'./model.fsh'");

    let StatementKind::ModuleImport(standard) = statements[1].kind() else {
        panic!("the second statement must be a v2 module import");
    };
    assert_eq!(text(&aliases_source, standard.alias.span()), "value");
    let ModuleImportSource::Standard {
        namespace, module, ..
    } = standard.source
    else {
        panic!("the value import must retain a standard origin");
    };
    assert_eq!(text(&aliases_source, namespace.span()), "std");
    assert_eq!(text(&aliases_source, module.span()), "value");

    let StatementKind::ModuleExport(export) = statements[2].kind() else {
        panic!("aliases must use the one explicit export-list form");
    };
    assert_eq!(
        export
            .names
            .iter()
            .map(|name| text(&aliases_source, name.span()))
            .collect::<Vec<_>>(),
        ["model", "value"]
    );

    let nominal = module_source("complete/nominal-type.fsh", 302);
    let VersionedParseOutcome::Complete(nominal_parse) = parse_v2(&nominal) else {
        panic!("the nominal type fixture must parse");
    };
    let StatementKind::NominalType(declaration) = nominal_parse.script().statements()[0].kind()
    else {
        panic!("type must have a dedicated nominal declaration node");
    };
    assert_eq!(text(&nominal, declaration.name.span()), "Item");
    assert_eq!(declaration.fields.len(), 1);
    assert_eq!(text(&nominal, declaration.fields[0].name.span()), "value");
    assert_eq!(
        text(&nominal, declaration.fields[0].value_type.name.span()),
        "Int"
    );
}

#[test]
fn v2_module_forms_format_canonically_and_idempotently() {
    let source = SourceFile::new(
        SourceId::new(303),
        "module-format.fsh",
        concat!(
            "language   2\n\n",
            "import   './model.fsh'   as   model\n",
            "import   std::value   as   value\n",
            "export   {   model,   value   }\n\n",
            "type   Item   =   {\n",
            "value:   Int,\n",
            "}\n",
        ),
    );
    let expected = concat!(
        "language 2\n\n",
        "import './model.fsh' as model\n",
        "import std::value as value\n",
        "export { model, value }\n\n",
        "type Item = {\n",
        "    value: Int,\n",
        "}\n",
    );

    let FormatOutcome::Complete(formatted) = format_source_v2(&source) else {
        panic!("the complete v2 module must format");
    };
    assert_eq!(formatted, expected);
    let reparsed = SourceFile::new(SourceId::new(304), "formatted.fsh", formatted.clone());
    assert_eq!(
        format_source_v2(&reparsed),
        FormatOutcome::Complete(formatted)
    );
}

fn module_source(relative: &str, id: u32) -> SourceFile {
    let path = module_root().join(relative);
    SourceFile::new(
        SourceId::new(id),
        relative,
        fs::read_to_string(path).unwrap(),
    )
}

fn text(source: &SourceFile, span: flash_syntax::Span) -> &str {
    source.slice(span).unwrap()
}

fn module_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language/modules")
}
