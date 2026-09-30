#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use opaal_runtime::help::{ModuleHelpCatalog, ModuleHelpKind};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgram, ModuleProgramError,
    ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::{ScriptExecutionOutcome, execute_module_program_outcome_with_limits};
use opaal_runtime::{Environment, Value};

struct Sources(String);
impl ModuleCanonicalizer for Sources {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(path.to_path_buf())
    }
}
impl ModuleSourceLoader for Sources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(if module.path().file_name().unwrap() == "api.opaal" {
            b"import std::list as lists\nexport { lists, identity, fail, wrong, valid }\ndef identity[X: Equal](x: X) -> X { return x }\ndef fail(x: Int) -> Int { throw 'callback failure' }\nlet wrong = {|x: Int, y: Int| -> Int x}\nlet valid = {|x: Int| -> Int x}\n".to_vec()
        } else {
            self.0.as_bytes().to_vec()
        })
    }
}
struct NoExecutables;
impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("pure callbacks must not resolve executables")
    }
}
fn try_load(source: &str) -> Result<ModuleProgram, ModuleProgramError> {
    let sources = Sources(format!("import std::list as list\n{source}"));
    ModuleProgramLoader::new(&sources, &sources).load(Path::new("/project/main.opaal"))
}
fn load(source: &str) -> ModuleProgram {
    try_load(source).unwrap_or_else(|error| panic!("source must analyze: {error:?}\n{source}"))
}
fn execute(program: &ModuleProgram, limits: EvalLimits) -> ScriptExecutionOutcome {
    execute_module_program_outcome_with_limits(
        program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::full(),
        Arc::new(FakeClock::new()),
        &mut Vec::new(),
        &limits,
    )
}
fn value(source: &str) -> Value {
    let result = execute(
        &load(source),
        EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()),
    );
    match result.primary() {
        PrimaryOutcome::Completed(completion) => completion.value().clone(),
        other => panic!("expected value: {other:?}\n{source}"),
    }
}

#[test]
fn map_filter_fold_reuse_ordinary_bindings_and_preserve_order() {
    assert_eq!(
        value(
            "let amount = 3\nlet transform = {|x: Int| -> Int x + amount}\nlist::map([2, 1, 2], transform)"
        ),
        Value::list(vec![Value::Int(5), Value::Int(4), Value::Int(5)])
    );
    assert_eq!(
        value("list::filter([3, 1, 2, 1], {|x: Int| -> Bool x < 3})"),
        Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(1)])
    );
    assert_eq!(
        value("list::fold([1, 2, 3], 0, {|a: Int, x: Int| -> Int a * 10 + x})"),
        Value::Int(123)
    );
    assert_eq!(
        value(
            "let input = [1, 2]\nlet output = list::map[Int, List[Int]](input, {|x| [x, x]})\n[input, output]"
        ),
        Value::list(vec![
            Value::list(vec![Value::Int(1), Value::Int(2)]),
            Value::list(vec![
                Value::list(vec![Value::Int(1), Value::Int(1)]),
                Value::list(vec![Value::Int(2), Value::Int(2)])
            ])
        ])
    );
}

#[test]
fn empty_calls_preflight_without_invoking_callback() {
    assert_eq!(
        value("def fail(x: Int) -> Int { throw 'must not invoke' }\nlist::map([], fail)"),
        Value::list(vec![])
    );
    assert_eq!(
        value("list::filter([], {|x: Int| -> Bool true})"),
        Value::list(vec![])
    );
    assert_eq!(
        value("list::fold[Int, String]([], 'seed', {|a, x| a})"),
        Value::string("seed")
    );
    assert_eq!(
        value("let result: List[Int] = list::map([], {|x: Int| x})\nresult"),
        Value::list(vec![])
    );
}

