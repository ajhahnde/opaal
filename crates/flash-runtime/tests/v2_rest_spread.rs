#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flash_platform::FakePlatform;
use flash_runtime::builtin::standard_registry;
use flash_runtime::eval::{FakeClock, RuntimeErrorKind, evaluate, expand_spread};
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use flash_runtime::plan::SessionOptions;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::execute_module_program;
use flash_runtime::{BindingMutability, Environment, ScopeStack, Value};
use flash_syntax::{
    CommandItemKind, ParseOutcome, SourceFile, SourceId, StageKind, StatementKind,
    VersionedParseOutcome, parse_v2, parse_v2_submission,
};

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
fn rest_spread_manifest_closes_static_and_dynamic_mismatch_routes() {
    let root = fixture_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();
    let modules = FixtureModules;

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "malformed rest/spread row {}", index + 1);
        if fields[0] == "syntax" {
            continue;
        }
        let report =
            ModuleProgramLoader::for_language(&modules, &modules, flash_syntax::LanguageMajor::V2)
                .analyze_with_commands(&root.join(fields[1]), &standard_registry());
        match fields[0] {
            "complete" | "cli" | "runtime-invalid" => {
                let program = report.program().unwrap_or_else(|| {
                    panic!(
                        "{} must pass shared analysis: {:?}",
                        fields[1],
                        report.issues()
                    )
                });
                let arguments = if fields[0] == "cli" {
                    ["build", "--target", "x86_64-unknown-linux-gnu", "Grüße 🌍"]
                        .map(str::to_owned)
                        .to_vec()
                } else {
                    Vec::new()
                };
                let result = execute_module_program(
                    program,
                    &arguments,
                    &root,
                    &mut Environment::new(),
                    &standard_registry(),
                    &NoExecutables,
                    &SessionOptions::default(),
                    &FakePlatform::full(),
                    Arc::new(FakeClock::new()),
                    &mut Vec::new(),
                );
                if fields[0] == "runtime-invalid" {
                    let error = result.expect_err("the dynamic mismatch must fail at runtime");
                    assert!(
                        error.render().contains(fields[2]),
                        "{} must contain {:?}: {}",
                        fields[1],
                        fields[2],
                        error.render()
                    );
                } else {
                    result.unwrap_or_else(|error| {
                        panic!("{} must execute: {}", fields[1], error.render())
                    });
                }
            }
            "invalid" => {
                let codes = report
                    .issues()
                    .iter()
                    .flat_map(|issue| issue.error().diagnostics())
                    .map(|diagnostic| diagnostic.code().to_owned())
                    .collect::<Vec<_>>();
                assert!(report.program().is_none(), "{} must fail", fields[1]);
                assert_eq!(
                    codes,
                    [fields[2]],
                    "{} must have one exact spread diagnostic",
                    fields[1]
                );
            }
            class => panic!("unknown rest/spread corpus class {class:?}"),
        }
    }
}

#[test]
fn pure_list_rest_then_explicit_spread_preserves_every_argument_exactly() {
    let source = SourceFile::new(
        SourceId::new(1_300),
        "build-arguments.fsh",
        fs::read_to_string(fixture_root().join("complete/build-arguments.fsh")).unwrap(),
    );
    let VersionedParseOutcome::Complete(parsed) = parse_v2(&source) else {
        panic!("the build-argument fixture must parse");
    };

    for total in 0..=64 {
        let mut arguments = Vec::with_capacity(total);
        if total > 0 {
            arguments.push("build".to_owned());
        }
        for index in 1..total {
            arguments.push(match index % 4 {
                0 => String::new(),
                1 => format!("argument {index} with spaces"),
                2 => format!("Grüße-🌍-{index}"),
                _ => format!("--option={index}"),
            });
        }
        let mut scope = ScopeStack::new();
        scope
            .declare(
                "args",
                BindingMutability::Immutable,
                Value::list(arguments.iter().cloned().map(Value::string).collect()),
            )
            .unwrap();
        let tail = evaluate(parsed.script(), &source, &mut scope).unwrap();
        let expected = arguments.iter().skip(1).cloned().collect::<Vec<_>>();
        assert_eq!(
            tail,
            Value::list(expected.iter().cloned().map(Value::string).collect())
        );

        let mut spread_scope = ScopeStack::new();
        spread_scope
            .declare("forwarded", BindingMutability::Immutable, tail)
            .unwrap();
        let words = expand_submission("builder ...$forwarded", &mut spread_scope).unwrap();
        assert_eq!(
            words.iter().map(|word| word.value()).collect::<Vec<_>>(),
            expected
                .iter()
                .map(|value| OsStr::new(value.as_str()))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn explicit_spread_never_flattens_a_nested_list() {
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "forwarded",
            BindingMutability::Immutable,
            Value::list(vec![Value::list(vec![Value::string("nested")])]),
        )
        .unwrap();
    assert_eq!(
        expand_submission("builder ...$forwarded", &mut scope)
            .unwrap_err()
            .kind(),
        &RuntimeErrorKind::SpreadElementNotWordEligible {
            index: 0,
            actual: "list",
        }
    );
}

#[test]
fn spread_type_diagnostics_are_v2_only() {
    let modules = FixtureModules;
    let source = fixture_root().join("invalid/non-list-spread.fsh");
    let v2 = ModuleProgramLoader::for_language(&modules, &modules, flash_syntax::LanguageMajor::V2)
        .analyze_with_commands(&source, &standard_registry());
    assert!(v2.issues().iter().any(|issue| {
        issue
            .error()
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code() == "SIG020")
    }));

    let v1 = ModuleProgramLoader::new(&modules, &modules).analyze_with_commands(
        &fixture_root().join("support/v1-spread-baseline.fsh"),
        &standard_registry(),
    );
    assert!(
        v1.issues().is_empty() && v1.program().is_some(),
        "frozen-v1 spread analysis must not receive v2 diagnostics: {:?}",
        v1.issues()
    );
}

fn expand_submission(
    text: &str,
    scope: &mut ScopeStack,
) -> Result<Vec<flash_runtime::eval::ExpandedWord>, flash_runtime::eval::RuntimeError> {
    let source = SourceFile::new(SourceId::new(1_302), "spread.fsh", text);
    let ParseOutcome::Complete(script) = parse_v2_submission(&source) else {
        panic!("the spread submission must parse");
    };
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("the spread submission must be a command job");
    };
    let StageKind::Command(command) = job.chain.or_terms()[0].and_terms()[0].stages()[0].kind()
    else {
        panic!("the spread submission must be a command stage");
    };
    let item = &command.items[0];
    let CommandItemKind::Spread(variable) = item.kind() else {
        panic!("the command item must be an explicit spread");
    };
    expand_spread(variable, item.span(), &source, scope)
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language/rest-spread")
}
