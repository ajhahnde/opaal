#![forbid(unsafe_code)]

//! Acceptance tests for executing one foreground external command from an
//! already-built plan with inherited standard descriptors. Most tests use a
//! recording platform; one exercises the real POSIX adapter and a Rust fixture.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use flashshell_platform::{
    Capabilities, Capability, ChildProcess, DescriptorEndpoint, FakeDescriptorEndpoint,
    FileActionError, FileOpenRequest, PipeEndpoints, PipeError, Platform, ProcessStatus,
    SpawnError, SpawnRequest, TerminateError, WaitError,
};
use flashshell_platform_posix::PosixPlatform;
use flashshell_runtime::command::{Carrier, CommandRegistry, CommandSignature};
use flashshell_runtime::eval::RuntimeErrorKind;
use flashshell_runtime::execute::execute_foreground;
use flashshell_runtime::plan::{ExecutionPlan, plan_pipeline};
use flashshell_runtime::resolve::ExecutableProbe;
use flashshell_runtime::{Environment, ScopeStack};
use flashshell_syntax::{ParseOutcome, Pipeline, SourceFile, SourceId, StatementKind, parse};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(1), "test.fsh", text)
}

fn pipeline(file: &SourceFile) -> Pipeline {
    let script = match parse(file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}"),
    };
    let statement = &script.statements()[0];
    let StatementKind::Job(job) = statement.kind() else {
        panic!("expected a bare command statement");
    };
    job.chain.or_terms()[0].and_terms()[0].clone()
}

struct FakeProbe;

impl ExecutableProbe for FakeProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        path == OsStr::new("/bin/tool") || path == OsStr::new("/bin/other")
    }
}

fn build(text: &str, registry: &CommandRegistry) -> ExecutionPlan {
    let file = source(text);
    let pipeline = pipeline(&file);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin"), ("FLAG", "exact value")]);
    plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut scope,
        &environment,
        registry,
        &FakeProbe,
    )
    .expect("plan should build")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SpawnRecord {
    executable: PathBuf,
    argv: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
    cwd: PathBuf,
}

#[derive(Debug)]
struct RecordingPlatform {
    capabilities: Capabilities,
    record: Mutex<Option<SpawnRecord>>,
    spawn_error: Option<SpawnError>,
    status: ProcessStatus,
    wait_error: Option<WaitError>,
}

impl RecordingPlatform {
    fn successful(status: ProcessStatus) -> Self {
        Self {
            capabilities: Capabilities::full(),
            record: Mutex::new(None),
            spawn_error: None,
            status,
            wait_error: None,
        }
    }

    fn record(&self) -> Option<SpawnRecord> {
        self.record.lock().expect("record lock").clone()
    }
}

impl Platform for RecordingPlatform {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        self.require(Capability::Pipes)?;
        Ok(PipeEndpoints::new(
            Box::new(FakeDescriptorEndpoint),
            Box::new(FakeDescriptorEndpoint),
        ))
    }

    fn open_file(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        Ok(Box::new(FakeDescriptorEndpoint))
    }

    fn inherit_descriptor(
        &self,
        _descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        Ok(Box::new(FakeDescriptorEndpoint))
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        self.require(Capability::ProcessSpawn)?;
        if let Some(error) = &self.spawn_error {
            return Err(error.clone());
        }
        *self.record.lock().expect("record lock") = Some(SpawnRecord {
            executable: request.executable().to_owned(),
            argv: request.argv().to_vec(),
            environment: request.environment().to_vec(),
            cwd: request.cwd().to_owned(),
        });
        Ok(Box::new(TestChild {
            status: self.status,
            wait_error: self.wait_error.clone(),
        }))
    }
}

#[derive(Debug)]
struct TestChild {
    status: ProcessStatus,
    wait_error: Option<WaitError>,
}

impl ChildProcess for TestChild {
    fn id(&self) -> u64 {
        42
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        match &self.wait_error {
            Some(error) => Err(error.clone()),
            None => Ok(self.status),
        }
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        Ok(())
    }
}

#[test]
fn one_external_stage_spawns_the_exact_plan_and_nonzero_is_normal() {
    let plan = build("^tool 'two words' ''", &CommandRegistry::new());
    let platform = RecordingPlatform::successful(ProcessStatus::Exited(7));

    let status = execute_foreground(&plan, &platform).expect("nonzero exit is normal");

    assert_eq!(status, ProcessStatus::Exited(7));
    assert_eq!(
        platform.record(),
        Some(SpawnRecord {
            executable: PathBuf::from("/bin/tool"),
            argv: vec![
                OsString::from("tool"),
                OsString::from("two words"),
                OsString::new(),
            ],
            environment: vec![
                (OsString::from("FLAG"), OsString::from("exact value")),
                (OsString::from("PATH"), OsString::from("/bin")),
            ],
            cwd: PathBuf::from("/work"),
        })
    );
}

