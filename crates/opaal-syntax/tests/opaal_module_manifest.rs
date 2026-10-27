#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use opaal_syntax::{
    FormatOutcome, ModuleImportSource, ParseOutcome, SourceFile, SourceId, StatementKind,
    format_source_opaal, parse_opaal,
};

#[test]
fn opaal_module_manifest_freezes_the_supported_forms_and_rejects_other_imports() {
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
                matches!(parse_opaal(&source), ParseOutcome::Complete(_)),
                "{relative} must parse as canonical opaal module syntax"
            ),
            "invalid" => assert!(
                matches!(parse_opaal(&source), ParseOutcome::Invalid(_)),
                "{relative} must not parse as a supported module import"
            ),
            _ => panic!("unknown module fixture class {class}"),
        }
    }
}

#[test]
fn opaal_module_forms_have_one_exact_ast_shape() {
    let aliases_source = module_source("complete/qualified-aliases.opaal", 301);
    let ParseOutcome::Complete(aliases) = parse_opaal(&aliases_source) else {
        panic!("qualified aliases must parse");
    };
    let statements = aliases.statements();
    assert_eq!(statements.len(), 3);

    let StatementKind::ModuleImport(local) = statements[0].kind() else {
        panic!("the first statement must be a opaal module import");
    };
    assert_eq!(text(&aliases_source, local.alias.span()), "model");
    let ModuleImportSource::Local { path } = local.source else {
        panic!("the model import must retain a local origin");
    };
    assert_eq!(text(&aliases_source, path), "'./model.opaal'");

    let StatementKind::ModuleImport(standard) = statements[1].kind() else {
        panic!("the second statement must be a opaal module import");
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

    let nominal = module_source("complete/nominal-type.opaal", 302);
    let ParseOutcome::Complete(nominal_parse) = parse_opaal(&nominal) else {
        panic!("the nominal type fixture must parse");
    };
    let StatementKind::NominalType(declaration) = nominal_parse.statements()[0].kind() else {
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
fn opaal_module_forms_format_canonically_and_idempotently() {
    let source = SourceFile::new(
        SourceId::new(303),
        "module-format.opaal",
        concat!(
            "\n",
            "import   './model.opaal'   as   model\n",
            "import   std::value   as   value\n",
            "export   {   model,   value   }\n\n",
            "type   Item   =   {\n",
            "value:   Int,\n",
            "}\n",
        ),
    );
    let expected = concat!(
        "\n",
        "import './model.opaal' as model\n",
        "import std::value as value\n",
        "export { model, value }\n\n",
        "type Item = {\n",
        "    value: Int,\n",
        "}\n",
    );

    let FormatOutcome::Complete(formatted) = format_source_opaal(&source) else {
        panic!("the complete opaal module must format");
    };
    assert_eq!(formatted, expected);
    let reparsed = SourceFile::new(SourceId::new(304), "formatted.opaal", formatted.clone());
    assert_eq!(
        format_source_opaal(&reparsed),
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

fn text(source: &SourceFile, span: opaal_syntax::Span) -> &str {
    source.slice(span).unwrap()
}

fn module_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/opaal-foundation/language/modules")
}

#[test]
fn concise_imports_keep_literal_aliases_and_original_entry_spans() {
    for (origin, alias) in [
        ("std::math", "math"),
        ("project::context", "context"),
        ("'./helpers.opaal'", "helpers"),
        ("'./界/helpers.opaal'", "helpers"),
        ("'./helpers'", "helpers"),
        ("'./as.opaal'", "as"),
        ("'./UPPER_2.opaal'", "UPPER_2"),
        ("'./a,b}.opaal' as tools", "tools"),
    ] {
        for input in [format!("import {origin}"), format!("import {{ {origin} }}")] {
            let source = SourceFile::new(SourceId::new(401), "alias", &input);
            let ParseOutcome::Complete(script) = parse_opaal(&source) else {
                panic!("{input}")
            };
            let entries = script.module_imports().collect::<Vec<_>>();
            assert_eq!(entries.len(), 1);
            assert_eq!(
                source
                    .slice(entries[0].alias().unwrap().alias.span())
                    .unwrap(),
                alias
            );
            let units = script.semantic_statements(&source).collect::<Vec<_>>();
            assert_eq!(units.len(), 1);
            assert!(source.slice(units[0].span()).unwrap().contains(origin));
        }
    }
    for component in [
        "",
        "1a",
        "build-tools",
        "a.txt",
        "a.opaal.opaal",
        "a.OPAAL",
        "界",
        "let",
        ".",
        "..",
        "back\\name",
    ] {
        let input = format!(
            "import './{component}{}'",
            if component.is_empty() { "" } else { ".opaal" }
        );
        let source = SourceFile::new(SourceId::new(402), "invalid-alias", &input);
        let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
            panic!("{input}")
        };
        assert!(
            diagnostics[0].message().contains("as alias"),
            "{diagnostics:?}"
        );
        let explicit = SourceFile::new(
            SourceId::new(403),
            "explicit",
            format!("{input} as renamed"),
        );
        assert!(matches!(parse_opaal(&explicit), ParseOutcome::Complete(_)));
    }
}

#[test]
fn concise_imports_classify_complete_incomplete_and_invalid_without_prefix_execution() {
    for input in [
        "import std::math",
        "import {std::math}",
        "import {std::math, std::io,}",
        "import {std::math\n as\n m, # next\n './x.opaal'}",
    ] {
        let source = SourceFile::new(SourceId::new(404), "complete", input);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Complete(_)),
            "{input}"
        );
    }
    for input in [
        "import",
        "import std::",
        "import {",
        "import { std::math,",
        "import std::math as",
        "import {std::math as",
    ] {
        let source = SourceFile::new(SourceId::new(405), "incomplete", input);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Incomplete(_)),
            "{input}: {:?}",
            parse_opaal(&source)
        );
    }
    for input in [
        "import {}",
        "import {, std::math}",
        "import {std::math,, std::io}",
        "import {std::math std::io}",
        "import {{std::math}}",
        "import {std::math} as all",
        "import std::{math, io}",
        "import math",
        "import {std::math, } bad",
        "def f() { import {std::math} }",
    ] {
        let source = SourceFile::new(SourceId::new(406), "invalid", input);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Invalid(_)),
            "{input}"
        );
        assert!(
            matches!(format_source_opaal(&source), FormatOutcome::Invalid(_)),
            "{input}"
        );
    }
}

