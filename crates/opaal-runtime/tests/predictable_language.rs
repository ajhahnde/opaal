#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::FakeClock;
use opaal_runtime::module::{
    AnalysisControl, AnalysisLimitKind, AnalysisLimits, ModuleAnalysisOutcome, ModuleCanonicalizer,
    ModuleEffect, ModuleId, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
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
fn list_comprehensions_resolve_the_source_then_filter_and_project() {
    let value = checked_value(
        "comprehension ordering and captures",
        r#"
def source() -> List[Int] { return [0, 2, 0, 3] }
def project(n: Int) -> Int {
    if n == 0 { throw "unselected projection" }
    return n * 10
}
let item = source()
let rows = [(i, project(item)) for i, item in item if (item != 0 && 9 / item > 0)]
let closures = [{|| n } for n in 2..=3]
let first = closures[0]
let next = closures[1]
let pairs = [i + left + right for i, (left, right) in [(1, 2), (1, 2)]]
if first() != 2 || next() != 3 || pairs != [3, 4] || item != [0, 2, 0, 3] {
    throw "comprehension binding escaped its iteration"
}
rows
"#,
    );
    assert_eq!(
        value,
        Value::list(vec![
            Value::Tuple(Arc::from([Value::Int(1), Value::Int(20)])),
            Value::Tuple(Arc::from([Value::Int(3), Value::Int(30)])),
        ])
    );
}

#[test]
fn list_comprehensions_type_imported_value_pipeline_stages() {
    let value = checked_value(
        "comprehension projections with an aliased value pipeline stage",
        r#"
import std::value::{length as count_items}
def keep[T](rows: List[T]) -> List[T] { return rows }
let typed: List[Int] = [([n] | count_items) for n in [1]]
let inferred = [([n] | count_items) for n in [1, 2]]
let empty = [([n] | count_items) for n in 1..1]
if typed != [1] || inferred != [1, 1] || keep[Int](empty) != [] {
    throw "comprehension projection returned an unexpected result"
}
keep(empty)
"#,
    );
    assert_eq!(value, Value::list(vec![]));
}

#[test]
fn list_comprehensions_retain_static_context_and_stop_on_deferred_mismatches() {
    let value = checked_value(
        "comprehension typed empties and dynamic failure",
        r#"
def keep[T](rows: List[T]) -> List[T] { return rows }
def with_witness[T](rows: List[T], witness: T) -> List[T] { return rows }
import std::list
def predicate(row: List[Int]) -> Bool { return row == [] }
def dynamic() -> Any { return "wrong" }
def int_predicate(item: Int) -> Bool { return item == 0 }
def project(n: Int) -> Any {
    if n == 1 { return dynamic() }
    throw "later projection"
}
let empty = [n for n in 1..1]
let nested = with_witness([[] for n in [1]], [0])
let counted = list::count([[] for n in [1]], predicate)
let dynamic_empty = with_witness([dynamic() for n in []], 0)
let dynamic_count = list::count([dynamic() for n in []], int_predicate)
let explicit = keep[List[Int]]([[] for n in []])
let later = keep((empty))
mut caught = false
try {
    let values: List[Int] = [project(n) for n in 1..=2]
} catch error {
    caught = !(error.message in ["later projection"])
}
if !caught || nested != [[]] || counted != 1 || explicit != [] || later != [] ||
    dynamic_empty != [] || dynamic_count != 0 {
    throw "comprehension type evidence was lost"
}
true
"#,
    );
    assert_eq!(value, Value::Bool(true));
}

#[test]
fn inferred_comprehension_arguments_do_not_repeat_generic_analysis() {
    let registry = standard_registry();
    let root = Path::new("/predictable/main.opaal");
    let kind = AnalysisLimitKind::GenericInstantiations;
    for (prefix, suffix) in [("fixed(", ")"), ("value::length([", "])")] {
        for projection in [false, true] {
            let mut work = Vec::new();
            let counts: &[usize] = if projection && prefix == "value::length([" {
                &[4, 8, 12]
            } else {
                &[4, 8, 12, 18]
            };
            for &calls in counts {
                let nested = format!("{}1{}", prefix.repeat(calls), suffix.repeat(calls));
                let expression = if projection {
                    format!("[{nested} for item in [0]]")
                } else {
                    nested
                };
                let source = OneModule {
                    source: format!(
                        "import std::value\ndef fixed[T](value: T) -> Int {{ return 1 }}\nlet output = {expression}\n"
                    ).into_bytes(),
                };
                let loader = ModuleProgramLoader::new(&source, &source);
                let report = loader.analyze_with_commands(root, &registry);
                assert!(report.issues().is_empty(), "{:?}", report.issues());
                assert_eq!(report.usage().get(kind), calls as u64, "{expression}");
                work.push(report.usage().get(AnalysisLimitKind::WorkUnits));
                let outcome = loader.analyze_with_commands_and_limits_controlled(
                    root,
                    &registry,
                    &AnalysisControl::never(),
                    AnalysisLimits::unlimited().with_limit(kind, calls as u64 - 1),
                );
                assert!(matches!(
                    outcome,
                    ModuleAnalysisOutcome::BudgetExceeded(error) if error.kind() == kind
                ));
            }
            assert!(work[2] - work[1] <= 2 * (work[1] - work[0]), "{work:?}");
            if let Some(&last) = work.get(3) {
                assert!(last <= 2 * work[2], "{work:?}");
            }
        }
    }
}

#[test]
fn inline_map_checks_each_nested_callback_once_under_the_shared_limits() {
    let root = Path::new("/predictable/main.opaal");
    let registry = standard_registry();
    let kind = AnalysisLimitKind::GenericInstantiations;
    let mut work = Vec::new();
    for calls in [4, 8, 12] {
        let nested = (0..calls).fold("1".to_owned(), |body, _| {
            format!("list::map([1], {{|x| {body} + x}})[0]")
        });
        let source = OneModule {
            source: format!("import std::list\n{nested}\n").into_bytes(),
        };
        let loader = ModuleProgramLoader::new(&source, &source);
        let report = loader.analyze_with_commands(root, &registry);
        assert!(report.issues().is_empty(), "{:?}", report.issues());
        assert_eq!(report.usage().get(kind), calls);
        work.push(report.usage().get(AnalysisLimitKind::WorkUnits));
        let outcome = loader.analyze_with_commands_and_limits_controlled(
            root,
            &registry,
            &AnalysisControl::never(),
            AnalysisLimits::unlimited().with_limit(kind, calls - 1),
        );
        assert!(
            matches!(outcome, ModuleAnalysisOutcome::BudgetExceeded(error) if error.kind() == kind)
        );
        let polls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&polls);
        let observed = AnalysisControl::cooperative(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            false
        });
        let observed = loader.analyze_with_commands_and_limits_controlled(
            root,
            &registry,
            &observed,
            AnalysisLimits::OPAAL,
        );
        assert!(matches!(observed, ModuleAnalysisOutcome::Complete(_)));
        let stop = polls.swap(0, Ordering::SeqCst) - 2;
        let counter = Arc::clone(&polls);
        let control =
            AnalysisControl::cooperative(move || counter.fetch_add(1, Ordering::SeqCst) >= stop);
        let outcome = loader.analyze_with_commands_and_limits_controlled(
            root,
            &registry,
            &control,
            AnalysisLimits::OPAAL,
        );
        assert!(matches!(outcome, ModuleAnalysisOutcome::Cancelled));
    }
    assert!(work[2] - work[1] <= 2 * (work[1] - work[0]), "{work:?}");
}

