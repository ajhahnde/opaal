#![forbid(unsafe_code)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{
    CancelReason, CancellationToken, Completion, EvalLimits, FakeClock, ResourceBudget,
    RuntimeErrorKind, apply_callable, evaluate_in_environment,
};
use opaal_runtime::module::{
    AnalysisControl, AnalysisLimitKind, AnalysisLimits, ModuleAnalysisOutcome, ModuleCanonicalizer,
    ModuleId, ModulePathError, ModuleProgram, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::execute_module_program_outcome_with_limits;
use opaal_runtime::stream::{AdmittedStreamError, StreamPull, ValueStream};
use opaal_runtime::{AdmittedValue, BindingMutability, Environment, ScopeError, ScopeStack, Value};
use opaal_syntax::{ParseOutcome, SourceFile, SourceId, parse_opaal};

#[derive(Default)]
struct Sources {
    canonical: BTreeMap<PathBuf, PathBuf>,
    source: BTreeMap<PathBuf, Vec<u8>>,
}

impl Sources {
    fn maps(mut self, requested: &str, canonical: &str) -> Self {
        self.canonical
            .insert(PathBuf::from(requested), PathBuf::from(canonical));
        self
    }

    fn contains(mut self, path: &str, source: &str) -> Self {
        self.source
            .insert(PathBuf::from(path), source.as_bytes().to_vec());
        self
    }
}

impl ModuleCanonicalizer for Sources {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.canonical
            .get(candidate)
            .cloned()
            .ok_or_else(|| ModulePathError::new(format!("unmapped path {}", candidate.display())))
    }
}

impl ModuleSourceLoader for Sources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.source
            .get(module.path())
            .cloned()
            .ok_or_else(|| ModuleSourceError::new("unmapped source"))
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        let source = self
            .source
            .get(module.path())
            .ok_or_else(|| ModuleSourceError::new("unmapped source"))?;
        Ok(source[..source.len().min(maximum)].to_vec())
    }
}

struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}

fn resource_sources(root_source: &str) -> Sources {
    Sources::default()
        .maps("/project/main.opaal", "/project/main.opaal")
        .maps("/project/./support.opaal", "/project/support.opaal")
        .maps("/project/support.opaal", "/project/support.opaal")
        .contains("/project/main.opaal", root_source)
        .contains(
            "/project/support.opaal",
            concat!(
                "\n",
                "def identity[T: Equal](value: T) -> T {\n",
                "    return value\n",
                "}\n\n",
                "export { identity }\n",
            ),
        )
}

fn analysis_source() -> &'static str {
    concat!(
        "\n",
        "import './support.opaal' as support\n",
        "import std::value as value\n\n",
        "let chosen: List[List[Int]] = [[support::identity[Int](1)]]\n",
        "value::length(chosen)\n",
    )
}

fn loader(sources: &Sources) -> ModuleProgramLoader<'_> {
    ModuleProgramLoader::new(sources, sources)
}

struct BoundedSource {
    bytes: Vec<u8>,
    requested: Cell<Option<usize>>,
}

impl ModuleCanonicalizer for BoundedSource {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(candidate.to_path_buf())
    }
}

impl ModuleSourceLoader for BoundedSource {
    fn load(&self, _module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        panic!("bounded analysis must not call the unbounded loader")
    }

    fn load_bounded(
        &self,
        _module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        self.requested.set(Some(maximum));
        Ok(self.bytes[..self.bytes.len().min(maximum)].to_vec())
    }
}

#[test]
fn source_limit_requests_only_the_ceiling_plus_one_byte() {
    let source = BoundedSource {
        bytes: b"let value = 1\n".to_vec(),
        requested: Cell::new(None),
    };
    let outcome = ModuleProgramLoader::new(&source, &source).analyze_with_limits_controlled(
        Path::new("/bounded.opaal"),
        &AnalysisControl::never(),
        AnalysisLimits::unlimited().with_limit(AnalysisLimitKind::SourceBytes, 3),
    );
    assert_eq!(source.requested.get(), Some(4));
    assert!(matches!(
        outcome,
        ModuleAnalysisOutcome::BudgetExceeded(exceeded)
            if exceeded.kind() == AnalysisLimitKind::SourceBytes && exceeded.limit() == 3
    ));
}