#[test]
fn preflight_failure_prevents_spawn_and_keeps_the_argument_span() {
    let plan = build("^tool \"bad\\0arg\"", &CommandRegistry::new());
    let expected_span = plan.stages()[0].argv()[1].span();
    let platform = RecordingPlatform::successful(ProcessStatus::Exited(0));

    let error = execute_foreground(&plan, &platform).expect_err("NUL must fail preflight");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::ArgumentContainsNul
    ));
    assert_eq!(error.span(), expected_span);
    assert_eq!(platform.record(), None);
}

#[test]
fn unsupported_process_spawn_is_anchored_on_the_command_word() {
    let plan = build("^tool", &CommandRegistry::new());
    let expected_span = plan.stages()[0].argv()[0].span();
    let platform = RecordingPlatform {
        capabilities: Capabilities::empty(),
        ..RecordingPlatform::successful(ProcessStatus::Exited(0))
    };

    let error = execute_foreground(&plan, &platform).expect_err("spawn is unsupported");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::ProcessSpawn(SpawnError::Platform(_))
    ));
    assert_eq!(error.span(), expected_span);
    assert_eq!(platform.record(), None);
}

#[test]
fn host_spawn_failure_is_anchored_on_the_command_word() {
    let plan = build("^tool", &CommandRegistry::new());
    let expected_span = plan.stages()[0].argv()[0].span();
    let platform = RecordingPlatform {
        spawn_error: Some(SpawnError::Operation {
            kind: io::ErrorKind::PermissionDenied,
            message: "permission denied".to_owned(),
        }),
        ..RecordingPlatform::successful(ProcessStatus::Exited(0))
    };

    let error = execute_foreground(&plan, &platform).expect_err("spawn must fail");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::ProcessSpawn(SpawnError::Operation {
            kind: io::ErrorKind::PermissionDenied,
            ..
        })
    ));
    assert_eq!(error.span(), expected_span);
}

#[test]
fn wait_failure_is_anchored_on_the_stage() {
    let plan = build("^tool", &CommandRegistry::new());
    let expected_span = plan.stages()[0].span();
    let platform = RecordingPlatform {
        wait_error: Some(WaitError::new(
            io::ErrorKind::Interrupted,
            "wait interrupted",
        )),
        ..RecordingPlatform::successful(ProcessStatus::Exited(0))
    };

    let error = execute_foreground(&plan, &platform).expect_err("wait must fail");

    assert!(matches!(error.kind(), RuntimeErrorKind::ProcessWait(_)));
    assert_eq!(error.span(), expected_span);
    assert!(platform.record().is_some());
}

#[test]
fn pipeline_and_internal_plans_are_rejected_before_spawn() {
    let mut registry = CommandRegistry::new();
    registry.register(CommandSignature::new(
        "inside",
        [Carrier::ByteStream],
        Carrier::ByteStream,
    ));
    let cases = [
        build("^tool | ^other", &registry),
        build("inside", &registry),
    ];

    for plan in cases {
        let platform = RecordingPlatform::successful(ProcessStatus::Exited(0));
        let error = execute_foreground(&plan, &platform).expect_err("plan is deferred");
        assert!(matches!(error.kind(), RuntimeErrorKind::Unsupported { .. }));
        assert_eq!(platform.record(), None);
    }
}

#[test]
fn a_signalled_child_remains_a_normal_platform_completion() {
    let plan = build("^tool", &CommandRegistry::new());
    let platform = RecordingPlatform::successful(ProcessStatus::Signaled(15));

    assert_eq!(
        execute_foreground(&plan, &platform),
        Ok(ProcessStatus::Signaled(15))
    );
}

#[test]
fn a_real_posix_status_fixture_reports_exit_and_signal_completion() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-status-fixture"));
    let fixture_dir = fixture.parent().expect("fixture has a parent").to_owned();
    let environment =
        Environment::from_snapshot([("PATH", fixture_dir.as_os_str().to_os_string())]);

    for (arguments, expected) in [
        ("exit 23", ProcessStatus::Exited(23)),
        ("signal", ProcessStatus::Signaled(6)),
    ] {
        let file = source(&format!("^flashshell-status-fixture {arguments}"));
        let plan = plan_pipeline(
            &pipeline(&file),
            &fixture_dir,
            &file,
            &mut ScopeStack::new(),
            &environment,
            &CommandRegistry::new(),
            &ExactProbe(fixture.clone()),
        )
        .expect("real fixture plan should build");

        assert_eq!(execute_foreground(&plan, &PosixPlatform), Ok(expected));
    }
}

struct ExactProbe(PathBuf);

impl ExecutableProbe for ExactProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        path == self.0.as_os_str()
    }
}