#[test]
fn check_preserves_legacy_deep_list_arms_after_command_for_words() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let registry = standard_registry();
            let root = Path::new("/predictable/main.opaal");
            let arm = format!("{}1{}", "[".repeat(33), "]".repeat(33));
            let else_if = format!("true {{ {arm} }} else if ^true for item in rows && true");
            for condition in [
                "^true for && true",
                "^true for item in rows && true",
                else_if.as_str(),
            ] {
                let input = format!("let rows = [if {condition} {{ {arm} }} else {{ {arm} }}]");
                let file = opaal_syntax::SourceFile::new(
                    opaal_syntax::SourceId::new(1),
                    "/predictable/main.opaal",
                    input,
                );
                let opaal_syntax::FormatOutcome::Complete(formatted) =
                    opaal_syntax::format_source_opaal(&file)
                else {
                    panic!("{condition}");
                };
                let source = OneModule {
                    source: formatted.into_bytes(),
                };
                let report = ModuleProgramLoader::new(&source, &source)
                    .analyze_with_commands(root, &registry);
                assert!(report.issues().is_empty(), "{:?}", report.issues());
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn comprehension_static_diagnostics_include_skipped_operands() {
    let registry = standard_registry();
    for (text, code) in [
        ("let rows = [i for i in 3]", "SIG026"),
        ("let rows = [i for i in [] if 1]", "SIG029"),
        (
            "import std::value\nlet rows = [value::misspelled for i in [] if false]",
            "MOD009",
        ),
        (
            "import std::value\nlet rows = [i for i in [] if value::misspelled]",
            "MOD009",
        ),
        (
            "import std::value\nlet rows = [i for i in value::misspelled]",
            "MOD009",
        ),
        (
            "def get_status() -> Status { ^true }\nlet rows = [i for i in [] if get_status()]",
            "SIG029",
        ),
        (
            "let rows: List[Int] = ['invalid' for i in [] if false]",
            "SIG030",
        ),
        ("let rows = [i for i, (i, _) in []]", "MOD010"),
        ("let rows = [a for (a, _) in [1]]", "SIG017"),
        (
            "def keep[T](rows: List[T]) -> List[T] { return rows }\nkeep([[] for n in []])",
            "SIG010",
        ),
        (
            "import std::value\nvalue::length([[] for n in []])",
            "SIG010",
        ),
        (
            "def item() -> List[Int] { return [1] }\nlet rows = [item() for item in item() if false]",
            "SIG022",
        ),
    ] {
        let source = OneModule {
            source: text.as_bytes().to_vec(),
        };
        let report = ModuleProgramLoader::new(&source, &source)
            .analyze_with_commands(Path::new("/predictable/main.opaal"), &registry);
        let diagnostics: Vec<_> = report
            .issues()
            .iter()
            .flat_map(|issue| issue.error().diagnostics())
            .collect();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == code),
            "{text}: {diagnostics:?}"
        );
    }
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
fn indexed_loops_keep_a_snapshot_and_fresh_bindings() {
    let source = r#"
mut rows = [(3, ('a', true)), (5, ('b', false)), (7, ('c', true))]
let i = 100
let number = 200
mut seen = []
for i, (number, (_, include)) in rows {
    rows = [(1000, ('new', true))]
    if !include { continue }
    seen = [...seen, (i, number)]
}
let empty: List[(Int, String)] = []
for (count, label) in empty { throw "empty loop ran" }
for i, item in 2..2 { throw "empty range ran" }
for i, item in [9] { seen = [...seen, (i, item)] }

def visit(item: Any) -> Int {
    for i, (item, _) in item {
        return i + item
    }
    return -1
}
let rows_with_bad_tail: Any = [(11, false), 99]
for (first, _) in rows_with_bad_tail {
    if first == 11 { break }
    throw "break did not leave the loop"
}
let returned = visit(rows_with_bad_tail)
(i, number, returned, seen)
"#;
    let pair = |index, item| Value::Tuple(Arc::from([Value::Int(index), Value::Int(item)]));
    let expected = Value::Tuple(Arc::from([
        Value::Int(100),
        Value::Int(200),
        Value::Int(11),
        Value::list(vec![pair(0, 3), pair(2, 7), pair(0, 9)]),
    ]));
    let value = checked_value("indexed snapshot, scope, and transfers", source);
    assert_eq!(value, expected);
}