#[test]
fn every_analysis_counter_accepts_the_exact_boundary_and_refuses_first_excess() {
    let sources = resource_sources(analysis_source());
    let commands = standard_registry();
    let baseline = loader(&sources).analyze_with_commands_and_limits(
        Path::new("/project/main.opaal"),
        &commands,
        AnalysisLimits::unlimited(),
    );
    assert!(baseline.program().is_some(), "{:?}", baseline.issues());
    assert_eq!(baseline.usage().get(AnalysisLimitKind::Modules), 3);
    assert_eq!(baseline.usage().get(AnalysisLimitKind::ModuleDepth), 2);

    for kind in [
        AnalysisLimitKind::SourceBytes,
        AnalysisLimitKind::Modules,
        AnalysisLimitKind::ModuleDepth,
        AnalysisLimitKind::AstNodes,
        AnalysisLimitKind::TypeDepth,
        AnalysisLimitKind::GenericInstantiations,
        AnalysisLimitKind::OverloadCandidates,
        AnalysisLimitKind::WorkUnits,
    ] {
        let used = baseline.usage().get(kind);
        assert!(used > 0, "{} must be charged", kind.name());
        let exact = AnalysisLimits::unlimited().with_limit(kind, used);
        let exact_outcome = loader(&sources).analyze_with_commands_and_limits_controlled(
            Path::new("/project/main.opaal"),
            &commands,
            &AnalysisControl::never(),
            exact,
        );
        assert!(
            matches!(exact_outcome, ModuleAnalysisOutcome::Complete(ref report) if report.program().is_some()),
            "{} exact limit {used} failed: {exact_outcome:?}",
            kind.name()
        );

        let below = AnalysisLimits::unlimited().with_limit(kind, used - 1);
        let exceeded = loader(&sources).analyze_with_commands_and_limits_controlled(
            Path::new("/project/main.opaal"),
            &commands,
            &AnalysisControl::never(),
            below,
        );
        assert!(
            matches!(exceeded, ModuleAnalysisOutcome::BudgetExceeded(error)
                if error.kind() == kind && error.limit() == used - 1),
            "{} must refuse its first excess: {exceeded:?}",
            kind.name()
        );
    }
}

#[test]
fn compound_assignment_charges_its_lowered_read_and_binary_at_exact_analysis_limits() {
    let compound = "mut total = 7\ntotal += 2\ntotal\n";
    let expanded = "mut total = 7\ntotal = total + 2\ntotal\n";
    let analyze = |text: &str| {
        loader(&resource_sources(text)).analyze_with_limits(
            Path::new("/project/main.opaal"),
            AnalysisLimits::unlimited(),
        )
    };
    let baseline = analyze(compound);
    let old = analyze(expanded);
    assert!(baseline.program().is_some(), "{:?}", baseline.issues());
    for kind in [AnalysisLimitKind::AstNodes, AnalysisLimitKind::WorkUnits] {
        let used = baseline.usage().get(kind);
        assert!(used > 0);
        if kind == AnalysisLimitKind::AstNodes {
            assert_eq!(used, old.usage().get(kind));
        }
        for (limit, complete) in [(used, true), (used - 1, false)] {
            let outcome = loader(&resource_sources(compound)).analyze_with_limits_controlled(
                Path::new("/project/main.opaal"),
                &AnalysisControl::never(),
                AnalysisLimits::unlimited().with_limit(kind, limit),
            );
            if complete {
                assert!(
                    matches!(outcome, ModuleAnalysisOutcome::Complete(ref report) if report.program().is_some())
                );
            } else {
                assert!(
                    matches!(outcome, ModuleAnalysisOutcome::BudgetExceeded(e) if e.kind() == kind && e.limit() == limit)
                );
            }
        }
    }
}

#[test]
fn inferred_nested_list_depth_is_measured_without_an_annotation() {
    let sources = resource_sources("[[[1]]]\n");
    let baseline = loader(&sources).analyze_with_limits(
        Path::new("/project/main.opaal"),
        AnalysisLimits::unlimited(),
    );
    assert!(baseline.program().is_some(), "{:?}", baseline.issues());
    assert_eq!(baseline.usage().get(AnalysisLimitKind::TypeDepth), 4);

    assert!(matches!(
        loader(&sources).analyze_with_limits_controlled(
            Path::new("/project/main.opaal"),
            &AnalysisControl::never(),
            AnalysisLimits::unlimited().with_limit(AnalysisLimitKind::TypeDepth, 3),
        ),
        ModuleAnalysisOutcome::BudgetExceeded(exceeded)
            if exceeded.kind() == AnalysisLimitKind::TypeDepth && exceeded.limit() == 3
    ));
}

#[test]
fn diagnostic_budget_refuses_instead_of_publishing_a_truncated_report() {
    let sources = resource_sources(concat!(
        "",
        "let first: Missing = 1\n",
        "let second: Missing = 2\n",
    ));
    let baseline = loader(&sources).analyze_with_limits(
        Path::new("/project/main.opaal"),
        AnalysisLimits::unlimited(),
    );
    let diagnostics = baseline.usage().get(AnalysisLimitKind::Diagnostics);
    assert!(diagnostics >= 2);

    assert!(matches!(
        loader(&sources).analyze_with_limits_controlled(
            Path::new("/project/main.opaal"),
            &AnalysisControl::never(),
            AnalysisLimits::unlimited()
                .with_limit(AnalysisLimitKind::Diagnostics, diagnostics - 1),
        ),
        ModuleAnalysisOutcome::BudgetExceeded(exceeded)
            if exceeded.kind() == AnalysisLimitKind::Diagnostics
                && exceeded.limit() == diagnostics - 1
    ));
}

