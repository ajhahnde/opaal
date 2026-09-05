#![forbid(unsafe_code)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flash_platform::FakePlatform;
use flash_runtime::Environment;
use flash_runtime::builtin::standard_registry;
use flash_runtime::eval::{CancelReason, CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use flash_runtime::module::{
    AnalysisControl, AnalysisLimitKind, AnalysisLimits, ModuleAnalysisOutcome, ModuleCanonicalizer,
    ModuleId, ModulePathError, ModuleProgram, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use flash_runtime::outcome::PrimaryOutcome;
use flash_runtime::plan::SessionOptions;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::execute_module_program_outcome_with_limits;
use flash_syntax::LanguageMajor;

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
        .maps("/project/main.fsh", "/project/main.fsh")
        .maps("/project/./support.fsh", "/project/support.fsh")
        .maps("/project/support.fsh", "/project/support.fsh")
        .contains("/project/main.fsh", root_source)
        .contains(
            "/project/support.fsh",
            concat!(
                "language 2\n\n",
                "def identity[T: Equal](value: T) -> T {\n",
                "    return $value\n",
                "}\n\n",
                "export { identity }\n",
            ),
        )
}

fn analysis_source() -> &'static str {
    concat!(
        "language 2\n\n",
        "import './support.fsh' as support\n",
        "import std::value as value\n\n",
        "let chosen: List[List[Int]] = [[support::identity[Int](1)]]\n",
        "value::length($chosen)\n",
    )
}

