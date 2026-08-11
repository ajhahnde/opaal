#![forbid(unsafe_code)]

//! Acceptance coverage for language statuses, conditional command chains, and
//! the plan-time `pipefail` snapshot.

use std::any::Any;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use flash_platform::{
    Capabilities, Capability, ChildProcess, DescriptorEndpoint, FileActionError, FileOpenRequest,
    PipeEndpoints, PipeError, Platform, PlatformError, ProcessGroup, ProcessGroupId, ProcessStatus,
    SpawnError, SpawnRequest, TerminateError, WaitError,
};
use flash_runtime::builtin::standard_registry;
use flash_runtime::command::CommandRegistry;
use flash_runtime::eval::{FakeClock, RuntimeErrorKind};
use flash_runtime::execute::{execute_foreground_chain, execute_foreground_status};
use flash_runtime::plan::{SessionOptions, plan_pipeline_with_options};
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::execute_script;
use flash_runtime::session::BackgroundFailureReason;
use flash_runtime::{Environment, ScopeStack};
use flash_syntax::{
    ConditionalChain, ParseOutcome, Pipeline, SourceFile, SourceId, StatementKind, parse,
};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(1), "test.fsh", text)
}

fn chain(file: &SourceFile) -> ConditionalChain {
    let script = match parse(file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}"),
    };
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("expected a job statement");
    };
    job.chain.clone()
}

fn pipeline(file: &SourceFile) -> Pipeline {
    chain(file).or_terms()[0].and_terms()[0].clone()
}

struct BinProbe;

impl ExecutableProbe for BinProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        matches!(
            path.to_str(),
            Some("/bin/tool" | "/bin/other" | "/bin/third")
        )
    }
}

#[derive(Debug)]
struct TestEndpoint;

impl DescriptorEndpoint for TestEndpoint {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
struct ScriptedPlatform {
    statuses: Vec<ProcessStatus>,
    wait_advances: Vec<u64>,
    clock: FakeClock,
    spawn_count: Arc<AtomicUsize>,
    executables: Arc<Mutex<Vec<PathBuf>>>,
}

impl ScriptedPlatform {
    fn new(statuses: Vec<ProcessStatus>, wait_advances: Vec<u64>, clock: FakeClock) -> Self {
        assert_eq!(statuses.len(), wait_advances.len());
        Self {
            statuses,
            wait_advances,
            clock,
            spawn_count: Arc::new(AtomicUsize::new(0)),
            executables: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn executables(&self) -> Vec<PathBuf> {
        self.executables.lock().expect("executable lock").clone()
    }
}

impl Platform for ScriptedPlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities::full()
    }

    fn shell_executable(&self) -> Result<PathBuf, PlatformError> {
        self.require(Capability::ShellExecutable)?;
        Ok(PathBuf::from("/fake/fsh"))
    }

    fn ignore_hangup(&self) -> Result<(), PlatformError> {
        self.require(Capability::HangupDisposition)
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        Ok(PipeEndpoints::new(
            Box::new(TestEndpoint),
            Box::new(TestEndpoint),
        ))
    }

    fn open_file(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Ok(Box::new(TestEndpoint))
    }

    fn inherit_descriptor(
        &self,
        _descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Ok(Box::new(TestEndpoint))
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        let index = self.spawn_count.fetch_add(1, Ordering::SeqCst);
        self.executables
            .lock()
            .expect("executable lock")
            .push(request.executable().to_owned());
        Ok(Box::new(ScriptedChild {
            id: u64::try_from(index + 1).expect("test child identity fits u64"),
            process_group: match request.process_group() {
                ProcessGroup::Inherit => None,
                ProcessGroup::New => ProcessGroupId::new(
                    u64::try_from(index + 1).expect("test process-group identity fits u64"),
                ),
                ProcessGroup::Join(group) => Some(group),
            },
            status: self.statuses[index],
            wait_advance: self.wait_advances[index],
            clock: self.clock.clone(),
        }))
    }
}

#[derive(Debug)]
struct ScriptedChild {
    id: u64,
    process_group: Option<ProcessGroupId>,
    status: ProcessStatus,
    wait_advance: u64,
    clock: FakeClock,
}

impl ChildProcess for ScriptedChild {
    fn id(&self) -> u64 {
        self.id
    }

