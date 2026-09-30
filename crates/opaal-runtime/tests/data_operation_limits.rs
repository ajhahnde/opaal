#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgram, ModuleProgramLoader,
    ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::{ScriptExecutionOutcome, execute_module_program_outcome_with_limits};
use opaal_runtime::{Environment, Value};

struct Source(String);

impl ModuleCanonicalizer for Source {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        (path == Path::new("/project/main.opaal"))
            .then(|| path.to_path_buf())
            .ok_or_else(|| ModulePathError::new("unexpected local import"))
    }
}

impl ModuleSourceLoader for Source {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        assert_eq!(module.path(), Path::new("/project/main.opaal"));
        Ok(self.0.as_bytes().to_vec())
    }
}

struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("resource tests must not resolve executables")
    }
}

fn load(body: &str) -> ModuleProgram {
    let source = Source(format!(
        "import std::string as string\nimport std::list as list\nimport std::record as record\nimport std::data as data\n{body}"
    ));
    ModuleProgramLoader::new(&source, &source)
        .load(Path::new("/project/main.opaal"))
        .unwrap_or_else(|error| panic!("source must analyze: {error:?}\n{}", source.0))
}

fn run(
    program: &ModuleProgram,
    budget: ResourceBudget,
    cancel: CancellationToken,
) -> ScriptExecutionOutcome {
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
        &EvalLimits::pure_opaal(cancel, budget),
    )
}

fn completed(outcome: &ScriptExecutionOutcome) -> Value {
    let PrimaryOutcome::Completed(completion) = outcome.primary() else {
        panic!("expected complete value: {outcome:?}");
    };
    completion.value().clone()
}

// Discover an admission boundary through the public execution API, then require
// the exact budget and one extra unit to preserve the generous value. Every
// smaller probe must be a resource error rather than a partial collection.
fn check_boundary(program: &ModuleProgram, budget: impl Fn(u64) -> ResourceBudget) {
    let expected = completed(&run(
        program,
        ResourceBudget::default(),
        CancellationToken::never(),
    ));
    let succeeds = |limit| {
        let outcome = run(program, budget(limit), CancellationToken::never());
        match outcome.primary() {
            PrimaryOutcome::Completed(completion) => {
                assert_eq!(completion.value(), &expected);
                true
            }
            PrimaryOutcome::Error(error) => {
                assert!(error.to_string().contains("resource budget"), "{outcome:?}");
                false
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    };
    let mut upper = 1;
    while !succeeds(upper) {
        upper *= 2;
        assert!(upper <= 1_048_576, "fixture exceeded its test bound");
    }
    let mut lower = 0;
    while lower < upper {
        let mid = lower + (upper - lower) / 2;
        if succeeds(mid) {
            upper = mid;
        } else {
            lower = mid + 1;
        }
    }
    assert!(lower > 0);
    assert!(!succeeds(lower - 1));
    assert!(succeeds(lower));
    assert!(succeeds(lower + 1));
}

#[test]
fn composed_operations_admit_exact_shared_work_item_and_byte_budgets() {
    for source in [
        "string::join(string::split(string::replace('a/α/a', 'a', '😀😀'), '/'), 'é')",
        "list::map[Int, List[Int]]([1, 2, 3], {|x| list::reverse[Int]([x, x])})",
        "list::sort_by[Int, List[Int]]([3, 1, 2, 1], {|x| [x, -x]})",
        "list::drop(list::reverse(list::sort([3, 1, 2, 1])), 1)",
        "let row = { a: 1, b: 2, c: null }\nrecord::merge(record::select(row, ['b', 'a']), record::set(row, 'd', 4))",
        "data::json_decode(data::json_encode([{a: [1, 2, 3]}, null]))",
        "list::find([1, 2, 3], {|x: Int| -> Bool x == 2})",
    ] {
        let program = load(source);
        check_boundary(&program, ResourceBudget::steps);
        check_boundary(&program, |n| {
            ResourceBudget::default().with_collection_items(n)
        });
        check_boundary(&program, |n| {
            ResourceBudget::default().with_collection_bytes(n)
        });
    }
}

#[test]
fn json_and_nested_callbacks_use_the_callers_depth_budget() {
    for source in [
        "data::json_decode(data::json_encode([[[0]]]))",
        "list::map[Int, List[Int]]([1], {|x| list::map[Int, Int]([x], {|y| y})})",
        "list::sort_by[Int, List[Int]]([2, 1], {|x| [x, x]})",
    ] {
        check_boundary(&load(source), |n| {
            ResourceBudget::default().with_call_depth(n)
        });
    }
}

#[test]
fn cancellation_at_every_observed_poll_remains_primary_through_try_catch() {
    for body in [
        "string::replace('aaaaaaaaaaaaaaaa', 'a', 'α😀α😀')",
        "string::join(string::split('α/β//γ/', '/'), '😀')",
        "list::map[Int, List[Int]]([3, 1, 2, 1], {|x| list::map[Int, Int]([x, x], {|y| y + 1})})",
        "list::sort_by[Int, List[Int]]([3, 1, 2, 1], {|x| [x, -x]})",
        "list::sort(list::reverse([3, 1, 2, 1]))",
        "list::drop(list::take([3, 1, 2, 1], 3), 1)",
        "list::find([3, 1, 2, 1], {|x: Int| -> Bool x == 2})",
        "[list::any([3, 1, 2], {|x: Int| -> Bool x == 1}), list::all([3, 1, 2], {|x: Int| -> Bool x != 1}), list::count([3, 1, 2], {|x: Int| -> Bool x == 1})]",
        "record::merge({ a: 1, b: 2, c: 3 }, { c: 4, b: 5, d: 6 })",
        "data::json_decode(data::json_encode([{a: [1, 2, 3]}, null]))",
    ] {
        let program = load(&format!("try {{ {body} }} catch error {{ 'caught' }}"));
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&polls);
        completed(&run(
            &program,
            ResourceBudget::default(),
            CancellationToken::from_fn(move || {
                seen.fetch_add(1, Ordering::SeqCst);
                false
            }),
        ));
        let count = polls.load(Ordering::SeqCst);
        assert!(count > 0);
        for stop in 0..count {
            let polls = Arc::new(AtomicUsize::new(0));
            let seen = Arc::clone(&polls);
            let outcome = run(
                &program,
                ResourceBudget::default(),
                CancellationToken::from_fn(move || seen.fetch_add(1, Ordering::SeqCst) >= stop),
            );
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Cancelled(_)),
                "{body}, poll {stop}/{count}: {outcome:?}"
            );
            assert!(polls.load(Ordering::SeqCst) > stop);
        }
    }
}

