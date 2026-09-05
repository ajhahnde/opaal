#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flash_platform::{
    Capabilities, ChildProcess, DescriptorEndpoint, FakePlatform, FileActionError, FileOpenRequest,
    PipeEndpoints, PipeError, Platform, ProcessGroup, ProcessGroupId, ProcessStatus, SpawnError,
    SpawnRequest, TerminateError, WaitError,
};
use flash_runtime::builtin::standard_registry;
use flash_runtime::eval::FakeClock;
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModuleOrigin, ModulePathError, ModuleProgram,
    ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader, ValueType,
};
use flash_runtime::outcome::{
    CompletedEvidence, ExecutionOutcome, FatalHostFailure, FatalHostFailureKind, OutcomeEvidence,
    PartialEffectEvidence, PrimaryOutcome, Refusal, RefusalReason, compose_outcome,
};
use flash_runtime::plan::SessionOptions;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::{
    ScriptCompletion, execute_module_program, execute_module_program_outcome,
};
use flash_runtime::seam::{OpaqueSlot, OpaqueSlotState, ProjectId};
use flash_runtime::{Environment, Value};
use flash_syntax::{LanguageMajor, SourceFile, SourceId};

struct FixtureModules;

struct NoExecutables;

#[derive(Default)]
struct ToolExecutable {
    probes: AtomicUsize,
}

#[derive(Debug)]
struct StatusChild {
    code: i32,
    group: Option<ProcessGroupId>,
}

impl ChildProcess for StatusChild {
    fn id(&self) -> u64 {
        70_006
    }

    fn process_group(&self) -> Option<ProcessGroupId> {
        self.group
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        Ok(ProcessStatus::Exited(self.code))
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        Ok(())
    }
}

struct StatusPlatform {
    inner: FakePlatform,
    code: i32,
    spawns: AtomicUsize,
}

impl StatusPlatform {
    const fn new(code: i32) -> Self {
        Self {
            inner: FakePlatform::full(),
            code,
            spawns: AtomicUsize::new(0),
        }
    }
}

impl Platform for StatusPlatform {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        self.inner.pipe()
    }

    fn open_file(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.inner.open_file(request)
    }

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.inner.inherit_descriptor(descriptor)
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        let group = match request.process_group() {
            ProcessGroup::Inherit => None,
            ProcessGroup::New => ProcessGroupId::new(70_006),
            ProcessGroup::Join(group) => Some(group),
        };
        Ok(Box::new(StatusChild {
            code: self.code,
            group,
        }))
    }
}

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}

impl ExecutableProbe for ToolExecutable {
    fn is_executable(&self, path: &OsStr) -> bool {
        self.probes.fetch_add(1, Ordering::SeqCst);
        path == OsStr::new("/bin/tool")
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
fn outcome_manifest_checks_compiled_variants_and_conversion_boundaries() {
    let root = outcome_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();
    let modules = FixtureModules;

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "malformed outcome row {}", index + 1);
        let report = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
            .analyze(&root.join(fields[1]));
        match fields[0] {
            "complete" | "refused" => assert!(
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
                    "{} must fail with {}: {codes:?}; {:?}",
                    fields[1],
                    fields[2],
                    report.issues()
                );
            }
            class => panic!("unknown outcome corpus class {class:?}"),
        }
    }
}

#[test]
fn standard_result_and_option_share_canonical_nominal_identity() {
    let program = load("complete/reference.fsh");
    let root = program.graph().root();
    let result = program
        .resolve_nominal_type(root, &["outcome", "Result"])
        .expect("std::outcome::Result resolves through its alias");
    let option = program
        .resolve_nominal_type(root, &["api", "outcome", "Option"])
        .expect("std::outcome::Option resolves through its deep re-export");
    let direct_option = program
        .resolve_nominal_type(root, &["outcome", "Option"])
        .expect("std::outcome::Option resolves through its direct alias");

    assert_eq!(result.id().name(), "Result");
    assert_eq!(result.type_parameters().len(), 2);
    assert_eq!(
        result
            .variants()
            .iter()
            .map(|variant| variant.name())
            .collect::<Vec<_>>(),
        ["Ok", "Err"]
    );
    assert_eq!(option.id().name(), "Option");
    assert_eq!(option.type_parameters().len(), 1);
    assert_eq!(
        option
            .variants()
            .iter()
            .map(|variant| variant.name())
            .collect::<Vec<_>>(),
        ["Some", "None"]
    );
    assert!(matches!(
        result.id().module().origin(),
        ModuleOrigin::Standard { namespace, module }
            if namespace == "std" && module == "outcome"
    ));
    assert_eq!(result.id().module(), option.id().module());
    assert_eq!(option.id(), direct_option.id());
}