fn program(source: &str) -> ModuleProgram {
    let sources = resource_sources(source);
    loader(&sources)
        .load(Path::new("/project/main.opaal"))
        .expect("resource fixture must pass shared analysis")
}

fn execute(
    program: &ModuleProgram,
    limits: &EvalLimits,
) -> opaal_runtime::script::ScriptExecutionOutcome {
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
        limits,
    )
}

#[test]
fn one_runtime_step_budget_crosses_statement_and_module_boundaries() {
    let program = program(concat!(
        "",
        "import './support.opaal' as support\n",
        "let first = support::identity(1)\n",
        "let second = support::identity(2)\n",
        "[first, second]\n",
    ));
    let used = (0..10_000)
        .find(|steps| {
            matches!(
                execute(
                    &program,
                    &EvalLimits::pure_opaal(
                        CancellationToken::never(),
                        ResourceBudget::steps(*steps).with_collection_items(2),
                    ),
                )
                .primary(),
                PrimaryOutcome::Completed(_)
            )
        })
        .expect("the fixture must complete below the search ceiling");
    assert!(used > 4, "module and root statements must share charges");

    let exact = ResourceBudget::steps(used).with_collection_items(2);
    assert!(matches!(
        execute(
            &program,
            &EvalLimits::pure_opaal(CancellationToken::never(), exact)
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));

    let first_excess = ResourceBudget::steps(used - 1).with_collection_items(2);
    let outcome = execute(
        &program,
        &EvalLimits::pure_opaal(CancellationToken::never(), first_excess),
    );
    assert!(matches!(
        outcome.primary(),
        PrimaryOutcome::Error(error) if error.render().contains("resource budget")
    ));
}

#[test]
fn collection_and_call_depth_limits_refuse_the_first_excess() {
    let collection_program = program(concat!("", "let marker = 1\n", "[1, 2, 3]\n",));
    assert!(matches!(
        execute(
            &collection_program,
            &EvalLimits::pure_opaal(
                CancellationToken::never(),
                ResourceBudget::steps(10_000).with_collection_items(3),
            )
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));
    let collection = ResourceBudget::steps(10_000).with_collection_items(2);
    let limits = EvalLimits::pure_opaal(CancellationToken::never(), collection);
    let outcome = execute(&collection_program, &limits);
    assert!(matches!(outcome.primary(), PrimaryOutcome::Error(_)));

    let retained_collections = program(concat!(
        "",
        "type Pair = { left: Int, right: Int }\n",
        "enum Choice { Some(Int, Int), None }\n",
        "let values = [1, 2, 3]\n",
        "let [first, ...rest] = values\n",
        "let record = { left: first, right: rest[0] }\n",
        "let pair = Pair { left: record.left, right: record.right }\n",
        "Choice::Some(pair.left, pair.right)\n",
    ));
    let retained_items = 3 + 2 + 2 + 2 + 2;
    assert!(matches!(
        execute(
            &retained_collections,
            &EvalLimits::pure_opaal(
                CancellationToken::never(),
                ResourceBudget::steps(10_000).with_collection_items(retained_items),
            )
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));
    let outcome = execute(
        &retained_collections,
        &EvalLimits::pure_opaal(
            CancellationToken::never(),
            ResourceBudget::steps(10_000).with_collection_items(retained_items - 1),
        ),
    );
    assert!(matches!(outcome.primary(), PrimaryOutcome::Error(_)));

    let recursive = program(concat!(
        "",
        "def descend(value: Int) -> Int {\n",
        "    if value == 0 { return 0 }\n",
        "    return descend(value - 1)\n",
        "}\n",
        "descend(8)\n",
    ));
    let depth = (0..64)
        .find(|depth| {
            matches!(
                execute(
                    &recursive,
                    &EvalLimits::pure_opaal(
                        CancellationToken::never(),
                        ResourceBudget::steps(10_000).with_call_depth(*depth),
                    ),
                )
                .primary(),
                PrimaryOutcome::Completed(_)
            )
        })
        .expect("the fixture must complete below the search ceiling");
    assert!(depth > 1);
    let exact = ResourceBudget::steps(10_000).with_call_depth(depth);
    assert!(matches!(
        execute(
            &recursive,
            &EvalLimits::pure_opaal(CancellationToken::never(), exact)
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));
    let first_excess = ResourceBudget::steps(10_000).with_call_depth(depth - 1);
    let outcome = execute(
        &recursive,
        &EvalLimits::pure_opaal(CancellationToken::never(), first_excess),
    );
    assert!(matches!(outcome.primary(), PrimaryOutcome::Error(_)));
}

#[test]
fn interpolated_collection_bytes_refuse_before_the_first_excess_copy() {
    const EXACT: ResourceBudget = ResourceBudget::steps(10_000).with_collection_bytes(12);
    fn assert_copy<T: Copy>() {}
    assert_copy::<ResourceBudget>();

    let strings = program(concat!("", "let seed = 'abcd'\n", "\"{seed}{seed}\"\n",));
    assert!(matches!(
        execute(
            &strings,
            &EvalLimits::pure_opaal(CancellationToken::never(), EXACT),
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));
    let outcome = execute(
        &strings,
        &EvalLimits::pure_opaal(
            CancellationToken::never(),
            ResourceBudget::steps(10_000).with_collection_bytes(11),
        ),
    );
    assert!(matches!(outcome.primary(), PrimaryOutcome::Error(_)));
}

#[test]
fn cancellation_wins_at_each_polled_schedule_without_becoming_a_budget_error() {
    let program = program(concat!(
        "",
        "let values = [1, 2, 3, 4, 5, 6, 7, 8]\n",
        "let total = 0\n",
        "for value in values { let total = total + value }\n",
        "total\n",
    ));

    for schedule in 0..8 {
        let polls = Arc::new(AtomicUsize::new(0));
        let token = CancellationToken::from_fn({
            let polls = Arc::clone(&polls);
            move || polls.fetch_add(1, Ordering::Relaxed) >= schedule
        });
        let outcome = execute(
            &program,
            &EvalLimits::pure_opaal(token, ResourceBudget::steps(10_000)),
        );
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Cancelled(cancelled) if cancelled.reason() == CancelReason::Requested),
            "schedule {schedule} did not preserve cancellation: {:?}",
            outcome.primary()
        );
    }
}

#[test]
fn new_loop_headers_charge_entries_and_poll_even_with_an_empty_body() {
    for (header, budget, completes) in [
        ("for item in 0..10 {}", 4, true),
        ("for i, item in 0..10 {}", 13, false),
        ("for i, item in 0..10 {}", 14, true),
        ("for (_, _) in rows {}", 8, false),
        ("for (_, _) in rows {}", 9, true),
    ] {
        let source = SourceFile::new(SourceId::new(40), "budget.opaal", header);
        let ParseOutcome::Complete(script) = parse_opaal(&source) else {
            panic!("loop fixture must parse: {header}");
        };
        let mut scope = ScopeStack::new();
        if header.contains("rows") {
            scope
                .declare(
                    "rows",
                    BindingMutability::Immutable,
                    &Value::list(vec![Value::Tuple(Arc::from([
                        Value::Int(0),
                        Value::Int(0),
                    ]))]),
                )
                .unwrap();
        }
        let result = evaluate_in_environment(
            &script,
            &source,
            &mut scope,
            &mut Environment::new(),
            &EvalLimits::new(CancellationToken::never(), ResourceBudget::steps(budget)),
        );
        if completes {
            assert!(
                matches!(result, Ok(Completion::Value(Value::Null))),
                "{header}: {result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(ref error) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded)),
                "{header}: {result:?}"
            );
        }
    }

    let source = SourceFile::new(
        SourceId::new(41),
        "cancel.opaal",
        "for i, item in 0..10000 {}",
    );
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("indexed range must parse");
    };
    let polls = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::from_fn(move || polls.fetch_add(1, Ordering::Relaxed) >= 16);
    let mut scope = ScopeStack::new();
    let result = evaluate_in_environment(
        &script,
        &source,
        &mut scope,
        &mut Environment::new(),
        &EvalLimits::new(cancel, ResourceBudget::steps(10_000)),
    );
    assert!(
        matches!(result, Ok(Completion::Cancelled(ref cancelled)) if source.slice(cancelled.span()).unwrap() == "0..10000"),
        "{result:?}"
    );
    assert!(scope.get("i").is_none());
    assert!(scope.get("item").is_none());
}

#[test]
fn indexed_loops_meter_validation_of_composite_items() {
    let items = ["1"; 32].join(", ");
    let legacy = program(&format!("for item in [[{items}]] {{}}"));
    let indexed = program(&format!("for i, item in [[{items}]] {{}}"));
    let completes = |program: &ModuleProgram, steps| {
        matches!(
            execute(
                program,
                &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::steps(steps)),
            )
            .primary(),
            PrimaryOutcome::Completed(_)
        )
    };
    let legacy_steps = (0..100)
        .find(|&steps| completes(&legacy, steps))
        .expect("legacy loop completes within the test budget");
    assert!(!completes(&indexed, legacy_steps + 32));
    assert!(completes(&indexed, legacy_steps + 33));
}

#[test]
fn rust_built_loop_patterns_apply_the_tuple_depth_limit_before_evaluation() {
    use opaal_syntax::{MAX_TUPLE_DEPTH, Pattern, Script, Statement, StatementKind};

    let source = SourceFile::new(
        SourceId::new(43),
        "embedded-loop.opaal",
        "for i, (_, _) in [] {}",
    );
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("fixture must parse");
    };
    let statement = &script.statements()[0];
    let StatementKind::For(parsed) = statement.kind() else {
        panic!("expected a loop");
    };
    let Pattern::Tuple(tuple) = &parsed.binding else {
        panic!("expected a tuple item pattern");
    };
    let mut pattern = tuple.elements[0].clone();
    for depth in 1..=MAX_TUPLE_DEPTH + 1 {
        let mut wrapper = tuple.clone();
        wrapper.elements[0] = pattern;
        pattern = Pattern::Tuple(wrapper);
        if depth >= MAX_TUPLE_DEPTH {
            let mut loop_statement = parsed.clone();
            loop_statement.binding = pattern.clone();
            let embedded = Script::new(
                vec![Statement::new(
                    StatementKind::For(loop_statement),
                    statement.span(),
                )],
                script.span(),
            );
            let result = evaluate_in_environment(
                &embedded,
                &source,
                &mut ScopeStack::new(),
                &mut Environment::new(),
                &EvalLimits::default(),
            );
            if depth == MAX_TUPLE_DEPTH {
                assert!(
                    matches!(result, Ok(Completion::Value(Value::Null))),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(ref error) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded)),
                    "{result:?}"
                );
            }
        }
    }
}

