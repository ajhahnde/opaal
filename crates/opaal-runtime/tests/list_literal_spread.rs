#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{
    CancellationToken, Completion, EvalLimits, FakeClock, ResourceBudget, RuntimeErrorKind,
    evaluate, evaluate_with_limits,
};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgram, ModuleProgramLoader,
    ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::execute_module_program_outcome_with_limits;
use opaal_runtime::{BindingMutability, Environment, NativePath, ScopeStack, Value};
use opaal_syntax::{ParseOutcome, SourceFile, SourceId, parse_opaal};

struct Sources(String);
impl ModuleCanonicalizer for Sources {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(path.to_path_buf())
    }
}
impl ModuleSourceLoader for Sources {
    fn load(&self, _: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(self.0.as_bytes().to_vec())
    }
}
struct NoExecutables;
impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("list literals must not resolve executables")
    }
}
fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(71), "spread.opaal", text)
}
fn pure(text: &str, scope: &mut ScopeStack) -> Value {
    let source = source(text);
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("must parse: {text}");
    };
    evaluate(&script, &source, scope).unwrap()
}
fn load(text: &str) -> ModuleProgram {
    let sources = Sources(format!("import std::list\n{text}"));
    ModuleProgramLoader::new(&sources, &sources)
        .load(Path::new("/project/main.opaal"))
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}
fn value(text: &str) -> Value {
    let outcome = execute_module_program_outcome_with_limits(
        &load(text),
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::full(),
        Arc::new(FakeClock::new()),
        &mut Vec::new(),
        &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()),
    );
    let PrimaryOutcome::Completed(completion) = outcome.primary() else {
        panic!("{text}: {outcome:?}");
    };
    completion.value().clone()
}

#[test]
fn empty_multiple_interleaved_and_nested_spreads_expand_one_level() {
    for (text, expected) in [
        ("[...[]]", Value::list(vec![])),
        ("[...[], ...[]]", Value::list(vec![])),
        (
            "[...[], ...[1, 2], 3, ...[[4]]]",
            Value::list(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
                Value::list(vec![Value::Int(4)]),
            ]),
        ),
        (
            "[...[1, 'two'], false]",
            Value::list(vec![
                Value::Int(1),
                Value::string("two"),
                Value::Bool(false),
            ]),
        ),
    ] {
        assert_eq!(pure(text, &mut ScopeStack::new()), expected);
        assert_eq!(value(text), expected);
    }
}

#[test]
fn both_evaluation_paths_preserve_generic_expected_empty_and_declared_evidence() {
    for text in [
        "list::reverse([...[], 1, ...[2]])",
        "let empty: List[Int] = []\nlist::reverse([...empty, 1, 2])",
        "let empty: List[Int] = []\nlet result: List[Int] = list::reverse([...empty, 1, 2])\nresult",
        "def copy[T](items: List[T]) -> List[T] { return [...items] }\nlist::reverse(copy[Int]([1, 2]))",
        "list::reverse[Int]([...[], 1, 2])",
    ] {
        assert_eq!(
            value(text),
            Value::list(vec![Value::Int(2), Value::Int(1)]),
            "{text}"
        );
    }
    for text in [
        "let empty: List[Int] = []\nlist::reverse([...empty])",
        "let result: List[Int] = list::reverse([...[]])\nresult",
        "list::reverse[Int]([...[]])",
    ] {
        assert_eq!(value(text), Value::list(vec![]), "{text}");
    }
    for text in [
        "let dynamic: Any = [1, 2]\nlist::reverse([...dynamic])",
        "let dynamic: Any = []\nlist::reverse([...dynamic])",
        "list::reverse([...[]])",
        "list::reverse([...[1, 'two']])",
    ] {
        let sources = Sources(format!("import std::list\n{text}"));
        assert!(
            ModuleProgramLoader::new(&sources, &sources)
                .load(Path::new("/project/main.opaal"))
                .is_err(),
            "must not invent type evidence: {text}"
        );
    }
    assert_eq!(
        value("let dynamic: Any = [1, 2]\nlist::reverse[Int]([...dynamic])"),
        Value::list(vec![Value::Int(2), Value::Int(1)])
    );
}

