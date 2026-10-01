use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use opaal_cli::check::HostCheckFilesystem;
use opaal_platform::operational::{
    AtomicWriteRequest, FakeOperationalAdapter, HttpRequest, HttpResponse, OperationalAdapter,
    OperationalCall, OperationalError, OperationalErrorKind, ProcessOutput, ProcessRequest,
    ReadFileRequest,
};
use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityProfile, Capabilities, FakePlatform,
};
use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule, AuthorityVerdict, CapabilityRequest, EffectSet,
    EvaluationContextId, RequiredEnforcement,
};
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::{CancellationToken, FakeClock};
use opaal_runtime::module::ActionId;
use opaal_runtime::operational::process::ProcessBudget;
use opaal_runtime::operational::source::{
    ControlledSourceOperations, SourceActionOutcome, SourceEffectEvent, SourceEffectJournal,
    SourceEffectOutcome,
};
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::project::{load_project_program, parse_project_manifest, parse_tool_lock};
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::{ScriptExecutionOutcome, execute_project_task_outcome};
use opaal_runtime::{Environment, Value};

use super::support::{ReportDir, fixture};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fault {
    None,
    Read,
    WriteBeforeRename,
    DurabilityAfterRename,
    JournalAfterWrite,
    CancelBefore,
    CancelAfterWrite,
}

struct Adapter {
    inner: FakeOperationalAdapter,
    fault: Fault,
    cancelled: Arc<AtomicBool>,
}

fn injected_error(message: &str) -> OperationalError {
    OperationalError::new(OperationalErrorKind::Io(io::ErrorKind::Other), message)
}

impl OperationalAdapter for Adapter {
    fn read_file(&self, request: ReadFileRequest<'_>) -> Result<Vec<u8>, OperationalError> {
        if self.fault == Fault::Read {
            return Err(injected_error("injected read failure"));
        }
        self.inner.read_file(request)
    }
    fn write_atomic(&self, request: AtomicWriteRequest<'_>) -> Result<(), OperationalError> {
        if self.fault == Fault::WriteBeforeRename {
            return Err(injected_error("injected failure before rename"));
        }
        self.inner.write_atomic(request)?;
        if self.fault == Fault::DurabilityAfterRename {
            return Err(injected_error("injected durability failure after rename"));
        }
        if self.fault == Fault::CancelAfterWrite {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn wall_time_unix_nanos(&self) -> Result<i128, OperationalError> {
        panic!("report cannot read the clock")
    }
    fn monotonic_nanos(&self) -> Result<u128, OperationalError> {
        panic!("report cannot read the clock")
    }
    fn http_request(
        &self,
        _: HttpRequest<'_>,
        _: &dyn Fn() -> bool,
    ) -> Result<HttpResponse, OperationalError> {
        panic!("report cannot use the network")
    }
    fn run_process(
        &self,
        _: ProcessRequest<'_>,
        _: &dyn Fn() -> bool,
    ) -> Result<ProcessOutput, OperationalError> {
        panic!("report cannot start a process")
    }
}

#[derive(Default)]
pub struct Journal {
    pub effects: Vec<(String, SourceEffectOutcome)>,
    pub ended: Option<SourceActionOutcome>,
    pub before: Vec<String>,
    fail_after_write: bool,
}

impl SourceEffectJournal for Journal {
    fn action_start(&mut self, _: &str, _: &ActionId) -> Result<(), String> {
        Ok(())
    }
    fn action_end(&mut self, _: &str, outcome: &SourceActionOutcome) -> Result<(), String> {
        self.ended = Some(outcome.clone());
        Ok(())
    }
    fn before(
        &mut self,
        event: &SourceEffectEvent,
        verdict: AuthorityVerdict,
    ) -> Result<(), String> {
        assert!(verdict.is_executable());
        self.before.push(format!("{:?}", event.request().effect()));
        Ok(())
    }
    fn after(
        &mut self,
        event: &SourceEffectEvent,
        outcome: &SourceEffectOutcome,
    ) -> Result<(), String> {
        if event.request().effect() == AuthorityEffect::FilesystemWrite && self.fail_after_write {
            return Err("injected journal failure after write".to_owned());
        }
        self.effects
            .push((format!("{:?}", event.request().effect()), outcome.clone()));
        Ok(())
    }
}

struct NoExecutables;
impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("controlled report cannot resolve an executable")
    }
}