    fn process_group(&self) -> Option<ProcessGroupId> {
        self.process_group
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        self.clock.advance(self.wait_advance);
        Ok(self.status)
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        Ok(())
    }
}

fn plan(text: &str, options: &SessionOptions) -> flash_runtime::plan::ExecutionPlan {
    let file = source(text);
    plan_pipeline_with_options(
        &pipeline(&file),
        Path::new("/work"),
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &BinProbe,
        options,
    )
    .expect("pipeline plan should build")
}

#[test]
fn a_plan_snapshots_pipefail_from_session_options() {
    let mut options = SessionOptions::default();
    let default_plan = plan("^tool", &options);
    options.set_pipefail(true);
    let pipefail_plan = plan("^tool", &options);
    options.set_pipefail(false);

    assert!(!default_plan.pipefail());
    assert!(pipefail_plan.pipefail());
}

#[test]
fn script_program_output_uses_the_injected_sink() {
    let clock = FakeClock::new();
    let platform = ScriptedPlatform::new(Vec::new(), Vec::new(), clock.clone());
    let mut environment = Environment::from_snapshot([("PATH", "/bin")]);
    let mut output = Vec::new();

    let completion = execute_script(
        "output.fsh",
        "which pwd | get kind | encode utf8\n",
        Path::new("/work"),
        &mut environment,
        &standard_registry(),
        &BinProbe,
        &SessionOptions::default(),
        &platform,
        Arc::new(clock),
        &mut output,
    )
    .expect("the internal command should write through the supplied sink");

    assert!(completion.background_failures().is_empty());
    assert_eq!(output, b"internal");
}

#[test]
fn script_join_returns_background_failures_in_job_identity_order() {
    let clock = FakeClock::new();
    let platform = ScriptedPlatform::new(
        vec![ProcessStatus::Exited(7), ProcessStatus::Exited(9)],
        vec![1, 1],
        clock.clone(),
    );
    let mut environment = Environment::from_snapshot([("PATH", "/bin")]);
    let mut output = Vec::new();

    let completion = execute_script(
        "jobs.fsh",
        "^tool &\n^other &\n",
        Path::new("/work"),
        &mut environment,
        &standard_registry(),
        &BinProbe,
        &SessionOptions::default(),
        &platform,
        Arc::new(clock),
        &mut output,
    )
    .expect("background failures are completed script outcomes");

    assert_eq!(
        completion.status().and_then(|status| status.code()),
        Some(7),
        "the first failing job still owns final-status precedence"
    );
    assert_eq!(
        completion
            .background_failures()
            .iter()
            .map(|failure| failure.job().get())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(matches!(
        completion.background_failures()[0].reason(),
        BackgroundFailureReason::Exited(status) if status.code() == Some(7)
    ));
    assert!(matches!(
        completion.background_failures()[1].reason(),
        BackgroundFailureReason::Exited(status) if status.code() == Some(9)
    ));
    assert!(output.is_empty());
}

#[test]
fn script_error_retains_joined_background_reports() {
    let clock = FakeClock::new();
    let platform = ScriptedPlatform::new(vec![ProcessStatus::Exited(7)], vec![1], clock.clone());
    let mut environment = Environment::from_snapshot([("PATH", "/bin")]);
    let mut output = Vec::new();

    let error = execute_script(
        "failure.fsh",
        "^tool &\nlet broken = 1 + true\n",
        Path::new("/work"),
        &mut environment,
        &standard_registry(),
        &BinProbe,
        &SessionOptions::default(),
        &platform,
        Arc::new(clock),
        &mut output,
    )
    .expect_err("the foreground runtime failure should remain primary");

    assert!(error.render().starts_with("error[RUN001]"));
    assert_eq!(error.background_failures().len(), 1);
    assert!(matches!(
        error.background_failures()[0].reason(),
        BackgroundFailureReason::Exited(status) if status.code() == Some(7)
    ));
}

#[test]
fn default_aggregation_selects_last_stage_and_retains_leaf_statuses_and_durations() {
    let clock = FakeClock::new();
    let platform = ScriptedPlatform::new(
        vec![
            ProcessStatus::Exited(7),
            ProcessStatus::Exited(4),
            ProcessStatus::Exited(0),
        ],
        vec![10, 10, 10],
        clock.clone(),
    );
    let status = execute_foreground_status(
        &plan("^tool | ^other | ^third", &SessionOptions::default()),
        &platform,
        &clock,
    )
    .expect("pipeline should complete");

    assert_eq!(status.code(), Some(0));
    assert!(status.is_ok());
    assert_eq!(status.duration().as_nanos(), 30);
    assert_eq!(status.stages().len(), 3);
    assert_eq!(status.stages()[0].code(), Some(7));
    assert_eq!(status.stages()[1].code(), Some(4));
    assert_eq!(status.stages()[2].code(), Some(0));
    assert_eq!(status.stages()[0].duration().as_nanos(), 10);
    assert_eq!(status.stages()[1].duration().as_nanos(), 20);
    assert_eq!(status.stages()[2].duration().as_nanos(), 30);
}

#[test]
fn pipefail_selects_the_rightmost_unsuccessful_stage() {
    let clock = FakeClock::new();
    let platform = ScriptedPlatform::new(
        vec![
            ProcessStatus::Exited(7),
            ProcessStatus::Exited(4),
            ProcessStatus::Exited(0),
        ],
        vec![1, 1, 1],
        clock.clone(),
    );
    let status = execute_foreground_status(
        &plan(
            "^tool | ^other | ^third",
            &SessionOptions::default().with_pipefail(true),
        ),
        &platform,
        &clock,
    )
    .expect("pipeline should complete");

    assert_eq!(status.code(), Some(4));
    assert!(!status.is_ok());
}

#[test]
fn one_stage_and_signal_completions_are_leaf_statuses() {
    let clock = FakeClock::new();
    let platform = ScriptedPlatform::new(vec![ProcessStatus::Signaled(15)], vec![9], clock.clone());
    let status = execute_foreground_status(
        &plan("^tool", &SessionOptions::default()),
        &platform,
        &clock,
    )
    .expect("stage should complete");

    assert_eq!(status.code(), None);
    assert_eq!(status.signal().and_then(|signal| signal.number()), Some(15));
    assert!(status.stages().is_empty());
    assert_eq!(status.duration().as_nanos(), 9);
}

#[test]
fn conditional_chains_short_circuit_and_return_the_last_evaluated_status() {
    let file = source("^tool && ^other || ^third");
    let syntax = chain(&file);
    let clock = FakeClock::new();
    let platform = ScriptedPlatform::new(
        vec![ProcessStatus::Exited(2), ProcessStatus::Exited(0)],
        vec![1, 1],
        clock.clone(),
    );
    let status = execute_foreground_chain(
        &syntax,
        Path::new("/work"),
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &BinProbe,
        &SessionOptions::default(),
        &platform,
        &clock,
    )
    .expect("chain should complete");

    assert_eq!(status.code(), Some(0));
    assert_eq!(
        platform.executables(),
        [PathBuf::from("/bin/tool"), PathBuf::from("/bin/third")]
    );
}

#[test]
fn pipefail_changes_status_branching_without_changing_stage_execution() {
    let file = source("^tool | ^other && ^third");
    let syntax = chain(&file);
    let clock = FakeClock::new();
    let platform = ScriptedPlatform::new(
        vec![ProcessStatus::Exited(5), ProcessStatus::Exited(0)],
        vec![1, 1],
        clock.clone(),
    );
    let status = execute_foreground_chain(
        &syntax,
        Path::new("/work"),
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &BinProbe,
        &SessionOptions::default().with_pipefail(true),
        &platform,
        &clock,
    )
    .expect("chain should complete");

    assert_eq!(status.code(), Some(5));
    assert_eq!(platform.executables().len(), 2);
}

#[test]
fn runtime_errors_abort_an_or_chain_instead_of_starting_its_rhs() {
    let file = source("^missing || ^tool");
    let syntax = chain(&file);
    let clock = FakeClock::new();
    let platform = ScriptedPlatform::new(Vec::new(), Vec::new(), clock.clone());
    let error = execute_foreground_chain(
        &syntax,
        Path::new("/work"),
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &BinProbe,
        &SessionOptions::default(),
        &platform,
        &clock,
    )
    .expect_err("resolution failure should abort the chain");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::CommandNotFound { .. }
    ));
    assert!(platform.executables().is_empty());
}