#[test]
fn non_list_operands_fail_at_the_operand_and_do_not_publish_the_binding() {
    for text in ["let result = [...7]", "let result = [...7, ...false]"] {
        let sources = Sources(text.into());
        let report = ModuleProgramLoader::new(&sources, &sources)
            .analyze_with_commands(Path::new("/project/main.opaal"), &standard_registry());
        assert!(report.program().is_none());
        let diagnostics = report
            .issues()
            .iter()
            .flat_map(|issue| issue.error().diagnostics())
            .collect::<Vec<_>>();
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code() == "SIG020"),
            "{diagnostics:?}"
        );
        assert_eq!(
            diagnostics.len(),
            if text.contains("false") { 2 } else { 1 }
        );
    }
    let source = source("let result = [...bad, missing()]");
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("must parse")
    };
    let mut scope = ScopeStack::new();
    scope
        .declare("bad", BindingMutability::Immutable, Value::Int(7))
        .unwrap();
    let error = evaluate(&script, &source, &mut scope).unwrap_err();
    assert_eq!(
        error.kind(),
        &RuntimeErrorKind::SpreadValueNotList { actual: "int" }
    );
    assert_eq!(source.slice(error.span()).unwrap(), "bad");
    assert!(scope.get("result").is_none());
}

#[test]
fn throw_from_spread_preserves_original_error_and_stops_later_work() {
    let text = "def fail() { throw 'original' }\ntry { [...fail(), missing()] } catch error { error.message }";
    assert_eq!(
        pure(text, &mut ScopeStack::new()),
        Value::string("original")
    );
}

#[test]
fn nested_values_keep_arc_identity_and_native_values_are_not_encoded() {
    let nested = Arc::from([Value::Int(1)]);
    let shared = Value::List(Arc::clone(&nested));
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        Value::Path(NativePath::new(std::ffi::OsString::from_vec(vec![
            b'n', 0xff,
        ])))
    };
    #[cfg(not(unix))]
    let path = Value::Path(NativePath::new("native-path"));
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "input",
            BindingMutability::Immutable,
            Value::list(vec![shared, path.clone()]),
        )
        .unwrap();
    let result = pure("[...input]", &mut scope);
    let values = result.as_list().unwrap();
    let Value::List(copied) = &values[0] else {
        panic!("nested list must survive")
    };
    assert!(Arc::ptr_eq(&nested, copied));
    assert_eq!(values[1], path);
}

#[test]
fn output_items_and_both_allocations_have_exact_budget_boundaries() {
    let source = source("let result = [...input]");
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("must parse")
    };
    for length in [0, 1, 2, 3, 17, 1024] {
        let input = Value::list(vec![Value::Int(1); length]);
        let run = |items, bytes| {
            let mut scope = ScopeStack::new();
            scope
                .declare("input", BindingMutability::Immutable, input.clone())
                .unwrap();
            let result = evaluate_with_limits(
                &script,
                &source,
                &mut scope,
                &EvalLimits::pure_opaal(
                    CancellationToken::never(),
                    ResourceBudget::default()
                        .with_collection_items(items)
                        .with_collection_bytes(bytes),
                ),
            );
            if result.is_err() {
                assert!(scope.get("result").is_none());
            }
            result
        };
        // Geometric Vec growth plus the separately allocated Arc result/header.
        let capacity = if length == 0 {
            0
        } else {
            length.next_power_of_two()
        };
        let bytes = ((capacity + length) * size_of::<Value>() + 2 * size_of::<usize>()) as u64;
        assert!(run(length as u64, bytes).is_ok(), "length {length}");
        if length > 0 {
            assert!(run(length as u64 - 1, bytes).is_err());
        }
        assert!(run(length as u64, bytes - 1).is_err());
    }
}

#[test]
fn huge_spread_consumes_work_and_can_cancel_during_the_copy() {
    let source = source("let result = [...input, missing()]");
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("must parse")
    };
    for cancel_at in [0, 1, 2, 10, 100, 999] {
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&polls);
        let mut scope = ScopeStack::new();
        scope
            .declare(
                "input",
                BindingMutability::Immutable,
                Value::list(vec![Value::Int(1); 100_000]),
            )
            .unwrap();
        let error = evaluate_with_limits(
            &script,
            &source,
            &mut scope,
            &EvalLimits::pure_opaal(
                CancellationToken::from_fn(move || {
                    seen.fetch_add(1, Ordering::SeqCst) >= cancel_at
                }),
                ResourceBudget::default(),
            ),
        )
        .unwrap();
        assert!(matches!(error, Completion::Cancelled(_)), "{error:?}");
        assert!(scope.get("result").is_none());
    }
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "input",
            BindingMutability::Immutable,
            Value::list(vec![Value::Int(1); 100_000]),
        )
        .unwrap();
    let error = evaluate_with_limits(
        &script,
        &source,
        &mut scope,
        &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::steps(100)),
    )
    .unwrap_err();
    assert_eq!(error.kind(), &RuntimeErrorKind::ResourceBudgetExceeded);
}
