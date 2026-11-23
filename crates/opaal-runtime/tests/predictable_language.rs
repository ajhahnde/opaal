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
fn if_expressions_retain_expected_and_inferred_types() {
    for (text, expected) in [
        (
            "let value = if false { 7 } else if true { 8 } else { 9 }\nvalue",
            Value::Int(8),
        ),
        (
            "def identity[T](value: T) -> T { return value }\nidentity(if true { ([]) } else { [1] })",
            Value::list(Vec::new()),
        ),
        (
            "def identity[T](value: T) -> T { return value }\nlet selected = if true { [] } else { [1] }\nidentity(selected)",
            Value::list(Vec::new()),
        ),
        (
            "def identity[T](value: T) -> T { return value }\nmut selected = if true { [] } else { [1] }\nidentity(selected)",
            Value::list(Vec::new()),
        ),
        (
            "def identity[T](value: T) -> T { return value }\nlet selected = if true { [] } else { [1] }\nlet alias = selected\nidentity((alias))",
            Value::list(Vec::new()),
        ),
        (
            "def identity[T](value: T) -> T { return value }\nlet nested = [if true { [] } else { [1] }]\nidentity(nested)",
            Value::list(vec![Value::list(Vec::new())]),
        ),
        (
            "let other: Any = 'unused'\nlet selected: Int = if true { 1 } else { (other) }\nselected",
            Value::Int(1),
        ),
        (
            "def identity[T](value: T) -> T { return value }\ndef pick(flag: Bool) -> Any { (if flag { 2 } else { 'two' }) }\npick(false)",
            Value::string("two"),
        ),
        (
            "def accept(value: List[Int]) -> List[Int] { return value }\naccept(if true { [] } else { [2] })",
            Value::list(Vec::new()),
        ),
        (
            "let f = {|flag: Bool| -> Any if flag { 'two' } else { 2 }}\nf(true)",
            Value::string("two"),
        ),
        (
            "mut items: List[Int] = [1]\nitems[0] = if true { 2 } else { 3 }\nitems[0]",
            Value::Int(2),
        ),
        (
            "type Box[T] = { value: T }\ndef pick(flag: Bool) -> Box[Int] { (if flag { Box {value: 1} } else { Box {value: 2} }) }\npick(false).value",
            Value::Int(2),
        ),
        (
            "mut selected = if true { 1 } else { 2 }\nselected = 'two'\nselected",
            Value::string("two"),
        ),
        (
            "let selected = if true { [1] } else { [2] }[0]\nselected",
            Value::Int(1),
        ),
        (
            "def fail() -> Int { throw 'unexpected' }\nlet value = if true || fail() { 3 } else if fail() { fail() } else { 4 }\nvalue",
            Value::Int(3),
        ),
        (
            "let selected = if true { {|| 1} } else { {|| 2} }\nselected()",
            Value::Int(1),
        ),
        (
            "def first() -> Int { return 1 }\ndef other() -> Int { return 2 }\nlet selected = if true { first } else { other }\nselected() + (if false { first } else { other })()",
            Value::Int(3),
        ),
        (
            "def consume(value: Int) -> Int { return value }\n(consume)(7)",
            Value::Int(7),
        ),
        (
            "def pick(flag: Bool, x: Any) -> Int { let picked: Int = if flag { 1 } else { x }; return picked }\npick(true, 'unused') + pick(false, 7)",
            Value::Int(8),
        ),
    ] {
        assert_eq!(checked_value("if expression", text), expected, "{text}");
    }
}

#[test]
fn dynamic_if_arm_defers_to_the_selected_runtime_type_guard() {
    let text = "def choose[T](first: T, second: T) -> T { return first }\ndef pick(flag: Bool, dynamic: Any) -> Any { let value = choose(if flag { 1 } else { dynamic }, 7); return value }\nmut caught = false\ntry { let unexpected = pick(false, 'wrong') } catch error { caught = true }\ncaught && pick(true, 'unused') == 1 && pick(false, 7) == 7";
    assert_eq!(
        checked_value("dynamic conditional argument", text),
        Value::Bool(true)
    );
}

#[test]
fn legacy_dynamic_calls_and_grouped_values_keep_runtime_checks() {
    for text in [
        "mut caught = false\ntry { let value = [1][0]() } catch error { caught = true }\ncaught",
        "let earlier = 'two'\nmut caught = false\ntry { let value: Int = (earlier) } catch error { caught = true }\ncaught",
        "def consume(value: Int) -> Int { return value }\nmut caught = false\ntry { (consume)(consume('two')) } catch error { caught = true }\ncaught",
    ] {
        assert_eq!(
            checked_value("legacy dynamic checks", text),
            Value::Bool(true)
        );
    }
}

#[test]
fn if_expression_checks_all_known_arms_and_reports_the_original_spans() {
    for (text, primary, secondary) in [
        ("let value = if true { 1 } else { 'two' }", "'two'", "1"),
        ("let value = if true { 'two' } else { 1 }", "1", "'two'"),
        (
            "let value: Int = if true { 1 } else { 'two' }",
            "'two'",
            "Int",
        ),
        ("let value = if true { ([]) } else { 1 }", "1", "([])"),
        (
            "def choose(flag: Bool, any: Any) { let value = if flag { any } else if false { 1 } else { 'two' } }",
            "'two'",
            "1",
        ),
        (
            "let loose: Any = 3\nlet result = if false { (loose, 1) } else if false { (37, 1) } else { (\"wrong\", 1) }",
            "\"wrong\"",
            "37",
        ),
        (
            "let loose: Any = 3\nlet result = if false { ((loose, 1), 2) } else if false { ((37, 1), 2) } else { ((\"wrong\", 1), 2) }",
            "\"wrong\"",
            "37",
        ),
    ] {
        let source = OneModule {
            source: text.as_bytes().to_vec(),
        };
        let report = ModuleProgramLoader::new(&source, &source).analyze_with_commands(
            Path::new("/predictable/invalid-if.opaal"),
            &standard_registry(),
        );
        assert!(report.program().is_none(), "{text}");
        let diagnostic = report
            .issues()
            .iter()
            .flat_map(|issue| issue.error().diagnostics())
            .find(|diagnostic| diagnostic.code() == "SIG025")
            .unwrap_or_else(|| panic!("{text}: {:?}", report.issues()));
        for (label, expected) in diagnostic.labels().iter().zip([primary, secondary]) {
            let span = label.span();
            assert_eq!(&text[span.start()..span.end()], expected, "{text}");
        }
        if secondary == "37" {
            assert_eq!(
                diagnostic.message(),
                "if expression arm supplies `String`, expected `Int`"
            );
        }
    }
}

#[test]
fn invalid_name_assignment_and_call_forms_fail_static_analysis() {
    for (case, text, expected_code) in [
        ("unknown bare name", "missing\n", "CMD007"),
        (
            "unknown qualified call in an unselected arm",
            "let value = if true { 1 } else { missing::call() }",
            "MOD009",
        ),
        (
            "unknown constructor in an unselected arm",
            "enum Choice { Present, Absent }\nlet value = if true { Choice::Present } else { Choice::Unknown }",
            "MOD009",
        ),
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