#[test]
fn aliases_reexports_and_generic_callbacks_share_descriptor_and_type_instantiation() {
    let source = "import './api.opaal' as api\napi::lists::map[Int, Int]([2, 1], api::identity)";
    assert_eq!(
        value(source),
        Value::list(vec![Value::Int(2), Value::Int(1)])
    );
    assert_eq!(
        value("import './api.opaal' as api\napi::lists::map[Int, Int]([], api::identity)"),
        Value::list(vec![])
    );
    assert_eq!(
        value(
            "def identity[X](x: X) -> X { x }\nlet alias = identity\nlist::map[Int, Int]([4], alias)"
        ),
        Value::list(vec![Value::Int(4)])
    );
    assert_eq!(
        value(
            "def copies[X](items: List[X]) -> List[X] { list::map[X, X](items, {|x: X| -> X x}) }\ncopies[Int]([4, 2])"
        ),
        Value::list(vec![Value::Int(4), Value::Int(2)])
    );
    assert_eq!(
        value(
            "import './api.opaal' as api\nlet callback = api::valid\nlist::map([4, 2], callback)"
        ),
        Value::list(vec![Value::Int(4), Value::Int(2)])
    );
    let program = load(source);
    let root = program.graph().root();
    let direct = program.resolve_operation(root, &["list", "map"]).unwrap();
    let reexport = program
        .resolve_operation(root, &["api", "lists", "map"])
        .unwrap();
    assert_eq!(direct, reexport);
    assert_eq!(direct.validate(), Ok(()));
    assert_eq!(
        direct.signature_labels(),
        ["std::list::map[T, U](input: List[T], transform: Callable(T) -> U) -> List[U]"]
    );
    let help = ModuleHelpCatalog::snapshot(&program)
        .query(root, "api::lists::map")
        .unwrap();
    assert_eq!(help.kind(), ModuleHelpKind::Operation);
    assert_eq!(help.operation().unwrap(), &direct);
}

#[test]
fn known_invalid_shapes_types_generics_and_actions_fail_analysis_even_when_empty() {
    for source in [
        "list::map[Int, Int]([], {|x, y| x})",
        "list::map[Int, Int]([], {|x: String| -> String x})",
        "list::filter[Int]([], {|x: Int| -> Int x})",
        "list::map[Int, String]([], {|x: Int| -> Int x})",
        "list::fold[Int, Int]([], 0, {|a: Int, x: Int| -> String 'bad'})",
        "list::map([], {|x| x})",
        "list::map([1], {|x| x})",
        "list::map[Int]([], {|x| x})",
        "list::map[Int, Int]([])",
        "list::map[Int, Int]([], 4)",
        "let wrong = {|x: Int, y: Int| -> Int x}\nlist::map[Int, Int]([], wrong)",
        "action ready(x: Int) -> Int effects {} { x }\nlist::map[Int, Int]([], ready)",
        "action ready(x: Int) -> Int effects { clock.wall; } { x }\nlist::map[Int, Int]([], ready)",
        "def predicate[X: Ordered](x: X) -> Bool { true }\nlist::filter[Bool]([], predicate)",
        "def extra[X, Y](x: X) -> X { x }\nlist::map[Int, Int]([], extra)",
        "def choose[X](x: X) -> X { x }\nlist::map[Int, String]([], choose)",
        "import './api.opaal' as api\nlist::map[Int, Int]([], api::wrong)",
        "import './api.opaal' as api\nlet callback = api::wrong\nlist::map[Int, Int]([], callback)",
    ] {
        assert!(try_load(source).is_err(), "must reject: {source}");
    }
}

#[test]
fn dynamic_callbacks_are_checked_before_iteration_and_on_each_return() {
    for callback in ["{|x, y| x}", "{|x: String| -> String x}", "7"] {
        let source = format!(
            "def dynamic(x: Any) -> Any {{ x }}\nlist::map[Int, Int]([], dynamic({callback}))"
        );
        assert!(
            matches!(
                execute(
                    &load(&source),
                    EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default())
                )
                .primary(),
                PrimaryOutcome::Error(_)
            ),
            "{source}"
        );
    }
    assert_eq!(
        value(
            "def dynamic(x: Any) -> Any { x }\nlist::map[Int, Int]([1, 2], dynamic({|x: Any| -> Any x}))"
        ),
        Value::list(vec![Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        value(
            "def dynamic(x: Any) -> Any { x }\nlist::filter[Int]([], dynamic({|x| 'unknown until called'}))"
        ),
        Value::list(vec![])
    );
    let source = "def dynamic(x: Any) -> Any { x }\nlist::filter[Int]([1], dynamic({|x| 'wrong'}))";
    assert!(matches!(
        execute(
            &load(source),
            EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default())
        )
        .primary(),
        PrimaryOutcome::Error(_)
    ));
    let source =
        "def dynamic(x: Any) -> Any { x }\nlist::map[Int, Int](dynamic([1, 'wrong']), {|x| x})";
    assert!(matches!(
        execute(
            &load(source),
            EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default())
        )
        .primary(),
        PrimaryOutcome::Error(_)
    ));
}