#[test]
fn merging_many_distinct_or_overlapping_keys_has_bounded_key_work() {
    for size in [16usize, 64, 256] {
        for overlap in [false, true] {
            let left = (0..size)
                .map(|i| format!("'key-{i:05}': {i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let right = (0..size)
                .rev()
                .map(|i| format!("'key-{:05}': null", if overlap { i } else { i + size }))
                .collect::<Vec<_>>()
                .join(", ");
            let program = load(&format!("record::merge({{ {left} }}, {{ {right} }})"));
            let n = 2 * size;
            let bound = (8 * n * n.ilog2() as usize + 24 * n) as u64;
            let value = completed(&run(
                &program,
                ResourceBudget::steps(bound),
                CancellationToken::never(),
            ));
            let Value::Record(record) = value else {
                panic!("merge must return a record")
            };
            assert_eq!(record.entries().len(), if overlap { size } else { n });
            for (i, (key, value)) in record.entries().iter().enumerate() {
                if i < size {
                    assert_eq!(key.as_ref(), format!("key-{i:05}"));
                    assert_eq!(
                        value,
                        &if overlap {
                            Value::Null
                        } else {
                            Value::Int(i as i64)
                        }
                    );
                } else {
                    assert_eq!(key.as_ref(), format!("key-{:05}", 3 * size - i - 1));
                    assert_eq!(value, &Value::Null);
                }
            }
            let low = run(
                &program,
                ResourceBudget::steps(n as u64),
                CancellationToken::never(),
            );
            assert!(
                matches!(low.primary(), PrimaryOutcome::Error(error) if error.to_string().contains("resource budget"))
            );
        }
    }
}