#[test]
fn wait_failures_remain_runtime_errors_in_status_execution() {
    #[derive(Debug)]
    struct WaitFailurePlatform;

    impl Platform for WaitFailurePlatform {
        fn capabilities(&self) -> Capabilities {
            Capabilities::full()
        }

        fn shell_executable(&self) -> Result<PathBuf, PlatformError> {
            self.require(Capability::ShellExecutable)?;
            Ok(PathBuf::from("/fake/fsh"))
        }

        fn ignore_hangup(&self) -> Result<(), PlatformError> {
            self.require(Capability::HangupDisposition)
        }

        fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
            unreachable!()
        }

        fn open_file(
            &self,
            _request: FileOpenRequest<'_>,
        ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
            unreachable!()
        }

        fn inherit_descriptor(
            &self,
            _descriptor: u32,
        ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
            unreachable!()
        }

        fn spawn(&self, _request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
            Ok(Box::new(WaitFailureChild))
        }
    }

    #[derive(Debug)]
    struct WaitFailureChild;

    impl ChildProcess for WaitFailureChild {
        fn id(&self) -> u64 {
            1
        }

        fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
            Err(WaitError::new(io::ErrorKind::Interrupted, "scripted"))
        }

        fn terminate(&mut self) -> Result<(), TerminateError> {
            Ok(())
        }
    }

    let error = execute_foreground_status(
        &plan("^tool", &SessionOptions::default()),
        &WaitFailurePlatform,
        &FakeClock::new(),
    )
    .expect_err("wait failure should remain a runtime error");
    assert!(matches!(error.kind(), RuntimeErrorKind::ProcessWait(_)));
}