#[test]
fn result_and_option_values_are_preserved_and_no_unwrapping_is_added() {
    assert_eq!(
        value(
            "import std::outcome as outcome\nlet values = list::map[Int, outcome::Result[Int, String]]([1, 2], {|x: Int| -> outcome::Result[Int, String] outcome::Result::Ok(x)})\nmatch values[1] { outcome::Result::Ok(x) => { x }; outcome::Result::Err(e) => { 0 } }"
        ),
        Value::Int(2)
    );
    assert_eq!(
        value(
            "import std::outcome as outcome\nlist::map[outcome::Option[Int], Int]([outcome::Option::None], {|x: outcome::Option[Int]| -> Int 1})"
        ),
        Value::list(vec![Value::Int(1)])
    );
}

#[test]
fn callback_failure_keeps_defining_source_frame_and_is_catchable() {
    let program = load("import './api.opaal' as api\nlist::map([1, 2], api::fail)");
    let result = execute(
        &program,
        EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()),
    );
    let PrimaryOutcome::Error(error) = result.primary() else {
        panic!("{result:?}");
    };
    assert!(error.render().contains("api.opaal"));
    assert!(error.to_string().contains("callback failure"));
    assert!(error.render().contains("fail"));
    assert_eq!(
        value(
            "import './api.opaal' as api\nmut result = 0\ntry { let ignored = list::map([1, 2], api::fail) } catch error { result = 7 }\nresult"
        ),
        Value::Int(7)
    );
}

#[test]
fn callback_body_cannot_inherit_environment_or_process_authority() {
    for source in [
        "list::map[Int, Any]([1], {|x| env('CALLBACK_TEST')})",
        "list::map[Int, Any]([1], {|x| ^forbidden-program})",
        "list::map[Int, Any]([1], {|x| glob('*')})",
    ] {
        let result = execute(
            &load(source),
            EvalLimits::new(CancellationToken::never(), ResourceBudget::default()),
        );
        assert!(
            !matches!(result.primary(), PrimaryOutcome::Completed(_)),
            "{source}"
        );
    }
}

#[test]
fn nested_callbacks_consume_one_depth_item_and_byte_budget() {
    let source = "list::map[Int, List[Int]]([1, 2], {|x| list::map[Int, Int]([x, x], {|y| y})})";
    let program = load(source);
    let run = |budget| {
        execute(
            &program,
            EvalLimits::pure_opaal(CancellationToken::never(), budget),
        )
    };
    // Input slots: 2 + 2 + 2; output slots: 2 + 2 + 2.
    assert!(matches!(
        run(ResourceBudget::unlimited().with_collection_items(12)).primary(),
        PrimaryOutcome::Completed(_)
    ));
    assert!(matches!(
        run(ResourceBudget::unlimited().with_collection_items(11)).primary(),
        PrimaryOutcome::Error(_)
    ));
    let bytes = (6 * size_of::<Value>()) as u64;
    assert!(matches!(
        run(ResourceBudget::unlimited().with_collection_bytes(bytes)).primary(),
        PrimaryOutcome::Completed(_)
    ));
    assert!(matches!(
        run(ResourceBudget::unlimited().with_collection_bytes(bytes - 1)).primary(),
        PrimaryOutcome::Error(_)
    ));
    assert!(matches!(
        run(ResourceBudget::unlimited().with_call_depth(2)).primary(),
        PrimaryOutcome::Completed(_)
    ));
    assert!(matches!(
        run(ResourceBudget::unlimited().with_call_depth(1)).primary(),
        PrimaryOutcome::Error(_)
    ));
    let steps = (1..200)
        .find(|steps| {
            matches!(
                run(ResourceBudget::steps(*steps)).primary(),
                PrimaryOutcome::Completed(_)
            )
        })
        .unwrap();
    assert!(matches!(
        run(ResourceBudget::steps(steps - 1)).primary(),
        PrimaryOutcome::Error(_)
    ));
}

#[test]
fn cancellation_remains_primary_during_nested_callbacks() {
    let program = load(
        "list::map[Int, List[Int]]([1, 2, 3], {|x| list::map[Int, Int]([x, x], {|y| y + 1})})",
    );
    let polls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&polls);
    let cancel = CancellationToken::from_fn(move || seen.fetch_add(1, Ordering::SeqCst) >= 28);
    let result = execute(
        &program,
        EvalLimits::pure_opaal(cancel, ResourceBudget::default()),
    );
    assert!(
        matches!(result.primary(), PrimaryOutcome::Cancelled(_)),
        "{result:?}"
    );
    assert_eq!(polls.load(Ordering::SeqCst), 29);
}