#[test]
fn rust_built_loops_reject_literal_patterns_before_iteration() {
    use opaal_syntax::{Pattern, Script, Statement, StatementKind};

    let source = SourceFile::new(
        SourceId::new(44),
        "embedded-loop.opaal",
        "for i, item in [] {}\nmatch 1 { 1 => {} }",
    );
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("fixture must parse");
    };
    let [loop_node, match_node] = script.statements() else {
        panic!("expected a loop and a match");
    };
    let (StatementKind::For(loop_statement), StatementKind::Match(match_statement)) =
        (loop_node.kind(), match_node.kind())
    else {
        panic!("expected a loop and a match");
    };
    let mut loop_statement = loop_statement.clone();
    let pattern = match_statement.arms[0].pattern.clone();
    assert!(matches!(pattern, Pattern::Literal(_)));
    loop_statement.binding = pattern;
    let embedded = Script::new(
        vec![Statement::new(
            StatementKind::For(loop_statement),
            loop_node.span(),
        )],
        script.span(),
    );
    let error = evaluate_in_environment(
        &embedded,
        &source,
        &mut ScopeStack::new(),
        &mut Environment::new(),
        &EvalLimits::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::Unsupported {
            feature: "for loop item pattern"
        }
    ));
}

#[test]
fn a_failed_dynamic_tuple_binding_never_runs_its_body_or_leaks_names() {
    let source = SourceFile::new(
        SourceId::new(42),
        "pattern.opaal",
        r#"
mut total = 0
for i, (item, (_, _)) in [(5, (1, 2)), (99, false)] { total += i + item }
"#,
    );
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("dynamic list fixture must parse");
    };
    let mut scope = ScopeStack::new();
    let error = evaluate_in_environment(
        &script,
        &source,
        &mut scope,
        &mut Environment::new(),
        &EvalLimits::new(CancellationToken::never(), ResourceBudget::steps(100)),
    )
    .unwrap_err();
    assert!(matches!(error.kind(), RuntimeErrorKind::PatternMismatch));
    assert_eq!(source.slice(error.span()).unwrap(), "(item, (_, _))");
    assert_eq!(scope.get("total"), Some(&Value::Int(5)));
    assert!(scope.get("i").is_none());
    assert!(scope.get("item").is_none());
}