#[test]
fn indexed_ranges_use_positions_and_can_include_the_largest_int() {
    let source = r#"
mut indices = []
mut items = []
for i, item in 9223372036854775806..=9223372036854775807 {
    indices = [...indices, i]
    items = [...items, item]
}
(indices, items)
"#;
    let value = checked_value("range ending at Int::MAX", source);
    assert_eq!(
        value,
        Value::Tuple(Arc::from([
            Value::list(vec![Value::Int(0), Value::Int(1)]),
            Value::list(vec![Value::Int(i64::MAX - 1), Value::Int(i64::MAX)]),
        ]))
    );
}

#[test]
fn loop_bindings_use_current_values_and_resolve_generic_shapes_at_runtime() {
    let source = r#"
def identity[T](value: T) -> T { return value }
mut before = if true { [] } else { [1] }
let previous = identity(before)
before = "changed"

mut rows = [1]
rows = [("ready", 7)]
let snapshot = rows
mut total = 0
for i, (label, count) in [...snapshot] {
    if label == "ready" { total += i + count }
}
rows = ["changed"]
for i, item in rows {
    if item == "changed" { total += i + 1 }
}

def first[T](rows: List[(Int, T)]) -> Int {
    for (_, (left, _)) in rows { return left }
    return 0
}
let nested = first([(0, (10, 20))])
let dynamic: List[Any] = [1, 2]
try {
    for (left, right) in dynamic { throw "mismatched body ran" }
} catch mismatch {
    if mismatch.message != "value does not match the pattern" { throw mismatch }
}
total + nested
"#;
    assert_eq!(checked_value("loop type evidence", source), Value::Int(18));
}

#[test]
fn loop_checking_rejects_known_shapes_and_duplicate_or_mutable_bindings() {
    let registry = standard_registry();
    let root = Path::new("/predictable/main.opaal");
    for (case, text, code) in [
        (
            "typed empty list",
            "let rows: List[Int] = []\nfor (a, b) in rows {}\n",
            "SIG017",
        ),
        (
            "wrong arity in callable",
            "def visit(rows: List[(Int, Int)]) { for i, (a, b, c) in rows {} }\n",
            "SIG017",
        ),
        (
            "nested wrong shape",
            "for (a, (b, c)) in [(1, false)] {}\n",
            "SIG017",
        ),
        ("tuple iterable", "for i, item in (1, 2) {}\n", "SIG026"),
        (
            "index also bound by pattern",
            "for i, (i, _) in [(1, 2)] {}\n",
            "MOD010",
        ),
        (
            "immutable item",
            "for i, item in [1] { item = i }\n",
            "BND001",
        ),
    ] {
        let source = OneModule {
            source: text.as_bytes().to_vec(),
        };
        let report =
            ModuleProgramLoader::new(&source, &source).analyze_with_commands(root, &registry);
        assert!(report.program().is_none(), "{case} must fail checking");
        assert!(
            report
                .issues()
                .iter()
                .flat_map(|issue| issue.error().diagnostics())
                .any(|diagnostic| diagnostic.code() == code),
            "{case}: {:?}",
            report.issues()
        );
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