fn direct(source: &str) -> Result<Value, opaal_runtime::session::SubmitError> {
    let mut session = opaal_runtime::session::Session::new(
        "/project",
        Environment::new(),
        SessionOptions::default(),
    );
    session
        .submit_with_value(
            "<interactive>",
            format!("import std::list as list\n{source}"),
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .map(|(_, value)| value)
}

fn direct_value(source: &str) -> Value {
    direct(source).unwrap()
}

#[test]
fn interactive_runtime_preflight_rejects_both_action_kinds_without_host_refusal() {
    for effects in ["", "clock.wall;"] {
        let source = format!(
            "action ready(x: Int) -> Int effects {{ {effects} }} {{ x }}\nlist::map[Int, Int]([], ready)"
        );
        let error = direct(&source).unwrap_err();
        let opaal_runtime::session::SubmitError::Runtime { error, .. } = error else {
            panic!("expected runtime preflight error");
        };
        assert!(
            matches!(
                error.kind(),
                opaal_runtime::eval::RuntimeErrorKind::Operation(_)
            ),
            "{error}"
        );
        assert!(error.to_string().contains("actions are not accepted"));
    }
}

#[test]
fn interactive_runtime_uses_complete_evidence_and_preserves_empty_any_ambiguity() {
    assert_eq!(
        direct_value("list::map([1, 2], {|x| -> Int x + 1})"),
        Value::list(vec![Value::Int(2), Value::Int(3)])
    );
    assert_eq!(
        direct_value("list::filter([], {|x: Int| -> Bool true})"),
        Value::list(vec![])
    );
    assert_eq!(
        direct_value("def identity[X](x: X) -> X { x }\nlist::map[Int, Int]([], identity)"),
        Value::list(vec![])
    );
    for source in [
        "list::map([], {|x| -> Int 1})",
        "let input: Any = [1]\nlist::map(input, {|x| -> Int x})",
        "def dynamic(x: Any) -> Any { x }\nlist::map(dynamic([1]), {|x| -> Int x})",
        "list::map([1, 'bad'], {|x| -> Int x})",
        "list::map([[1], ['bad']], {|x| -> Int 1})",
    ] {
        assert!(direct(source).is_err(), "{source}");
    }
}

#[test]
fn imported_callback_error_source_and_caller_frame_are_distinct() {
    let source = "import './api.opaal' as api\nmut locations = []\ntry { let ignored = list::map([1], api::fail) } catch error { locations = [error.source.name, error.frames[0].name, error.frames[0].start, error.frames[0].end] }\nlocations";
    let call = "list::map([1], api::fail)";
    let start = "import std::list as list\n".len() + source.find(call).unwrap();
    assert_eq!(
        value(source),
        Value::list(vec![
            Value::string("/project/api.opaal"),
            Value::string("/project/main.opaal"),
            Value::Int(start as i64),
            Value::Int((start + call.len()) as i64)
        ])
    );
}

#[test]
fn generic_forwarding_and_returned_callbacks_keep_concrete_type_evidence() {
    assert_eq!(
        value(
            "def identity[X: Equal](x: X) -> X { x }\ndef copies[X: Equal](items: List[X]) -> List[X] { list::map[X, X](items, identity) }\ndef forward[X: Equal](items: List[X]) -> List[X] { copies[X](items) }\nforward[Int]([4, 2])"
        ),
        Value::list(vec![Value::Int(4), Value::Int(2)])
    );
    assert_eq!(
        value(
            "def make[X](seed: X) -> Closure { {|x: X| -> X seed} }\nlet callback = make[Int](7)\nlist::map[Int, Int]([1, 2], callback)"
        ),
        Value::list(vec![Value::Int(7), Value::Int(7)])
    );
}

#[test]
fn nested_callback_validation_shares_live_call_depth_and_restores_it_after_error() {
    let program = load("list::map[Int, List[List[Int]]]([1], {|x| [[x]]})");
    let run = |depth| {
        execute(
            &program,
            EvalLimits::pure_opaal(
                CancellationToken::never(),
                ResourceBudget::unlimited().with_call_depth(depth),
            ),
        )
    };
    assert!(matches!(run(3).primary(), PrimaryOutcome::Completed(_)));
    assert!(matches!(run(2).primary(), PrimaryOutcome::Error(_)));
    let program = load(
        "try { let ignored = list::map[Int, List[List[Int]]]([1], {|x| [[x]]}) } catch error { null }\nlist::map[Int, Int]([2], {|x| x})",
    );
    let result = execute(
        &program,
        EvalLimits::pure_opaal(
            CancellationToken::never(),
            ResourceBudget::unlimited().with_call_depth(2),
        ),
    );
    assert!(
        matches!(result.primary(), PrimaryOutcome::Completed(completion)
        if completion.value() == &Value::list(vec![Value::Int(2)]))
    );
}

#[test]
fn list_queries_short_circuit_and_keep_exact_empty_identities() {
    for (source, expected) in [
        (
            "def fail(x: Int) -> Bool { throw 'unvisited' }\nlist::any[Int]([], fail)",
            Value::Bool(false),
        ),
        (
            "def fail(x: Int) -> Bool { throw 'unvisited' }\nlist::all[Int]([], fail)",
            Value::Bool(true),
        ),
        (
            "def fail(x: Int) -> Bool { throw 'unvisited' }\nlist::count[Int]([], fail)",
            Value::Int(0),
        ),
        (
            "def stop(x: Int) -> Bool { if x == 2 { return true }; throw 'unvisited' }\nlist::any([2, 3], stop)",
            Value::Bool(true),
        ),
        (
            "def stop(x: Int) -> Bool { if x == 2 { return false }; throw 'unvisited' }\nlist::all([2, 3], stop)",
            Value::Bool(false),
        ),
        (
            "list::count([3, 1, 2, 1], {|x: Int| -> Bool x < 3})",
            Value::Int(3),
        ),
        (
            "list::any([3, 1], {|x: Int| -> Bool false})",
            Value::Bool(false),
        ),
        (
            "list::all([3, 1], {|x: Int| -> Bool true})",
            Value::Bool(true),
        ),
    ] {
        assert_eq!(value(source), expected, "{source}");
    }
    for name in ["any", "all", "count", "find"] {
        for callback in ["{|x: Int| -> Int 1}", "{|x, y| true}", "ready"] {
            let source = format!(
                "action ready(x: Int) -> Bool effects {{}} {{ true }}\nlist::{name}[Int]([], {callback})"
            );
            assert!(try_load(&source).is_err(), "{source}");
        }
        let source = format!(
            "def dynamic(x: Any) -> Any {{ x }}\nlist::{name}[Int]([1], dynamic({{|x| 'wrong'}}))"
        );
        assert!(
            matches!(
                execute(
                    &load(&source),
                    EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default())
                )
                .primary(),
                PrimaryOutcome::Error(_)
            ),
            "{source}"
        );
    }
}