pub fn run(
    fault: Fault,
) -> (
    ScriptExecutionOutcome,
    Journal,
    Option<Vec<u8>>,
    Vec<OperationalCall>,
) {
    let work = ReportDir::new();
    let root = &work.0;
    let manifest = parse_project_manifest(
        &root.join("opaal.toml"),
        &std::fs::read(root.join("opaal.toml")).unwrap(),
    )
    .unwrap();
    let tools = parse_tool_lock(
        &manifest,
        "ci",
        &std::fs::read(root.join("tools.toml")).unwrap(),
    )
    .unwrap();
    let filesystem = HostCheckFilesystem;
    let project = load_project_program(manifest, &filesystem, &filesystem).unwrap();
    let task = project.task("summarize").unwrap();
    let effects = EffectSet::new([
        CapabilityRequest::filesystem_read(root).unwrap(),
        CapabilityRequest::filesystem_write(root.join("report.json")).unwrap(),
    ]);
    let rules = [
        CapabilityRequest::filesystem_read(root).unwrap(),
        CapabilityRequest::filesystem_write(root.join("report.json")).unwrap(),
    ]
    .map(|request| AuthorityRule::grant(request, RequiredEnforcement::Enforced));
    let cancelled = Arc::new(AtomicBool::new(fault == Fault::CancelBefore));
    let cancellation = CancellationToken::from_fn({
        let cancelled = Arc::clone(&cancelled);
        move || cancelled.load(Ordering::SeqCst)
    });
    let clock = Arc::new(FakeClock::new());
    let mut context = OperationalContext::new(
        AuthorityContext::new(EvaluationContextId::new(1).unwrap(), rules).unwrap(),
        cancellation.clone(),
        clock.clone(),
        None,
    );
    let platform = FakePlatform::with_authority_profile(
        Capabilities::full(),
        AuthorityProfile::unsupported()
            .with(
                AuthorityEffect::FilesystemRead,
                AuthorityEnforcement::Enforced,
            )
            .with(
                AuthorityEffect::FilesystemWrite,
                AuthorityEnforcement::Enforced,
            ),
    );
    let adapter = Adapter {
        inner: FakeOperationalAdapter::new(),
        fault,
        cancelled,
    };
    adapter
        .inner
        .insert_file(root.join("jobs.json"), fixture("jobs.json"));
    let mut journal = Journal {
        fail_after_write: fault == Fault::JournalAfterWrite,
        ..Journal::default()
    };
    let nodes = BTreeMap::from([(
        task.action().id().clone(),
        format!("sha256:{}#000000", task.action().id().contract_digest()),
    )]);
    let executable_files = BTreeMap::new();
    let tls_ca = BTreeMap::new();
    let outcome = {
        let mut operations = ControlledSourceOperations::new(
            &mut context,
            &effects,
            project.manifest(),
            &tools,
            &executable_files,
            &platform,
            &adapter,
            &tls_ca,
            &mut journal,
            nodes,
            BTreeMap::new(),
            ProcessBudget::default(),
            None,
        );
        execute_project_task_outcome(
            &project,
            task,
            vec![Value::Path(opaal_runtime::NativePath::new(
                root.join("jobs.json"),
            ))],
            root,
            &mut Environment::new(),
            &standard_registry(),
            &NoExecutables,
            &SessionOptions::default(),
            &platform,
            clock,
            cancellation,
            &mut operations,
            &mut Vec::new(),
        )
    };
    (
        outcome,
        journal,
        adapter.inner.file(&root.join("report.json")),
        adapter.inner.calls(),
    )
}