fn loader(sources: &Sources) -> ModuleProgramLoader<'_> {
    ModuleProgramLoader::for_language(sources, sources, LanguageMajor::V2)
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
        bytes: b"language 2\n".to_vec(),
        requested: Cell::new(None),
    };
    let outcome = ModuleProgramLoader::for_language(&source, &source, LanguageMajor::V2)
        .analyze_with_limits_controlled(
            Path::new("/bounded.fsh"),
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
        Path::new("/project/main.fsh"),
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
            Path::new("/project/main.fsh"),
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
            Path::new("/project/main.fsh"),
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
fn inferred_nested_list_depth_is_measured_without_an_annotation() {
    let sources = resource_sources("language 2\n[[[1]]]\n");
    let baseline = loader(&sources)
        .analyze_with_limits(Path::new("/project/main.fsh"), AnalysisLimits::unlimited());
    assert!(baseline.program().is_some(), "{:?}", baseline.issues());
    assert_eq!(baseline.usage().get(AnalysisLimitKind::TypeDepth), 4);

    assert!(matches!(
        loader(&sources).analyze_with_limits_controlled(
            Path::new("/project/main.fsh"),
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
        "language 2\n",
        "let first: Missing = 1\n",
        "let second: Missing = 2\n",
    ));
    let baseline = loader(&sources)
        .analyze_with_limits(Path::new("/project/main.fsh"), AnalysisLimits::unlimited());
    let diagnostics = baseline.usage().get(AnalysisLimitKind::Diagnostics);
    assert!(diagnostics >= 2);

    assert!(matches!(
        loader(&sources).analyze_with_limits_controlled(
            Path::new("/project/main.fsh"),
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
        .load(Path::new("/project/main.fsh"))
        .expect("resource fixture must pass shared analysis")
}

fn execute(
    program: &ModuleProgram,
    limits: &EvalLimits,
) -> flash_runtime::script::ScriptExecutionOutcome {
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
        "language 2\n",
        "import './support.fsh' as support\n",
        "let first = support::identity(1)\n",
        "let second = support::identity(2)\n",
        "[$first, $second]\n",
    ));
    let used = (0..10_000)
        .find(|steps| {
            matches!(
                execute(
                    &program,
                    &EvalLimits::pure_v2(
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
            &EvalLimits::pure_v2(CancellationToken::never(), exact)
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));

    let first_excess = ResourceBudget::steps(used - 1).with_collection_items(2);
    let outcome = execute(
        &program,
        &EvalLimits::pure_v2(CancellationToken::never(), first_excess),
    );
    assert!(matches!(
        outcome.primary(),
        PrimaryOutcome::Error(error) if error.render().contains("resource budget")
    ));
}

#[test]
fn collection_and_call_depth_limits_refuse_the_first_excess() {
    let collection_program = program(concat!("language 2\n", "let marker = 1\n", "[1, 2, 3]\n",));
    assert!(matches!(
        execute(
            &collection_program,
            &EvalLimits::pure_v2(
                CancellationToken::never(),
                ResourceBudget::steps(10_000).with_collection_items(3),
            )
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));
    let collection = ResourceBudget::steps(10_000).with_collection_items(2);
    let limits = EvalLimits::pure_v2(CancellationToken::never(), collection);
    let outcome = execute(&collection_program, &limits);
    assert!(matches!(outcome.primary(), PrimaryOutcome::Error(_)));

    let retained_collections = program(concat!(
        "language 2\n",
        "type Pair = { left: Int, right: Int }\n",
        "enum Choice { Some(Int, Int), None }\n",
        "let values = [1, 2, 3]\n",
        "let [first, ...rest] = $values\n",
        "let record = { left: $first, right: $rest[0] }\n",
        "let pair = Pair { left: $record.left, right: $record.right }\n",
        "Choice::Some($pair.left, $pair.right)\n",
    ));
    let retained_items = 3 + 2 + 2 + 2 + 2;
    assert!(matches!(
        execute(
            &retained_collections,
            &EvalLimits::pure_v2(
                CancellationToken::never(),
                ResourceBudget::steps(10_000).with_collection_items(retained_items),
            )
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));
    let outcome = execute(
        &retained_collections,
        &EvalLimits::pure_v2(
            CancellationToken::never(),
            ResourceBudget::steps(10_000).with_collection_items(retained_items - 1),
        ),
    );
    assert!(matches!(outcome.primary(), PrimaryOutcome::Error(_)));

    let recursive = program(concat!(
        "language 2\n",
        "def descend(value: Int) -> Int {\n",
        "    if $value == 0 { return 0 }\n",
        "    return descend($value - 1)\n",
        "}\n",
        "descend(8)\n",
    ));
    let depth = (0..64)
        .find(|depth| {
            matches!(
                execute(
                    &recursive,
                    &EvalLimits::pure_v2(
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
            &EvalLimits::pure_v2(CancellationToken::never(), exact)
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));
    let first_excess = ResourceBudget::steps(10_000).with_call_depth(depth - 1);
    let outcome = execute(
        &recursive,
        &EvalLimits::pure_v2(CancellationToken::never(), first_excess),
    );
    assert!(matches!(outcome.primary(), PrimaryOutcome::Error(_)));
}

#[test]
fn interpolated_collection_bytes_refuse_before_the_first_excess_copy() {
    const EXACT: ResourceBudget = ResourceBudget::steps(10_000).with_collection_bytes(12);
    fn assert_copy<T: Copy>() {}
    assert_copy::<ResourceBudget>();

    let strings = program(concat!(
        "language 2\n",
        "let seed = 'abcd'\n",
        "\"$seed$seed\"\n",
    ));
    assert!(matches!(
        execute(
            &strings,
            &EvalLimits::pure_v2(CancellationToken::never(), EXACT),
        )
        .primary(),
        PrimaryOutcome::Completed(_)
    ));
    let outcome = execute(
        &strings,
        &EvalLimits::pure_v2(
            CancellationToken::never(),
            ResourceBudget::steps(10_000).with_collection_bytes(11),
        ),
    );
    assert!(matches!(outcome.primary(), PrimaryOutcome::Error(_)));
}

#[test]
fn cancellation_wins_at_each_polled_schedule_without_becoming_a_budget_error() {
    let program = program(concat!(
        "language 2\n",
        "let values = [1, 2, 3, 4, 5, 6, 7, 8]\n",
        "let total = 0\n",
        "for value in $values { let total = $total + $value }\n",
        "$total\n",
    ));

    for schedule in 0..8 {
        let polls = Arc::new(AtomicUsize::new(0));
        let token = CancellationToken::from_fn({
            let polls = Arc::clone(&polls);
            move || polls.fetch_add(1, Ordering::Relaxed) >= schedule
        });
        let outcome = execute(
            &program,
            &EvalLimits::pure_v2(token, ResourceBudget::steps(10_000)),
        );
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Cancelled(cancelled) if cancelled.reason() == CancelReason::Requested),
            "schedule {schedule} did not preserve cancellation: {:?}",
            outcome.primary()
        );
    }
}