#[test]
fn find_uses_the_standard_option_identity_and_distinguishes_some_null() {
    let found = value(
        "def stop(x: Int) -> Bool { if x == 2 { return true }; throw 'unvisited' }\nlist::find([2, 1, 3], stop)",
    );
    let Value::Variant(found) = found else {
        panic!("expected nominal Option")
    };
    assert_eq!(found.id().module().path(), Path::new("std::outcome"));
    assert_eq!(found.id().name(), "Option");
    assert_eq!(found.constructor(), "Some");
    assert_eq!(found.payload(), &[Value::Int(2)]);
    assert_eq!(
        found.type_arguments().to_vec(),
        vec![opaal_runtime::module::ValueType::Int]
    );
    for source in [
        "list::find[Int]([], {|x| true})",
        "list::find([1, 2], {|x: Int| -> Bool false})",
    ] {
        let Value::Variant(absent) = value(source) else {
            panic!("expected Option")
        };
        assert_eq!(absent.id(), found.id());
        assert_eq!(absent.constructor(), "None");
        assert!(absent.payload().is_empty());
    }
    assert_eq!(
        value(
            "import std::outcome as outcome\nlet found: outcome::Option[Null] = list::find([null], {|x: Null| -> Bool true})\nmatch found { outcome::Option::Some(x) => { x == null }; outcome::Option::None => { false } }"
        ),
        Value::Bool(true)
    );
    assert_eq!(
        value(
            "import std::outcome as outcome\nlet found: outcome::Option[Int] = list::find([], {|x| true})\nmatch found { outcome::Option::Some(x) => { x }; outcome::Option::None => { 9 } }"
        ),
        Value::Int(9)
    );
    assert_eq!(
        direct_value(
            "import std::outcome as outcome\nlet found = list::find[Int]([4], {|x| true})\nmatch found { outcome::Option::Some(x) => { x }; outcome::Option::None => { 0 } }"
        ),
        Value::Int(4)
    );
}