#[test]
fn import_groups_charge_every_entry_and_accept_only_the_exact_analysis_boundaries() {
    for count in [1, 32] {
        let entries = (0..count)
            .map(|i| format!("std::math as m{i}"))
            .collect::<Vec<_>>();
        let singles = entries
            .iter()
            .map(|entry| format!("import {entry}\n"))
            .collect::<String>();
        let group = format!("import {{ {} }}\n", entries.join(","));
        let analyze = |text: &str| {
            loader(&resource_sources(text)).analyze_with_limits(
                Path::new("/project/main.opaal"),
                AnalysisLimits::unlimited(),
            )
        };
        let single_report = analyze(&singles);
        let group_report = analyze(&group);
        assert!(
            group_report.program().is_some(),
            "{:?}",
            group_report.issues()
        );
        assert_eq!(
            group_report.usage().get(AnalysisLimitKind::AstNodes),
            single_report.usage().get(AnalysisLimitKind::AstNodes) + 1
        );
        for kind in [
            AnalysisLimitKind::AstNodes,
            AnalysisLimitKind::WorkUnits,
            AnalysisLimitKind::Modules,
            AnalysisLimitKind::SourceBytes,
        ] {
            let used = group_report.usage().get(kind);
            for limit in [used, used - 1] {
                let outcome = loader(&resource_sources(&group)).analyze_with_limits_controlled(
                    Path::new("/project/main.opaal"),
                    &AnalysisControl::never(),
                    AnalysisLimits::unlimited().with_limit(kind, limit),
                );
                if limit == used {
                    assert!(
                        matches!(outcome, ModuleAnalysisOutcome::Complete(ref report) if report.program().is_some()),
                        "{outcome:?}"
                    );
                } else {
                    assert!(
                        matches!(outcome, ModuleAnalysisOutcome::BudgetExceeded(error) if error.kind() == kind),
                        "{outcome:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn grouping_preserves_dependency_initialization_order_instead_of_imposing_lexical_order() {
    let mut errors = Vec::new();
    for source in [
        "import './z.opaal' as z\nimport './a.opaal' as a",
        "import {'./z.opaal', './a.opaal'}",
    ] {
        let sources = Sources::default()
            .maps("/project/main.opaal", "/project/main.opaal")
            .maps("/project/z.opaal", "/project/z.opaal")
            .maps("/project/a.opaal", "/project/a.opaal")
            .contains("/project/main.opaal", source)
            .contains("/project/a.opaal", "let broken = 1 / 0\nexport {broken}\n")
            .contains("/project/z.opaal", "let broken = 2 / 0\nexport {broken}\n");
        let program = loader(&sources)
            .load(Path::new("/project/main.opaal"))
            .unwrap();
        let outcome = execute(
            &program,
            &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::opaal()),
        );
        let PrimaryOutcome::Error(error) = outcome.primary() else {
            panic!("{outcome:?}")
        };
        assert!(error.render().contains("/project/a.opaal"));
        errors.push(error.render().to_owned());
    }
    assert_eq!(errors[0], errors[1]);
}

#[test]
fn member_selectors_charge_exact_analysis_and_retained_binding_boundaries() {
    let text = "import std::math::{abs as first, abs as second}\n";
    let sources = resource_sources(text);
    let report = loader(&sources).analyze_with_limits(
        Path::new("/project/main.opaal"),
        AnalysisLimits::unlimited(),
    );
    assert!(report.program().is_some(), "{:?}", report.issues());
    for kind in [
        AnalysisLimitKind::AstNodes,
        AnalysisLimitKind::WorkUnits,
        AnalysisLimitKind::SourceBytes,
    ] {
        let used = report.usage().get(kind);
        for ceiling in [used, used - 1] {
            let outcome = loader(&sources).analyze_with_limits_controlled(
                Path::new("/project/main.opaal"),
                &AnalysisControl::never(),
                AnalysisLimits::unlimited().with_limit(kind, ceiling),
            );
            assert!(
                if ceiling == used {
                    matches!(outcome, ModuleAnalysisOutcome::Complete(ref report) if report.program().is_some())
                } else {
                    matches!(outcome, ModuleAnalysisOutcome::BudgetExceeded(error) if error.kind() == kind)
                },
                "{kind:?} {ceiling}: {outcome:?}"
            );
        }
    }
    let program = report.program().unwrap();
    for (items, bytes, succeeds) in [(2, 17, true), (1, 17, false), (2, 16, false)] {
        let limits = EvalLimits::pure_opaal(
            CancellationToken::never(),
            ResourceBudget::steps(100)
                .with_collection_items(items)
                .with_collection_bytes(bytes),
        );
        let outcome = execute(program, &limits);
        assert_eq!(
            matches!(outcome.primary(), PrimaryOutcome::Completed(_)),
            succeeds,
            "items={items}, bytes={bytes}: {outcome:?}"
        );
    }
}

#[test]
fn record_shorthand_analysis_charges_fields_references_and_bindings_exactly() {
    let source = "type Item = {value: Int}\nlet value = 7\nlet item = Item {value}\ndef pick(Item {value}: Item) -> Int { return value }\nlet fields = {value, result: pick(item)}\n";
    let sources = resource_sources(source);
    let report = loader(&sources).analyze_with_limits(
        Path::new("/project/main.opaal"),
        AnalysisLimits::unlimited(),
    );
    assert!(report.program().is_some(), "{:?}", report.issues());
    let expanded = source
        .replace("Item {value}", "Item {value: value}")
        .replace("{value,", "{value: value,");
    let expanded_sources = resource_sources(&expanded);
    let expanded_report = loader(&expanded_sources).analyze_with_limits(
        Path::new("/project/main.opaal"),
        AnalysisLimits::unlimited(),
    );
    assert!(
        expanded_report.program().is_some(),
        "{:?}",
        expanded_report.issues()
    );
    for kind in [
        AnalysisLimitKind::SourceBytes,
        AnalysisLimitKind::AstNodes,
        AnalysisLimitKind::TypeDepth,
        AnalysisLimitKind::WorkUnits,
    ] {
        let used = report.usage().get(kind);
        assert!(used > 0);
        if matches!(
            kind,
            AnalysisLimitKind::AstNodes | AnalysisLimitKind::TypeDepth
        ) {
            assert_eq!(
                used,
                expanded_report.usage().get(kind),
                "{kind:?}: hidden shorthand work must equal explicit work"
            );
        }
        for limit in [used, used - 1] {
            let outcome = loader(&sources).analyze_with_limits_controlled(
                Path::new("/project/main.opaal"),
                &AnalysisControl::never(),
                AnalysisLimits::unlimited().with_limit(kind, limit),
            );
            assert!(
                if limit == used {
                    matches!(outcome, ModuleAnalysisOutcome::Complete(ref report) if report.program().is_some())
                } else {
                    matches!(outcome, ModuleAnalysisOutcome::BudgetExceeded(ref error) if error.kind() == kind)
                },
                "{kind:?} {limit}: {outcome:?}"
            );
        }
    }
}

#[test]
fn list_literal_spread_analysis_charges_source_nodes_depth_and_work_exactly() {
    let source =
        "let items: List[Int] = [1, 2]\nlet result = [...[], ...items, ...[[3]]]\nresult\n";
    let sources = resource_sources(source);
    let report = loader(&sources).analyze_with_limits(
        Path::new("/project/main.opaal"),
        AnalysisLimits::unlimited(),
    );
    assert!(report.program().is_some(), "{:?}", report.issues());
    for kind in [
        AnalysisLimitKind::SourceBytes,
        AnalysisLimitKind::AstNodes,
        AnalysisLimitKind::TypeDepth,
        AnalysisLimitKind::WorkUnits,
    ] {
        let used = report.usage().get(kind);
        assert!(used > 0);
        for limit in [used, used - 1] {
            let outcome = loader(&sources).analyze_with_limits_controlled(
                Path::new("/project/main.opaal"),
                &AnalysisControl::never(),
                AnalysisLimits::unlimited().with_limit(kind, limit),
            );
            if limit == used {
                assert!(
                    matches!(outcome, ModuleAnalysisOutcome::Complete(ref report) if report.program().is_some()),
                    "{outcome:?}"
                );
            } else {
                assert!(
                    matches!(outcome, ModuleAnalysisOutcome::BudgetExceeded(ref error) if error.kind() == kind),
                    "{outcome:?}"
                );
            }
        }
    }
}

#[test]
fn runtime_wrapping_counts_tuple_depth_independently_of_source_depth() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            for (wrapped, succeeds) in [(31, true), (32, false)] {
                let source = format!(
                    "mut value: Any = (1, 2)\nfor _ in 0..{wrapped} {{ value = [value] }}\n",
                );
                let limits =
                    EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::opaal());
                let program = program(&source);
                let outcome = execute(&program, &limits);
                assert_eq!(
                    matches!(outcome.primary(), PrimaryOutcome::Completed(_)),
                    succeeds,
                    "{outcome:?}"
                );
                if !succeeds {
                    let PrimaryOutcome::Error(error) = outcome.primary() else {
                        unreachable!()
                    };
                    assert!(error.render().contains("resource budget"), "{error:?}");
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn tuple_slot_depth_refuses_before_the_next_slot_runs() {
    let mut source = String::from(
        "def later() -> Int { throw \"later slot ran\" }\nmut value: Any = (1, 2)\n\
         for _ in 0..31 { value = [value] }\n",
    );
    source.push_str("let result = (value, later())\n");
    let outcome = execute(&program(&source), &EvalLimits::default());
    assert!(
        matches!(outcome.primary(), PrimaryOutcome::Error(error)
            if error.render().contains("resource budget") && !error.render().contains("later slot ran")),
        "{outcome:?}"
    );
}

#[test]
fn tuple_free_shared_values_keep_their_source_budget() {
    let source = "mut value: Any = 0\nfor _ in 0..24 { value = [value, value] }\n";
    let outcome = execute(&program(source), &EvalLimits::default());
    assert!(
        matches!(outcome.primary(), PrimaryOutcome::Completed(_)),
        "{outcome:?}"
    );
}

#[test]
fn rust_ingress_rejects_before_taking_ownership_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let source = SourceFile::new(
                SourceId::new(1),
                "embedding.opaal",
                "def consume(item: Any) -> Int { return 7 }",
            );
            let ParseOutcome::Complete(script) = parse_opaal(&source) else {
                panic!("expected a complete callable definition");
            };
            let span = script.span();
            let mut scope = ScopeStack::new();
            let limits = EvalLimits::default();
            evaluate_in_environment(
                &script, &source, &mut scope, &mut Environment::new(), &limits,
            ).unwrap();
            scope.declare("seed", BindingMutability::Mutable, &Value::Int(1)).unwrap();
            let mut legacy = Value::Null;
            for _ in 0..64 {
                legacy = Value::list(vec![legacy]);
            }
            let admitted = AdmittedValue::new(&legacy).unwrap();
            assert!(AdmittedValue::new(&Value::list(vec![legacy])).is_err());
            // The admitted copy releases the last reference on this stack.
            drop(admitted);
            let mut schema = opaal_runtime::module::ValueType::Int;
            for _ in 0..64 {
                schema = opaal_runtime::module::ValueType::List(Box::new(schema));
            }
            use opaal_runtime::stream::{StreamCardinality, StreamSchema};
            assert!(StreamSchema::new(&schema, StreamCardinality::Unknown).is_ok());
            let too_deep = opaal_runtime::module::ValueType::List(Box::new(schema));
            assert!(StreamSchema::new(&too_deep, StreamCardinality::Unknown).is_err());
            let apply = |scope: &ScopeStack, value: &Value, limits: &EvalLimits| {
                apply_callable(
                    scope.get("consume").unwrap(),
                    std::slice::from_ref(value),
                    &source,
                    span,
                    &mut Environment::new(),
                    limits,
                )
            };
            let mut producer_values = vec![Value::Tuple(Arc::from([Value::Int(1), Value::Int(2)]))];
            for _ in 0..10_000 {
                producer_values.push(Value::list(vec![producer_values.last().unwrap().clone()]));
            }
            assert!(matches!(apply(&scope, &producer_values[31], &limits), Ok(Completion::Value(Value::Int(7)))));
            for value in [&producer_values[32], producer_values.last().unwrap()] {
                assert!(matches!(apply(&scope, value, &limits), Err(error) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded)));
                assert!(matches!(scope.assign("seed", value), Err(ScopeError::ValueAdmission(_))));
                assert!(ValueStream::once(value).is_err());
            }
            assert_eq!(scope.get("seed"), Some(&Value::Int(1)));
            let cancelled = EvalLimits::pure_opaal(CancellationToken::from_fn(|| true), ResourceBudget::opaal());
            assert!(matches!(apply(&scope, producer_values.last().unwrap(), &cancelled), Ok(Completion::Cancelled(_))));

            let producer_values = Rc::new(producer_values);
            let mut stream = ValueStream::from_pull_fn({
                let producer_values = Rc::clone(&producer_values);
                move || match AdmittedValue::new(producer_values.last().unwrap()) {
                    Ok(value) => StreamPull::Item(value),
                    Err(_) => StreamPull::Failed(AdmittedStreamError::resource_limit(span)),
                }
            });
            assert!(matches!(stream.pull(), StreamPull::Failed(error) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded)));
            assert!(matches!(stream.pull(), StreamPull::End));
            drop(stream);
            let mut producer_values = Rc::try_unwrap(producer_values).ok().unwrap();
            // The producer owns the raw graph and can release it from the root down.
            while producer_values.pop().is_some() {}
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn prefix_expansion_is_bounded_at_exact_work_and_diagnostic_limits() {
    let declarations = (0..1024)
        .map(|index| format!("let check_{index:04} = {index}\n"))
        .collect::<String>();
    for selectors in ["check_*", "check_*, check*", "absent_*, missing_*"] {
        let text = format!("export {{{selectors}}}\n{declarations}");
        let sources = resource_sources(&text);
        let report = loader(&sources).analyze_with_limits(
            Path::new("/project/main.opaal"),
            AnalysisLimits::unlimited(),
        );
        assert_eq!(report.program().is_some(), selectors == "check_*");
        for kind in [AnalysisLimitKind::WorkUnits, AnalysisLimitKind::Diagnostics] {
            let used = report.usage().get(kind);
            if used == 0 {
                continue;
            }
            for (limit, accepted) in [(used, true), (used - 1, false)] {
                let outcome = loader(&sources).analyze_with_limits_controlled(
                    Path::new("/project/main.opaal"),
                    &AnalysisControl::never(),
                    AnalysisLimits::unlimited().with_limit(kind, limit),
                );
                if accepted {
                    assert!(
                        matches!(outcome, ModuleAnalysisOutcome::Complete(_)),
                        "{kind:?}: {outcome:?}"
                    );
                } else {
                    assert!(
                        matches!(outcome, ModuleAnalysisOutcome::BudgetExceeded(_)),
                        "{kind:?}: {outcome:?}"
                    );
                }
            }
        }
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let control =
            AnalysisControl::cooperative(move || observed.fetch_add(1, Ordering::SeqCst) >= 5000);
        assert!(matches!(
            loader(&sources).analyze_with_limits_controlled(
                Path::new("/project/main.opaal"),
                &control,
                AnalysisLimits::unlimited()
            ),
            ModuleAnalysisOutcome::Cancelled
        ));
    }
}

#[test]
fn cancellation_at_every_prefix_analysis_poll_never_accepts_partial_exports() {
    let sources =
        resource_sources("export {check_*}\nlet check_a = 1\nlet check_b = 2\nlet check_c = 3\n");
    let polls = Arc::new(AtomicUsize::new(0));
    let observed = polls.clone();
    let counting = AnalysisControl::cooperative(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        false
    });
    assert!(matches!(loader(&sources).analyze_with_limits_controlled(
        Path::new("/project/main.opaal"), &counting, AnalysisLimits::unlimited()),
        ModuleAnalysisOutcome::Complete(report) if report.program().is_some()));
    for stop in 0..polls.load(Ordering::SeqCst) {
        let observed = Arc::new(AtomicUsize::new(0));
        let control =
            AnalysisControl::cooperative(move || observed.fetch_add(1, Ordering::SeqCst) >= stop);
        assert!(
            matches!(
                loader(&sources).analyze_with_limits_controlled(
                    Path::new("/project/main.opaal"),
                    &control,
                    AnalysisLimits::unlimited()
                ),
                ModuleAnalysisOutcome::Cancelled
            ),
            "poll {stop} accepted a partial program"
        );
    }
}