#[test]
fn completed_values_domain_errors_and_statuses_remain_distinct() {
    let reference = execute(
        &load("complete/reference.fsh"),
        &NoExecutables,
        Environment::new(),
    );
    assert_eq!(reference.value(), &Value::Int(2));
    assert!(reference.status().is_none());

    let domain_result = execute(
        &load("complete/domain-error.fsh"),
        &NoExecutables,
        Environment::new(),
    );
    let Value::Variant(result) = domain_result.value() else {
        panic!("a domain Err must remain an ordinary variant value");
    };
    assert_eq!(result.id().name(), "Result");
    assert_eq!(
        result.type_arguments(),
        &[ValueType::Int, ValueType::String]
    );
    assert_eq!(result.constructor(), "Err");
    assert_eq!(result.payload(), &[Value::string("domain")]);
    assert!(domain_result.status().is_none());

    let option = execute(
        &load("complete/option-none.fsh"),
        &NoExecutables,
        Environment::new(),
    );
    let Value::Variant(option_value) = option.value() else {
        panic!("None must remain an ordinary variant value");
    };
    assert_eq!(option_value.id().name(), "Option");
    assert_eq!(option_value.type_arguments(), &[ValueType::Int]);
    assert_eq!(option_value.constructor(), "None");
    assert!(option_value.payload().is_empty());
    assert!(option.status().is_none());

    let status = execute(
        &load("complete/status-success.fsh"),
        &NoExecutables,
        Environment::new(),
    );
    assert_eq!(status.value(), &Value::Null);
    assert_eq!(
        status.status().and_then(flash_runtime::Status::code),
        Some(7)
    );
}

#[test]
fn pure_v2_process_execution_refuses_before_probe_or_spawn() {
    let program = load("refused/process.fsh");
    let probe = ToolExecutable::default();
    let platform = StatusPlatform::new(7);
    let mut environment = Environment::from_snapshot([("PATH", "/bin")]);
    let mut output = Vec::new();
    let outcome = execute_module_program_outcome(
        &program,
        &[],
        &outcome_root(),
        &mut environment,
        &standard_registry(),
        &probe,
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
        &mut output,
    );

    assert!(matches!(
        outcome.primary(),
        PrimaryOutcome::Refused(refusal)
            if refusal.reason() == RefusalReason::Unsupported
                && refusal.operation() == "process execution"
    ));
    assert!(outcome.evidence().is_empty());
    assert_eq!(probe.probes.load(Ordering::SeqCst), 0);
    assert_eq!(platform.spawns.load(Ordering::SeqCst), 0);
    assert!(output.is_empty());
}