#[test]
fn selection_preserves_values_duplicates_and_input_and_rejects_negative_counts() {
    assert_eq!(
        value(
            "let input = [3, 1, 3]\n[input, list::take(input, 2), list::drop(input, 1), list::reverse(input)]"
        ),
        Value::list(vec![
            Value::list(vec![Value::Int(3), Value::Int(1), Value::Int(3)]),
            Value::list(vec![Value::Int(3), Value::Int(1)]),
            Value::list(vec![Value::Int(1), Value::Int(3)]),
            Value::list(vec![Value::Int(3), Value::Int(1), Value::Int(3)]),
        ])
    );
    for (source, expected) in [
        ("list::take[Int]([], 9223372036854775807)", vec![]),
        ("list::drop[Int]([], 0)", vec![]),
        ("list::reverse[Int]([])", vec![]),
        ("list::take([1, 2], 0)", vec![]),
        (
            "list::take([1, 2], 9223372036854775807)",
            vec![Value::Int(1), Value::Int(2)],
        ),
        ("list::drop([1, 2], 9223372036854775807)", vec![]),
        ("list::drop([1, 2], 0)", vec![Value::Int(1), Value::Int(2)]),
    ] {
        assert_eq!(value(source), Value::list(expected), "{source}");
    }
    for name in ["take", "drop"] {
        assert_eq!(
            value(&format!(
                "mut caught = false\ntry {{ list::{name}[Int]([], -1) }} catch error {{ caught = true }}\ncaught"
            )),
            Value::Bool(true)
        );
        assert!(try_load(&format!("list::{name}([1], '2')")).is_err());
        let source = format!(
            "def dynamic(x: Any) -> Any {{ x }}\nlist::{name}[Int](dynamic([1, 'wrong']), 0)"
        );
        assert!(matches!(
            execute(
                &load(&source),
                EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default())
            )
            .primary(),
            PrimaryOutcome::Error(_)
        ));
    }
}

