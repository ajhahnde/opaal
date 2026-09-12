#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::FakeClock;
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleEffect, ModuleId, ModulePathError, ModuleProgramLoader,
    ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::execute_module_program;
use opaal_runtime::session::Session;
use opaal_runtime::{Environment, Value};

struct OneModule {
    source: Vec<u8>,
}

impl ModuleCanonicalizer for OneModule {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(candidate.to_path_buf())
    }
}

impl ModuleSourceLoader for OneModule {
    fn load(&self, _module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(self.source.clone())
    }
}

struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}

#[derive(Default)]
struct CountingExecutableProbe {
    calls: AtomicUsize,
}

impl ExecutableProbe for CountingExecutableProbe {
    fn is_executable(&self, _path: &OsStr) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        true
    }
}

fn checked_value(case: &str, source: &str) -> Value {
    let source = OneModule {
        source: source.as_bytes().to_vec(),
    };
    let root = Path::new("/predictable/main.opaal");
    let registry = standard_registry();
    let report = ModuleProgramLoader::new(&source, &source).analyze_with_commands(root, &registry);
    assert!(
        report.issues().is_empty(),
        "{case} must pass static analysis: {:?}",
        report.issues()
    );
    let program = report
        .program()
        .cloned()
        .unwrap_or_else(|| panic!("{case} must produce an executable module program"));
    let mut environment = Environment::new();
    let mut output = Vec::new();
    let platform = FakePlatform::full();
    let completion = execute_module_program(
        &program,
        &[],
        Path::new("/predictable"),
        &mut environment,
        &registry,
        &NoExecutables,
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
        &mut output,
    )
    .unwrap_or_else(|error| panic!("{case} must execute after checking: {error}"));
    assert!(output.is_empty(), "{case} must not write process output");
    completion.value().clone()
}

#[test]
fn checked_names_calls_and_block_values_equal_their_runtime_values() {
    let cases = [
        (
            "bare read and assignment",
            "mut value = 1\nvalue = value + 1\nvalue\n",
            Value::Int(2),
        ),
        (
            "function and if block values",
            "def choose(flag: Bool) -> Int {\n    if flag {\n        7\n    } else {\n        9\n    }\n}\nchoose(true)\n",
            Value::Int(7),
        ),
        (
            "match arm block value",
            "match 2 {\n    1 => { 10 }\n    2 => { 20 }\n    _ => { 30 }\n}\n",
            Value::Int(20),
        ),
        (
            "closure body and value call",
            "let increment = {|value: Int| value + 1}\nincrement(4)\n",
            Value::Int(5),
        ),
        (
            "terminated explicit return",
            "def choose() -> Int { return 1; }\nchoose()\n",
            Value::Int(1),
        ),
    ];

    for (case, source, expected) in cases {
        assert_eq!(checked_value(case, source), expected, "{case}");
    }
}

#[test]
fn invalid_name_assignment_and_call_forms_fail_static_analysis() {
    for (case, text, expected_code) in [
        ("unknown bare name", "missing\n", "CMD007"),
        (
            "immutable assignment",
            "let value = 1\nvalue = 2\n",
            "BND001",
        ),
        ("non-callable value", "let value = 1\nvalue()\n", "SIG022"),
        (
            "unknown command nested in spread expression",
            "which ...{[(misspelled)]}\n",
            "CMD007",
        ),
        (
            "command status function result",
            "def wrong() -> Int { pwd }\n",
            "SIG005",
        ),
    ] {
        let source = OneModule {
            source: text.as_bytes().to_vec(),
        };
        let report = ModuleProgramLoader::new(&source, &source).analyze_with_commands(
            Path::new("/predictable/invalid.opaal"),
            &standard_registry(),
        );
        assert!(
            report.program().is_none(),
            "{case} must not produce a program"
        );
        assert!(
            !report.issues().is_empty(),
            "{case} must produce a diagnostic"
        );
        let codes = report
            .issues()
            .iter()
            .flat_map(|issue| issue.error().diagnostics())
            .map(|diagnostic| diagnostic.code().to_owned())
            .collect::<Vec<_>>();
        assert!(
            codes.iter().any(|code| code == expected_code),
            "{case} must produce {expected_code}, found {codes:?}"
        );
    }
}

#[test]
fn implicit_function_results_are_checked_across_block_forms() {
    for (case, text) in [
        (
            "if branch result",
            "def choose(flag: Bool) -> Int {\n    if flag {\n        1\n    } else {\n        \"wrong\"\n    }\n}\n",
        ),
        (
            "if missing else null result",
            "def choose(flag: Bool) -> Int {\n    if flag {\n        1\n    }\n}\n",
        ),
        (
            "match arm result",
            "def choose(value: Int) -> Int {\n    match value {\n        1 => { 1 }\n        _ => { \"wrong\" }\n    }\n}\n",
        ),
        (
            "try catch result",
            "def choose() -> Int {\n    try {\n        1\n    } catch error {\n        \"wrong\"\n    }\n}\n",
        ),
        ("empty function result", "def choose() -> Int {}\n"),
        (
            "terminated function result",
            "def choose() -> Int {\n    1;\n}\n",
        ),
    ] {
        let source = OneModule {
            source: text.as_bytes().to_vec(),
        };
        let report = ModuleProgramLoader::new(&source, &source).analyze_with_commands(
            Path::new("/predictable/invalid-result.opaal"),
            &standard_registry(),
        );
        let codes = report
            .issues()
            .iter()
            .flat_map(|issue| issue.error().diagnostics())
            .map(|diagnostic| diagnostic.code().to_owned())
            .collect::<Vec<_>>();
        assert!(
            report.program().is_none() && codes.iter().any(|code| code == "SIG005"),
            "{case} must fail with SIG005, found {codes:?}"
        );
    }
}

#[test]
fn command_spread_evaluates_against_the_session_environment() {
    let source = OneModule {
        source: b"which ...{[env(\"TARGET\")]}\n".to_vec(),
    };
    let registry = standard_registry();
    let report = ModuleProgramLoader::new(&source, &source)
        .analyze_with_commands(Path::new("/predictable/effects.opaal"), &registry);
    let program = report.program().expect("the spread source must check");
    assert!(
        program
            .effects()
            .direct(program.graph().root())
            .occurrences()
            .iter()
            .any(|occurrence| occurrence.effect() == ModuleEffect::ChildEnvironment)
    );

    let probe = CountingExecutableProbe::default();
    let platform = FakePlatform::full();
    let mut session = Session::new(
        PathBuf::from("/predictable"),
        Environment::from_snapshot([("PATH", "/fake"), ("TARGET", "tool")]),
        SessionOptions::default(),
    );
    session
        .submit(
            "interactive",
            "which ...{[env(\"TARGET\")]}\n",
            &probe,
            &platform,
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .expect("spread expressions must see the session environment");
}

#[test]
fn interactive_unknown_names_fail_before_external_lookup() {
    let probe = CountingExecutableProbe::default();
    let platform = FakePlatform::full();
    let mut session = Session::new(
        PathBuf::from("/predictable"),
        Environment::from_snapshot([("PATH", "/fake")]),
        SessionOptions::default(),
    );
    let error = session
        .submit(
            "interactive",
            "misspelled\n",
            &probe,
            &platform,
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .expect_err("an unknown bare name must fail");

    assert!(
        error
            .render()
            .contains("unknown bare name cannot start an external process"),
        "{}",
        error.render()
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}