#[test]
fn member_selectors_preserve_tokens_without_creating_an_origin_alias() {
    use opaal_syntax::{ModuleImportRef, StatementKind};
    for text in [
        "import std::math::{abs as magnitude_of}\n",
        "import './not-a-module-name.opaal'::{Payload, make as create, code,}\n",
        "import {std::math::{abs}, std::io, './x.opaal'::{value}}\n",
    ] {
        let source =
            opaal_syntax::SourceFile::new(opaal_syntax::SourceId::new(90), "selectors", text);
        let opaal_syntax::ParseOutcome::Complete(script) = opaal_syntax::parse_opaal(&source)
        else {
            panic!("{text}");
        };
        let entries = script.module_imports().collect::<Vec<_>>();
        assert!(matches!(entries[0], ModuleImportRef::Members(_)));
        for entry in entries {
            if let Some(import) = entry.members() {
                assert!(entry.alias().is_none());
                assert!(source.slice(import.span).unwrap().starts_with('{'));
                for selector in &import.members {
                    assert!(!source.slice(selector.member.span()).unwrap().is_empty());
                    assert!(!source.slice(selector.local.span()).unwrap().is_empty());
                }
            }
        }
        let units = script.semantic_statements(&source).collect::<Vec<_>>();
        assert!(matches!(
            units[0].kind(),
            StatementKind::ModuleMemberImport(_)
        ));
    }
}

#[test]
fn member_selector_recovery_distinguishes_missing_input_and_invalid_syntax() {
    use opaal_syntax::{ParseOutcome, SourceFile, SourceId, parse_opaal};
    for text in [
        "import std::math::{",
        "import std::math::{abs",
        "import std::math::{abs as",
        "import {std::math::{abs},",
    ] {
        let source = SourceFile::new(SourceId::new(91), "incomplete", text);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Incomplete(_)),
            "{text}: {:?}",
            parse_opaal(&source)
        );
    }
    for text in [
        "import std::math::{}",
        "import std::math::{*}",
        "import std::math::{abs::{x}}",
        "import std::math::{abs} as math",
        "import math::{abs}",
        "if true { import std::math::{abs} }",
    ] {
        let source = SourceFile::new(SourceId::new(92), "invalid", text);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Invalid(_)),
            "{text}: {:?}",
            parse_opaal(&source)
        );
    }
}

#[test]
fn export_prefix_selectors_retain_tokens_and_reject_other_wildcards() {
    use opaal_syntax::ModuleExportSelector;
    for text in [
        "export { test_* }\n",
        "export { public, check *, Item }\n",
        "export {\n# kept\ntest_*,\n}\n",
    ] {
        let source = SourceFile::new(SourceId::new(401), "prefix.opaal", text);
        let ParseOutcome::Complete(script) = parse_opaal(&source) else {
            panic!("{text}");
        };
        let StatementKind::ModuleExport(export) = script.statements()[0].kind() else {
            panic!();
        };
        let selector = export
            .names
            .iter()
            .find(|selector| matches!(selector, ModuleExportSelector::Prefix { .. }))
            .unwrap();
        let ModuleExportSelector::Prefix { prefix, marker } = selector else {
            panic!();
        };
        assert_eq!(source.slice(*marker).unwrap(), "*");
        assert_eq!(selector.span(), prefix.span().cover(*marker));
        assert!(source.slice(selector.span()).unwrap().ends_with('*'));
    }
    for text in [
        "export {}",
        "export { * }",
        "export { *test }",
        "export { test** }",
        "export { te*st }",
        "export { 'test_*' }",
        "export { m::test* }",
        "export { test* as name }",
        "export { {test*} }",
        "export { test\n* }",
        "export { test # comment\n* }",
        "export { test \\\n* }",
        "if true { export {test*} }",
        "import std::math::{abs*}",
    ] {
        let source = SourceFile::new(SourceId::new(402), "invalid.opaal", text);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Invalid(_)),
            "{text}"
        );
    }
    for text in ["export {", "export { test_", "export {\n test_*,\n"] {
        let source = SourceFile::new(SourceId::new(403), "incomplete.opaal", text);
        assert!(
            matches!(parse_opaal(&source), ParseOutcome::Incomplete(_)),
            "{text}"
        );
    }
    for text in ["export VALUE = 2 * 3\n", "echo test*\n"] {
        let source = SourceFile::new(SourceId::new(404), "unchanged.opaal", text);
        assert!(matches!(parse_opaal(&source), ParseOutcome::Complete(_)));
    }
}