#[test]
fn natural_and_keyed_sort_are_stable_and_validate_the_ordered_relation() {
    assert_eq!(
        value("list::sort([3, 1, 2, 1])"),
        Value::list(vec![
            Value::Int(1),
            Value::Int(1),
            Value::Int(2),
            Value::Int(3)
        ])
    );
    assert_eq!(
        value("list::sort[List[Int]]([[1, 2], [], [1], [0], [1, 1]])"),
        Value::list(vec![
            Value::list(vec![]),
            Value::list(vec![Value::Int(0)]),
            Value::list(vec![Value::Int(1)]),
            Value::list(vec![Value::Int(1), Value::Int(1)]),
            Value::list(vec![Value::Int(1), Value::Int(2)])
        ])
    );
    assert_eq!(
        value(
            "let items = [{ k: 2, id: 'a' }, { k: 1, id: 'b' }, { k: 2, id: 'c' }]\nlet sorted = list::sort_by[Record, Int](items, {|x: Record| -> Int x.k})\nlist::map[Record, String](sorted, {|x: Record| -> String x.id})"
        ),
        Value::list(vec![
            Value::string("b"),
            Value::string("a"),
            Value::string("c")
        ])
    );
    assert_eq!(
        value(
            "def ordered[X: Ordered](items: List[X]) -> List[X] { list::sort[X](items) }\nordered[Int]([2, 1])"
        ),
        Value::list(vec![Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        value(
            "def fail(x: Int) -> String { throw 'unvisited' }\nlist::sort_by[Int, String]([], fail)"
        ),
        Value::list(vec![])
    );
    for source in [
        "list::sort[Bool]([])",
        "list::sort[Any]([])",
        "list::sort[Record]([])",
        "list::sort[List[Bool]]([])",
        "list::sort([])",
        "list::sort([1, 1.0])",
        "list::sort_by[Int, Bool]([], {|x| true})",
        "list::sort_by[Int, Int]([], {|x, y| 0})",
        "def unordered[X](items: List[X]) -> List[X] { list::sort[X](items) }",
    ] {
        assert!(try_load(source).is_err(), "{source}");
    }
    for source in [
        "def dynamic(x: Any) -> Any { x }\nlist::sort[Int](dynamic(['wrong']))",
        "def dynamic(x: Any) -> Any { x }\ndef key(x: Int) -> Any { if x == 1 { return 1 }; return 1.0 }\nlist::sort_by[Int, Int]([1, 2], dynamic(key))",
    ] {
        assert!(
            matches!(
                execute(
                    &load(source),
                    EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default())
                )
                .primary(),
                PrimaryOutcome::Error(_)
            ),
            "{source}"
        );
    }
}

#[test]
fn list_observers_share_option_results_and_ordered_constraints() {
    let program = load("list::sort_by[Int, Int]([2, 1], {|x| x})");
    for name in [
        "any", "all", "count", "find", "sort", "sort_by", "take", "drop", "reverse",
    ] {
        let operation = program
            .resolve_operation(program.graph().root(), &["list", name])
            .unwrap();
        assert_eq!(operation.validate(), Ok(()));
        let help = ModuleHelpCatalog::snapshot(&program)
            .query(program.graph().root(), &format!("list::{name}"))
            .unwrap();
        assert_eq!(help.operation(), Some(&operation));
        if matches!(name, "sort" | "sort_by") {
            assert!(operation.signature_labels()[0].contains(": Ordered"));
        }
        if name == "find" {
            assert!(
                operation.signature_labels()[0].ends_with("Option[T]"),
                "{:?}",
                operation.signature_labels()
            );
        }
    }
}

#[test]
fn interactive_list_selection_and_order_require_complete_type_evidence() {
    for source in [
        "list::sort([2, 1])",
        "list::reverse([2, 1])",
        "list::take([1, 2], 2)",
        "list::drop([9, 1, 2], 1)",
    ] {
        assert_eq!(
            direct_value(source),
            Value::list(vec![Value::Int(1), Value::Int(2)]),
            "{source}"
        );
    }
    for source in [
        "list::sort([])",
        "list::sort[Bool]([])",
        "list::reverse([])",
        "def dynamic(x: Any) -> Any { x }\nlist::sort(dynamic([2, 1]))",
    ] {
        assert!(direct(source).is_err(), "{source}");
    }
    assert_eq!(
        direct_value("let items: List[Int] = []\nlist::sort(items)"),
        Value::list(vec![])
    );
}

#[test]
fn query_and_key_callbacks_propagate_errors_in_source_order_and_share_steps() {
    for (name, signature, body) in [
        ("any", "Int", "false"),
        ("all", "Int", "true"),
        ("count", "Int", "false"),
        ("find", "Int", "false"),
        ("sort_by", "Int, Int", "x"),
    ] {
        let result_type = if name == "sort_by" { "Int" } else { "Bool" };
        let source = format!(
            "def key(x: Int) -> {result_type} {{ if x == 2 {{ throw 'second item' }}; if x == 1 {{ throw 'third item' }}; return {body} }}\nlist::{name}[{signature}]([3, 2, 1], key)"
        );
        let result = execute(
            &load(&source),
            EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()),
        );
        let PrimaryOutcome::Error(error) = result.primary() else {
            panic!("{result:?}")
        };
        assert!(error.to_string().contains("second item"), "{error}");
    }
    let first = load("list::any([1, 2, 3], {|x: Int| -> Bool x == 1})");
    let last = load("list::any([1, 2, 3], {|x: Int| -> Bool x == 3})");
    let minimum = |program: &ModuleProgram| {
        (1..200)
            .find(|steps| {
                matches!(
                    execute(
                        program,
                        EvalLimits::pure_opaal(
                            CancellationToken::never(),
                            ResourceBudget::steps(*steps)
                        )
                    )
                    .primary(),
                    PrimaryOutcome::Completed(_)
                )
            })
            .unwrap()
    };
    assert!(minimum(&first) < minimum(&last));
    for name in ["any", "all", "count", "find", "sort_by"] {
        let result = if name == "sort_by" {
            "x"
        } else if name == "all" {
            "true"
        } else {
            "false"
        };
        let types = if name == "sort_by" { "Int, Int" } else { "Int" };
        let items = (1..=30)
            .map(|item| item.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let program = load(&format!(
            "list::{name}[{types}]([{items}], {{|x| {result}}})"
        ));
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&polls);
        let token = CancellationToken::from_fn(move || seen.fetch_add(1, Ordering::SeqCst) >= 20);
        let outcome = execute(
            &program,
            EvalLimits::pure_opaal(token, ResourceBudget::default()),
        );
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Cancelled(_)),
            "{name}: {outcome:?}"
        );
    }
}

