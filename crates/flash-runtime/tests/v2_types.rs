#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flash_platform::FakePlatform;
use flash_runtime::Environment;
use flash_runtime::builtin::standard_registry;
use flash_runtime::eval::FakeClock;
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use flash_runtime::plan::SessionOptions;
use flash_runtime::query::SemanticHover;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::execute_module_program;
use flash_syntax::LanguageMajor;

struct FixtureModules;

struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}

impl ModuleCanonicalizer for FixtureModules {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(candidate.to_path_buf())
    }
}

impl ModuleSourceLoader for FixtureModules {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        fs::read(module.path()).map_err(|error| ModuleSourceError::new(error.to_string()))
    }
}

#[test]
fn v2_type_manifest_executes_through_shared_analysis() {
    let root = type_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();
    let modules = FixtureModules;

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "malformed type row {}", index + 1);
        let report = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
            .analyze(&root.join(fields[1]));
        match fields[0] {
            "complete" | "runtime-invalid" => assert!(
                report.issues().is_empty() && report.program().is_some(),
                "{} must pass shared analysis: {:?}",
                fields[1],
                report.issues()
            ),
            "invalid" => {
                let codes = report
                    .issues()
                    .iter()
                    .flat_map(|issue| issue.error().diagnostics())
                    .map(|diagnostic| diagnostic.code().to_owned())
                    .collect::<Vec<_>>();
                assert!(
                    report.program().is_none() && codes.iter().any(|code| code == fields[2]),
                    "{} must be rejected with {}: {codes:?}; {:?}",
                    fields[1],
                    fields[2],
                    report.issues()
                );
            }
            class => panic!("unknown type corpus class {class:?}"),
        }
    }
}

#[test]
fn dynamic_v2_type_boundaries_fail_at_runtime_with_exact_errors() {
    let root = type_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();
    let modules = FixtureModules;

    for row in manifest.lines() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        if fields[0] != "runtime-invalid" {
            continue;
        }
        let program = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
            .load(&root.join(fields[1]))
            .unwrap_or_else(|error| panic!("{} must pass shared analysis: {error}", fields[1]));
        let error = execute_module_program(
            &program,
            &[],
            &root,
            &mut Environment::new(),
            &standard_registry(),
            &NoExecutables,
            &SessionOptions::default(),
            &FakePlatform::full(),
            Arc::new(FakeClock::new()),
            &mut Vec::new(),
        )
        .expect_err("the dynamic boundary must fail during execution");
        assert!(
            error.render().contains(fields[2]),
            "{} must contain {:?}: {}",
            fields[1],
            fields[2],
            error.render()
        );
    }
}

#[test]
fn cross_module_nominal_annotations_keep_one_semantic_identity() {
    let root = type_root();
    let modules = FixtureModules;
    let program = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
        .load(&root.join("complete/cross-module-main.fsh"))
        .expect("the cross-module nominal fixture must load");
    let root_module = program.graph().root();
    let source = program.sources().source(root_module).unwrap();
    let cursor = source.text().find("model::Box[Int]").unwrap() + "model::".len() + 1;
    let registry = standard_registry();
    let queries = program.semantic_queries(&registry);
    let SemanticHover::NominalType(hover) = queries.hover_at(root_module, cursor).unwrap() else {
        panic!("a qualified nominal annotation must expose nominal hover identity");
    };
    let definition = queries.definition_at(root_module, cursor).unwrap();

    assert_eq!(hover.nominal().id().module(), definition.module());
    assert_eq!(hover.nominal().declaration_span(), definition.span());
    assert_eq!(hover.nominal().id().name(), "Box");
    assert!(
        definition
            .module()
            .path()
            .ends_with("complete/support/model.fsh")
    );
}

#[test]
fn named_function_signatures_expose_only_empty_downstream_slots() {
    let root = type_root();
    let modules = FixtureModules;
    let program = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
        .load(&root.join("complete/generic-inference.fsh"))
        .expect("the generic function fixture must load");
    let signatures = program.types().functions(program.graph().root());

    assert!(!signatures.is_empty());
    assert!(
        signatures
            .iter()
            .all(|signature| signature.downstream().is_foundation_only())
    );
}

#[test]
fn constructors_and_patterns_share_nominal_semantic_queries_through_reexports() {
    let root = type_root();
    let modules = FixtureModules;
    let program = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
        .load(&root.join("complete/reexport-main.fsh"))
        .expect("the deep nominal re-export fixture must load");
    let root_module = program.graph().root();
    let source = program.sources().source(root_module).unwrap();
    let registry = standard_registry();
    let queries = program.semantic_queries(&registry);

    for needle in [
        "api::model::Box { value: 11 }",
        "api::model::Maybe::Some($value)",
        "api::model::Maybe::Some(selected)",
    ] {
        let cursor = source.text().find(needle).unwrap() + needle.find("model").unwrap() + 8;
        let SemanticHover::NominalType(hover) = queries
            .hover_at(root_module, cursor)
            .unwrap_or_else(|| panic!("{needle:?} has no semantic hover"))
        else {
            panic!("{needle:?} must expose nominal hover identity");
        };
        let definition = queries.definition_at(root_module, cursor).unwrap();
        assert_eq!(hover.nominal().id().module(), definition.module());
        assert_eq!(hover.nominal().declaration_span(), definition.span());
    }
}

#[test]
fn complete_v2_type_corpus_executes_nominal_values_and_recursive_patterns() {
    let root = type_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();
    let modules = FixtureModules;

    for row in manifest.lines() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        if fields[0] != "complete" {
            continue;
        }
        let program = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
            .load(&root.join(fields[1]))
            .unwrap();
        let mut environment = Environment::new();
        let mut output = Vec::new();
        execute_module_program(
            &program,
            &[],
            &root,
            &mut environment,
            &standard_registry(),
            &NoExecutables,
            &SessionOptions::default(),
            &FakePlatform::full(),
            Arc::new(FakeClock::new()),
            &mut output,
        )
        .unwrap_or_else(|error| panic!("{} must execute: {}", fields[1], error.render()));
        assert!(output.is_empty(), "{} must not print implicitly", fields[1]);
    }
}

#[test]
fn v2_pattern_diagnostics_do_not_change_frozen_v1_analysis() {
    let root = type_root();
    let modules = FixtureModules;
    let report = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V1)
        .analyze(&root.join("complete/support/v1-static-pattern-baseline.fsh"));
    let codes = report
        .issues()
        .iter()
        .flat_map(|issue| issue.error().diagnostics())
        .map(|diagnostic| diagnostic.code().to_owned())
        .collect::<Vec<_>>();

    assert!(
        report.program().is_some(),
        "frozen v1 analysis changed: {codes:?}; {:?}",
        report.issues()
    );
    assert!(
        !codes
            .iter()
            .any(|code| matches!(code.as_str(), "SIG012" | "SIG017" | "SIG018")),
        "v2-only pattern diagnostics leaked into v1: {codes:?}"
    );
}

fn type_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language/types")
}