#[test]
fn outcome_precedence_retains_one_primary_and_ordered_secondary_evidence() {
    let source = SourceFile::new(SourceId::new(1_701), "matrix.fsh", "effect");
    let span = source.span(0..6).unwrap();
    let primary = PrimaryOutcome::<(), &str>::Refused(Refusal::new(
        RefusalReason::Denied,
        "network request",
        span,
    ));
    let evidence = vec![
        OutcomeEvidence::Completed(CompletedEvidence::new("prepare", None)),
        OutcomeEvidence::PartialEffect(PartialEffectEvidence::new(
            "network request",
            "request bytes accepted",
        )),
        OutcomeEvidence::CleanupFailure("close failed"),
    ];
    let outcome = compose_outcome(Some(primary), evidence).unwrap();

    assert!(outcome.downstream().is_foundation_only());
    let (_, _, downstream) = outcome.clone().into_parts();
    assert!(downstream.is_foundation_only());

    assert!(matches!(
        outcome.primary(),
        PrimaryOutcome::Refused(refusal) if refusal.reason() == RefusalReason::Denied
    ));
    assert!(matches!(
        outcome.evidence(),
        [
            OutcomeEvidence::Completed(completed),
            OutcomeEvidence::PartialEffect(partial),
            OutcomeEvidence::CleanupFailure("close failed"),
        ] if completed.operation() == "prepare"
            && partial.operation() == "network request"
            && partial.detail() == "request bytes accepted"
    ));

    let cleanup_only = compose_outcome::<(), _>(
        None,
        vec![
            OutcomeEvidence::CleanupFailure("primary resource error"),
            OutcomeEvidence::CleanupFailure("secondary resource error"),
        ],
    )
    .unwrap();
    assert_eq!(
        cleanup_only.primary(),
        &PrimaryOutcome::Error("primary resource error")
    );
    assert_eq!(
        cleanup_only.evidence(),
        &[OutcomeEvidence::CleanupFailure("secondary resource error")]
    );

    let unknown_project = OpaqueSlot::<ProjectId>::unknown();
    assert_eq!(unknown_project.state(), OpaqueSlotState::Unknown);
}

#[test]
fn cancellation_refusal_and_fatal_host_failure_are_distinct_primary_tags() {
    let source = SourceFile::new(SourceId::new(1_702), "tags.fsh", "tag");
    let span = source.span(0..3).unwrap();
    let cancellation =
        flash_runtime::eval::Cancellation::new(flash_runtime::eval::CancelReason::Requested, span);
    let cancelled =
        ExecutionOutcome::<(), &str>::new(PrimaryOutcome::Cancelled(cancellation), Vec::new());
    assert!(matches!(
        cancelled.primary(),
        PrimaryOutcome::Cancelled(value)
            if value.reason() == flash_runtime::eval::CancelReason::Requested
    ));

    for reason in [
        RefusalReason::Denied,
        RefusalReason::Unsupported,
        RefusalReason::Unknown,
    ] {
        let refused = ExecutionOutcome::<(), &str>::new(
            PrimaryOutcome::Refused(Refusal::new(reason, "effect", span)),
            Vec::new(),
        );
        assert!(matches!(
            refused.primary(),
            PrimaryOutcome::Refused(value) if value.reason() == reason
        ));
    }

    let fatal = ExecutionOutcome::<(), &str>::new(
        PrimaryOutcome::FatalHostFailure(FatalHostFailure::new(
            FatalHostFailureKind::Reporting,
            "diagnostic sink failed",
        )),
        Vec::new(),
    );
    assert!(matches!(
        fatal.primary(),
        PrimaryOutcome::FatalHostFailure(value)
            if value.kind() == FatalHostFailureKind::Reporting
                && value.message() == "diagnostic sink failed"
    ));
}

fn execute(
    program: &ModuleProgram,
    probe: &dyn ExecutableProbe,
    environment: Environment,
) -> ScriptCompletion {
    execute_on(program, probe, environment, &FakePlatform::full())
}

fn execute_on(
    program: &ModuleProgram,
    probe: &dyn ExecutableProbe,
    mut environment: Environment,
    platform: &dyn Platform,
) -> ScriptCompletion {
    let mut output = Vec::new();
    let completion = execute_module_program(
        program,
        &[],
        &outcome_root(),
        &mut environment,
        &standard_registry(),
        probe,
        &SessionOptions::default(),
        platform,
        Arc::new(FakeClock::new()),
        &mut output,
    )
    .unwrap_or_else(|error| panic!("outcome fixture must execute: {}", error.render()));
    assert!(
        output.is_empty(),
        "non-interactive outcomes do not print implicitly"
    );
    completion
}

fn load(path: &str) -> ModuleProgram {
    let modules = FixtureModules;
    ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
        .load(&outcome_root().join(path))
        .unwrap_or_else(|error| panic!("{path} must load: {error:?}"))
}

fn outcome_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language/outcomes")
}