#[test]
fn find_payload_uses_one_retained_slot_and_failed_limits_do_not_return_an_option() {
    let program = load("list::find[Int]([1], {|x| true})");
    // One input item and one Some payload item. Existing literal list slots
    // retain their released byte accounting; the new payload charges its slot.
    let bytes = size_of::<Value>() as u64;
    assert!(matches!(
        execute(
            &program,
            EvalLimits::pure_opaal(
                CancellationToken::never(),
                ResourceBudget::unlimited()
                    .with_collection_items(2)
                    .with_collection_bytes(bytes)
            )
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));
    for budget in [
        ResourceBudget::unlimited().with_collection_items(1),
        ResourceBudget::unlimited().with_collection_bytes(bytes - 1),
    ] {
        assert!(matches!(
            execute(
                &program,
                EvalLimits::pure_opaal(CancellationToken::never(), budget)
            )
            .primary(),
            PrimaryOutcome::Error(_)
        ));
    }
    assert_eq!(
        direct_value("import std::value as value\nvalue::length([])"),
        Value::Int(0)
    );
}

#[test]
fn interactive_option_identity_survives_separate_submissions_and_construction() {
    let mut session = opaal_runtime::session::Session::new(
        "/project",
        Environment::new(),
        SessionOptions::default(),
    );
    for (index, (source, expected)) in [
        (
            "import std::list as list\nimport std::outcome as result\nlet found = list::find[Null]([null], {|x| true})\nlet absent = list::find[Null]([], {|x| true})\ntrue",
            Value::Bool(true),
        ),
        (
            "let copied: result::Option[Null] = found\nmatch copied { result::Option::Some(x) => { x == null }; result::Option::None => { false } }",
            Value::Bool(true),
        ),
        (
            "let made: result::Option[Null] = result::Option::Some(null)\nmade == found && absent != made",
            Value::Bool(true),
        ),
        (
            "let none: result::Option[Null] = result::Option::None\nnone == absent",
            Value::Bool(true),
        ),
        (
            "def unwrap(value: result::Option[Null]) -> Bool { match value { result::Option::Some(x) => { x == null }; result::Option::None => { false } } }\nunwrap(found)",
            Value::Bool(true),
        ),
        ("unwrap(absent)", Value::Bool(false)),
    ]
    .into_iter()
    .enumerate()
    {
        let (_, actual) = session
            .submit_with_value(
                format!("cell-{index}"),
                source,
                &NoExecutables,
                &FakePlatform::full(),
                &FakeClock::new(),
                &mut Vec::new(),
            )
            .unwrap_or_else(|error| panic!("{source}: {}", error.render()));
        assert_eq!(actual, expected, "{source}");
    }
}

#[test]
fn interactive_list_composition_uses_instantiated_result_evidence() {
    for source in [
        "def identity[X](x: X) -> X { x }\nlist::sort(identity[List[Int]]([2, 1]))",
        "def identity[X](x: X) -> X { x }\nlist::sort(identity([2, 1]))",
        "list::sort(list::reverse([1, 2]))",
        "list::sort([[2, 1]][0])",
        "let items: List[List[Int]] = [[2, 1]]\nlist::sort(items[0])",
        "list::sort((list::reverse([1, 2])))",
        "let result: List[Int] = list::sort(list::reverse[Int]([]))\nresult",
        "def empty[X](x: X) -> List[X] { [] }\nlist::reverse(empty[Int](0))",
        "list::map(list::reverse([1, 2]), {|x: Int| -> Int x})",
        "def order[X: Ordered](items: List[X]) -> List[X] { let copy: List[X] = items; list::sort(copy) }\norder[Int]([2, 1])",
    ] {
        assert_eq!(direct_value(source), value(source), "{source}");
    }
    for source in [
        "def dynamic(x: Any) -> Any { x }\nlist::sort(dynamic([2, 1]))",
        "def dynamic(x: Any) -> Any { x }\nlist::sort((dynamic([2, 1])))",
        "def dynamic(x: Any) -> Any { x }\nlist::sort(dynamic([[2, 1]])[0])",
        "let input: Any = [[2, 1]]\nlist::sort(input[0])",
        "let row: Record = { items: [2, 1] }\nlist::sort(row.items)",
        "def dynamic(x: Any) -> Any { x }\nlist::sort([dynamic(1)])",
        "def identity[X](x: X) -> X { x }\nlist::sort(identity[Any]([2, 1]))",
        "list::sort(list::reverse[Any]([1, 2]))",
        "list::sort(list::reverse([]))",
        "list::sort[Bool](list::reverse[Bool]([]))",
    ] {
        assert!(direct(source).is_err(), "{source}");
        assert!(try_load(source).is_err(), "{source}");
    }
}

#[test]
fn keyed_sort_charges_keys_workspace_and_output_in_one_caller_budget() {
    let program = load("list::sort_by[Int, Int]([3, 1, 2, 1], {|x| x})");
    // Four released literal slots, four key slots and four result slots. The
    // new operation owns two Value buffers and two index buffers.
    let bytes = (4 * (2 * size_of::<Value>() + 2 * size_of::<usize>())) as u64;
    let run = |items, byte_limit| {
        execute(
            &program,
            EvalLimits::pure_opaal(
                CancellationToken::never(),
                ResourceBudget::unlimited()
                    .with_collection_items(items)
                    .with_collection_bytes(byte_limit),
            ),
        )
    };
    assert!(
        matches!(run(12, bytes).primary(), PrimaryOutcome::Completed(completion)
        if completion.value() == &Value::list(vec![Value::Int(1), Value::Int(1), Value::Int(2), Value::Int(3)]))
    );
    for outcome in [run(11, bytes), run(12, bytes - 1)] {
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Error(error)
            if error.to_string().contains("resource budget")),
            "{outcome:?}"
        );
    }
}
