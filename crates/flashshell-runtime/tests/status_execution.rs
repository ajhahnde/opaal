#![forbid(unsafe_code)]

//! Acceptance coverage for language statuses, conditional command chains, and
//! the plan-time `pipefail` snapshot.

use std::any::Any;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use flashshell_platform::{
    Capabilities, ChildProcess, DescriptorEndpoint, FileActionError, FileOpenRequest,
    PipeEndpoints, PipeError, Platform, ProcessStatus, SpawnError, SpawnRequest, TerminateError,
    WaitError,
};
use flashshell_runtime::command::CommandRegistry;
use flashshell_runtime::eval::{FakeClock, RuntimeErrorKind};
use flashshell_runtime::execute::{execute_foreground_chain, execute_foreground_status};
use flashshell_runtime::plan::{SessionOptions, plan_pipeline_with_options};
use flashshell_runtime::resolve::ExecutableProbe;
use flashshell_runtime::{Environment, ScopeStack};
use flashshell_syntax::{
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
            status: self.statuses[index],
            wait_advance: self.wait_advances[index],
            clock: self.clock.clone(),
        }))
    }
}

#[derive(Debug)]
struct ScriptedChild {
    status: ProcessStatus,
    wait_advance: u64,
    clock: FakeClock,
}

impl ChildProcess for ScriptedChild {
    fn id(&self) -> u64 {
        1
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        self.clock.advance(self.wait_advance);
        Ok(self.status)
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        Ok(())
    }
}

fn plan(text: &str, options: &SessionOptions) -> flashshell_runtime::plan::ExecutionPlan {
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
