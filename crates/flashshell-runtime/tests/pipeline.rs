#![forbid(unsafe_code)]

//! Acceptance tests for arbitrary-length external foreground byte pipelines.

use std::any::Any;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use flashshell_platform::{
    Capabilities, Capability, ChildProcess, DescriptorEndpoint, FAKE_STOP_SIGNAL, FakePlatform,
    FileActionError, FileOpenRequest, JobSignal, PipeEndpoints, PipeError, Platform, PlatformError,
    ProcessGroup, ProcessStatus, RecordingPlatform, SignalError, SpawnError, SpawnRequest,
    TerminateError, WaitError,
};
use flashshell_platform_posix::PosixPlatform;
use flashshell_runtime::command::CommandRegistry;
use flashshell_runtime::eval::{Clock, Instant, RuntimeErrorKind};
use flashshell_runtime::execute::{execute_foreground_pipeline, start_background_pipeline};
use flashshell_runtime::plan::{ExecutionPlan, RedirectionAction, plan_pipeline};
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
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("expected a bare command statement");
    };
    job.chain.or_terms()[0].and_terms()[0].clone()
}

struct BinProbe;

impl ExecutableProbe for BinProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        matches!(path.to_str(), Some("/bin/tool" | "/bin/other"))
    }
}

fn build(text: &str) -> ExecutionPlan {
    let file = source(text);
    let syntax = pipeline(&file);
    plan_pipeline(
        &syntax,
        "/work",
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &BinProbe,
    )
    .expect("pipeline plan should build")
}

#[derive(Debug)]
struct TestEndpoint {
    id: usize,
    drops: Arc<AtomicUsize>,
}

impl Drop for TestEndpoint {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl DescriptorEndpoint for TestEndpoint {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SpawnRecord {
    executable: PathBuf,
    descriptors: Vec<(u32, usize)>,
    closed_descriptors: Vec<u32>,
}

#[derive(Debug)]
struct RecordingPipelinePlatform {
    capabilities: Capabilities,
    pipe_count: AtomicUsize,
    file_action_count: AtomicUsize,
    spawn_count: Arc<AtomicUsize>,
    endpoint_drops: Arc<AtomicUsize>,
    records: Arc<Mutex<Vec<SpawnRecord>>>,
    wait_order: Arc<Mutex<Vec<usize>>>,
    terminate_order: Arc<Mutex<Vec<usize>>>,
    statuses: Vec<ProcessStatus>,
    expected_spawn_calls: usize,
    expected_endpoint_drops: usize,
    pipe_error_at: Option<usize>,
    file_action_error_at: Option<usize>,
    spawn_error_at: Option<usize>,
    wait_error_at: Option<usize>,
    report_process_groups: bool,
}

impl RecordingPipelinePlatform {
    fn new(statuses: Vec<ProcessStatus>) -> Self {
        let expected_spawn_calls = statuses.len();
        let expected_endpoint_drops = statuses.len().saturating_sub(1) * 2;
        Self {
            capabilities: Capabilities::full(),
            pipe_count: AtomicUsize::new(0),
            file_action_count: AtomicUsize::new(0),
            spawn_count: Arc::new(AtomicUsize::new(0)),
            endpoint_drops: Arc::new(AtomicUsize::new(0)),
            records: Arc::new(Mutex::new(Vec::new())),
            wait_order: Arc::new(Mutex::new(Vec::new())),
            terminate_order: Arc::new(Mutex::new(Vec::new())),
            statuses,
            expected_spawn_calls,
            expected_endpoint_drops,
            pipe_error_at: None,
            file_action_error_at: None,
            spawn_error_at: None,
            wait_error_at: None,
            report_process_groups: true,
        }
    }
}

impl Platform for RecordingPipelinePlatform {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn shell_executable(&self) -> Result<PathBuf, PlatformError> {
        self.require(Capability::ShellExecutable)?;
        Ok(PathBuf::from("/fake/fsh"))
    }

    fn ignore_hangup(&self) -> Result<(), PlatformError> {
        self.require(Capability::HangupDisposition)
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        self.require(Capability::Pipes)?;
        let index = self.pipe_count.fetch_add(1, Ordering::SeqCst);
        if self.pipe_error_at == Some(index) {
            return Err(PipeError::Operation {
                kind: io::ErrorKind::ResourceBusy,
                message: "scripted pipe failure".to_owned(),
            });
        }
        Ok(PipeEndpoints::new(
            Box::new(TestEndpoint {
                id: index * 2,
                drops: Arc::clone(&self.endpoint_drops),
            }),
            Box::new(TestEndpoint {
                id: index * 2 + 1,
                drops: Arc::clone(&self.endpoint_drops),
            }),
        ))
    }

    fn open_file(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        let index = self.file_action_count.fetch_add(1, Ordering::SeqCst);
        if self.file_action_error_at == Some(index) {
            return Err(FileActionError::Operation {
                kind: io::ErrorKind::PermissionDenied,
                message: "scripted file-action failure".to_owned(),
            });
        }
        Ok(Box::new(TestEndpoint {
            id: 1_000 + index,
            drops: Arc::clone(&self.endpoint_drops),
        }))
    }

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        Ok(Box::new(TestEndpoint {
            id: descriptor as usize,
            drops: Arc::clone(&self.endpoint_drops),
        }))
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        self.require(Capability::ProcessSpawn)?;
        let index = self.spawn_count.fetch_add(1, Ordering::SeqCst);
        if self.spawn_error_at == Some(index) {
            return Err(SpawnError::Operation {
                kind: io::ErrorKind::PermissionDenied,
                message: "scripted spawn failure".to_owned(),
            });
        }
        let descriptors = request
            .descriptors()
            .iter()
            .map(|mapping| {
                let endpoint = mapping
                    .endpoint()
                    .as_any()
                    .downcast_ref::<TestEndpoint>()
                    .expect("recording platform receives its own endpoint");
                (mapping.target(), endpoint.id)
            })
            .collect();
        self.records.lock().expect("record lock").push(SpawnRecord {
            executable: request.executable().to_owned(),
            descriptors,
            closed_descriptors: request.closed_descriptors().to_vec(),
        });
        Ok(Box::new(RecordingChild {
            index,
            id: index as u64 + 1,
            process_group: self
                .report_process_groups
                .then_some(request.process_group())
                .and_then(|placement| match placement {
                    ProcessGroup::New => flashshell_platform::ProcessGroupId::new(index as u64 + 1),
                    ProcessGroup::Join(group) => Some(group),
                    ProcessGroup::Inherit => None,
                }),
            status: self.statuses[index],
            wait_error: self.wait_error_at == Some(index),
            expected_spawns: self.expected_spawn_calls,
            expected_endpoint_drops: self.expected_endpoint_drops,
            spawn_count: Arc::clone(&self.spawn_count),
            endpoint_drops: Arc::clone(&self.endpoint_drops),
            wait_order: Arc::clone(&self.wait_order),
            terminate_order: Arc::clone(&self.terminate_order),
        }))
    }
}

#[derive(Debug)]
struct RecordingChild {
    index: usize,
    id: u64,
    process_group: Option<flashshell_platform::ProcessGroupId>,
    status: ProcessStatus,
    wait_error: bool,
    expected_spawns: usize,
    expected_endpoint_drops: usize,
    spawn_count: Arc<AtomicUsize>,
    endpoint_drops: Arc<AtomicUsize>,
    wait_order: Arc<Mutex<Vec<usize>>>,
    terminate_order: Arc<Mutex<Vec<usize>>>,
}

impl ChildProcess for RecordingChild {
    fn id(&self) -> u64 {
        self.id
    }

    fn process_group(&self) -> Option<flashshell_platform::ProcessGroupId> {
        self.process_group
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        assert_eq!(
            self.spawn_count.load(Ordering::SeqCst),
            self.expected_spawns,
            "every stage must spawn before the first wait",
        );
        assert_eq!(
            self.endpoint_drops.load(Ordering::SeqCst),
            self.expected_endpoint_drops,
            "the parent must release every pipe endpoint before waiting",
        );
        self.wait_order.lock().expect("wait lock").push(self.index);
        if self.wait_error {
            Err(WaitError::new(
                io::ErrorKind::Interrupted,
                "scripted wait failure",
            ))
        } else {
            Ok(self.status)
        }
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        self.terminate_order
            .lock()
            .expect("terminate lock")
            .push(self.index);
        Ok(())
    }
}

#[derive(Debug)]
struct StepClock {
    next: AtomicU64,
}

impl StepClock {
    const fn new(first: u64) -> Self {
        Self {
            next: AtomicU64::new(first),
        }
    }
}

impl Clock for StepClock {
    fn now(&self) -> Instant {
        Instant::from_nanos(self.next.fetch_add(1, Ordering::SeqCst))
    }
}

#[test]
fn background_pipeline_returns_owned_grouped_members_without_waiting() {
    let plan = build("^tool | ^other");
    let platform =
        RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(7), ProcessStatus::Exited(9)]);
    let clock = StepClock::new(100);

    let started = start_background_pipeline(&plan, &platform, &clock)
        .expect("the background pipeline should start");

    let group = started.group();
    assert_eq!(group.get(), 1);
    assert_eq!(started.started_at(), Instant::from_nanos(100));
    assert_eq!(
        started
            .members()
            .iter()
            .map(|member| (member.process().get(), member.started_at()))
            .collect::<Vec<_>>(),
        vec![(1, Instant::from_nanos(101)), (2, Instant::from_nanos(102)),]
    );
    assert!(
        platform.wait_order.lock().expect("wait lock").is_empty(),
        "background startup must return every child without waiting"
    );

    for member in started.into_members() {
        let (_, mut child, _) = member.into_parts();
        assert_eq!(child.process_group(), Some(group));
        child.wait().expect("test cleanup should reap the child");
    }
    assert_eq!(*platform.wait_order.lock().expect("wait lock"), vec![0, 1]);
}

#[test]
fn background_pipeline_requires_process_groups_before_platform_work() {
    let plan = build("^tool | ^other");
    let platform = RecordingPipelinePlatform {
        capabilities: Capabilities::full_without(Capability::ProcessGroups),
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0); 2])
    };

    let error = start_background_pipeline(&plan, &platform, &StepClock::new(0))
        .expect_err("background startup requires process groups");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::BackgroundProcessGroupUnavailable
    ));
    assert_eq!(platform.pipe_count.load(Ordering::SeqCst), 0);
    assert_eq!(platform.spawn_count.load(Ordering::SeqCst), 0);
}

#[test]
fn background_pipe_creation_failure_spawns_no_child() {
    let plan = build("^tool | ^other | ^tool");
    let platform = RecordingPipelinePlatform {
        pipe_error_at: Some(1),
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0); 3])
    };

    let error = start_background_pipeline(&plan, &platform, &StepClock::new(0))
        .expect_err("the second pipe should fail");

    assert!(matches!(error.kind(), RuntimeErrorKind::PipeCreate(_)));
    assert_eq!(platform.spawn_count.load(Ordering::SeqCst), 0);
    assert!(platform.wait_order.lock().expect("wait lock").is_empty());
    assert!(
        platform
            .terminate_order
            .lock()
            .expect("terminate lock")
            .is_empty()
    );
}

#[test]
fn background_later_spawn_failure_terminates_and_reaps_started_members() {
    let plan = build("^tool | ^other | ^tool");
    let platform = RecordingPipelinePlatform {
        expected_spawn_calls: 2,
        spawn_error_at: Some(1),
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0); 3])
    };

    let error = start_background_pipeline(&plan, &platform, &StepClock::new(0))
        .expect_err("the second spawn should fail");

    assert!(matches!(error.kind(), RuntimeErrorKind::ProcessSpawn(_)));
    assert_eq!(
        *platform.terminate_order.lock().expect("terminate lock"),
        vec![0]
    );
    assert_eq!(*platform.wait_order.lock().expect("wait lock"), vec![0]);
}

#[test]
fn background_unestablished_group_terminates_and_reaps_the_started_child() {
    let plan = build("^tool | ^other");
    let platform = RecordingPipelinePlatform {
        expected_spawn_calls: 1,
        expected_endpoint_drops: 2,
        report_process_groups: false,
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0); 2])
    };

    let error = start_background_pipeline(&plan, &platform, &StepClock::new(0))
        .expect_err("background startup requires an established process group");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::BackgroundProcessGroupUnavailable
    ));
    assert_eq!(
        *platform.terminate_order.lock().expect("terminate lock"),
        vec![0]
    );
    assert_eq!(*platform.wait_order.lock().expect("wait lock"), vec![0]);
}

#[test]
fn arbitrary_pipeline_spawns_every_stage_then_returns_ordered_statuses() {
    let plan = build("^tool | ^other | ^tool | ^other");
    let statuses = vec![
        ProcessStatus::Exited(7),
        ProcessStatus::Exited(0),
        ProcessStatus::Signaled(13),
        ProcessStatus::Exited(9),
    ];
    let platform = RecordingPipelinePlatform::new(statuses.clone());

    assert_eq!(execute_foreground_pipeline(&plan, &platform), Ok(statuses));
    assert_eq!(
        *platform.records.lock().expect("record lock"),
        vec![
            SpawnRecord {
                executable: PathBuf::from("/bin/tool"),
                descriptors: vec![(1, 1)],
                closed_descriptors: vec![],
            },
            SpawnRecord {
                executable: PathBuf::from("/bin/other"),
                descriptors: vec![(0, 0), (1, 3)],
                closed_descriptors: vec![],
            },
            SpawnRecord {
                executable: PathBuf::from("/bin/tool"),
                descriptors: vec![(0, 2), (1, 5)],
                closed_descriptors: vec![],
            },
            SpawnRecord {
                executable: PathBuf::from("/bin/other"),
                descriptors: vec![(0, 4)],
                closed_descriptors: vec![],
            },
        ]
    );
    assert_eq!(
        *platform.wait_order.lock().expect("wait lock"),
        vec![0, 1, 2, 3]
    );
}

#[test]
fn pipe_both_maps_stdout_and_stderr_to_the_same_owned_endpoint() {
    let plan = build("^tool |& ^other");
    let platform =
        RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0), ProcessStatus::Exited(0)]);

    execute_foreground_pipeline(&plan, &platform).expect("pipeline should complete");

    assert_eq!(
        platform.records.lock().expect("record lock")[0].descriptors,
        vec![(1, 1), (2, 1)]
    );
}

#[test]
fn redirections_apply_left_to_right_with_aliasing_and_an_explicit_close() {
    let plan = build("^tool >first 2>&1 1>second 3>&2 2>&-");
    let platform = RecordingPipelinePlatform {
        expected_endpoint_drops: 2,
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0)])
    };

    assert_eq!(
        execute_foreground_pipeline(&plan, &platform),
        Ok(vec![ProcessStatus::Exited(0)])
    );
    assert_eq!(
        *platform.records.lock().expect("record lock"),
        vec![SpawnRecord {
            executable: PathBuf::from("/bin/tool"),
            descriptors: vec![(1, 1_001), (3, 1_000)],
            closed_descriptors: vec![2],
        }]
    );
    assert_eq!(platform.endpoint_drops.load(Ordering::SeqCst), 2);
}

#[test]
fn unsupported_file_actions_fail_at_the_target_before_spawn() {
    let plan = build("^tool >target");
    let RedirectionAction::Output { target, .. } = plan.stages()[0].redirections()[0].action()
    else {
        panic!("expected output redirection");
    };
    let expected_span = target.span();
    let platform = RecordingPipelinePlatform {
        capabilities: Capabilities::empty().with(Capability::ProcessSpawn),
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0)])
    };

    let error =
        execute_foreground_pipeline(&plan, &platform).expect_err("file actions are unsupported");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::RedirectionSetup(_)
    ));
    assert_eq!(error.span(), expected_span);
    assert_eq!(platform.spawn_count.load(Ordering::SeqCst), 0);
}

#[test]
fn later_redirection_failure_terminates_and_reaps_started_siblings() {
    let plan = build("^tool | ^other >missing");
    let RedirectionAction::Output { target, .. } = plan.stages()[1].redirections()[0].action()
    else {
        panic!("expected output redirection");
    };
    let expected_span = target.span();
    let platform = RecordingPipelinePlatform {
        expected_spawn_calls: 1,
        file_action_error_at: Some(0),
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0); 2])
    };

    let error = execute_foreground_pipeline(&plan, &platform)
        .expect_err("the second stage file action should fail");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::RedirectionSetup(_)
    ));
    assert_eq!(error.span(), expected_span);
    assert_eq!(platform.spawn_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        *platform.terminate_order.lock().expect("terminate lock"),
        vec![0]
    );
    assert_eq!(*platform.wait_order.lock().expect("wait lock"), vec![0]);
}

#[test]
fn unsupported_pipes_fail_at_the_first_edge_before_spawn() {
    let plan = build("^tool | ^other");
    let expected_span = plan.edges()[0].operator_span();
    let platform = RecordingPipelinePlatform {
        capabilities: Capabilities::empty().with(Capability::ProcessSpawn),
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0), ProcessStatus::Exited(0)])
    };

    let error = execute_foreground_pipeline(&plan, &platform).expect_err("pipes are unsupported");

    assert!(matches!(error.kind(), RuntimeErrorKind::PipeCreate(_)));
    assert_eq!(error.span(), expected_span);
    assert_eq!(platform.spawn_count.load(Ordering::SeqCst), 0);
}

#[test]
fn pipe_creation_failure_releases_earlier_edges_before_any_spawn() {
    let plan = build("^tool | ^other | ^tool");
    let expected_span = plan.edges()[1].operator_span();
    let platform = RecordingPipelinePlatform {
        pipe_error_at: Some(1),
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0); 3])
    };

    let error = execute_foreground_pipeline(&plan, &platform).expect_err("second pipe should fail");

    assert!(matches!(error.kind(), RuntimeErrorKind::PipeCreate(_)));
    assert_eq!(error.span(), expected_span);
    assert_eq!(platform.spawn_count.load(Ordering::SeqCst), 0);
    assert_eq!(platform.endpoint_drops.load(Ordering::SeqCst), 2);
}

#[test]
fn spawn_failure_closes_all_parent_endpoints_and_reaps_started_siblings() {
    let plan = build("^tool | ^other | ^tool | ^other");
    let expected_span = plan.stages()[2].argv()[0].span();
    let platform = RecordingPipelinePlatform {
        expected_spawn_calls: 3,
        spawn_error_at: Some(2),
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0); 4])
    };

    let error = execute_foreground_pipeline(&plan, &platform).expect_err("third spawn should fail");

    assert!(matches!(error.kind(), RuntimeErrorKind::ProcessSpawn(_)));
    assert_eq!(error.span(), expected_span);
    assert_eq!(platform.endpoint_drops.load(Ordering::SeqCst), 6);
    assert_eq!(*platform.wait_order.lock().expect("wait lock"), vec![0, 1]);
    assert_eq!(
        *platform.terminate_order.lock().expect("terminate lock"),
        vec![0, 1]
    );
}

#[test]
fn wait_failure_keeps_its_stage_span_and_does_not_skip_later_reaping() {
    let plan = build("^tool | ^other | ^tool");
    let expected_span = plan.stages()[1].span();
    let platform = RecordingPipelinePlatform {
        wait_error_at: Some(1),
        ..RecordingPipelinePlatform::new(vec![ProcessStatus::Exited(0); 3])
    };

    let error = execute_foreground_pipeline(&plan, &platform).expect_err("second wait should fail");

    assert!(matches!(error.kind(), RuntimeErrorKind::ProcessWait(_)));
    assert_eq!(error.span(), expected_span);
    assert_eq!(
        *platform.wait_order.lock().expect("wait lock"),
        vec![0, 1, 2]
    );
}

#[test]
fn real_posix_pipeline_moves_more_than_a_pipe_buffer_without_capture() {
    const LENGTH: usize = 4 * 1024 * 1024;
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    let name = fixture.file_name().expect("fixture has a name");
    let text = format!(
        "^{0} source {LENGTH} 7 | ^{0} relay 11 | ^{0} relay 13 | ^{0} sink {LENGTH} 17",
        name.to_string_lossy(),
    );
    let file = source(&text);
    let syntax = pipeline(&file);
    let plan = plan_pipeline(
        &syntax,
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture),
    )
    .expect("real pipeline plan should build");

    assert_eq!(
        execute_foreground_pipeline(&plan, &PosixPlatform),
        Ok(vec![
            ProcessStatus::Exited(7),
            ProcessStatus::Exited(11),
            ProcessStatus::Exited(13),
            ProcessStatus::Exited(17),
        ])
    );
}

#[test]
fn real_posix_pipe_both_merges_stdout_and_stderr() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    let name = fixture.file_name().expect("fixture has a name");
    let text = format!("^{0} both 3 |& ^{0} sink 4 5", name.to_string_lossy(),);
    let file = source(&text);
    let syntax = pipeline(&file);
    let plan = plan_pipeline(
        &syntax,
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture),
    )
    .expect("real merged pipeline plan should build");

    assert_eq!(
        execute_foreground_pipeline(&plan, &PosixPlatform),
        Ok(vec![ProcessStatus::Exited(3), ProcessStatus::Exited(5)])
    );
}

#[test]
fn real_posix_redirections_cover_input_truncate_and_append() {
    let temp = TempDir::new("redirection-files");
    let input = temp.path.join("input.bin");
    let output = temp.path.join("output.bin");
    fs::write(&input, vec![b'x'; 64 * 1024]).expect("input should be seeded");
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let name = fixture.file_name().expect("fixture has a name");

    let plan = build_real(
        &format!(
            "^{0} relay 7 <{1} >{2}",
            name.to_string_lossy(),
            input.display(),
            output.display(),
        ),
        &fixture,
    );
    assert_eq!(
        execute_foreground_pipeline(&plan, &PosixPlatform),
        Ok(vec![ProcessStatus::Exited(7)])
    );
    assert_eq!(
        fs::read(&output).expect("output should exist").len(),
        64 * 1024
    );

    let plan = build_real(
        &format!(
            "^{0} source 3 8 >>{1}",
            name.to_string_lossy(),
            output.display(),
        ),
        &fixture,
    );
    assert_eq!(
        execute_foreground_pipeline(&plan, &PosixPlatform),
        Ok(vec![ProcessStatus::Exited(8)])
    );
    assert_eq!(
        fs::read(&output)
            .expect("output should remain readable")
            .len(),
        64 * 1024 + 3
    );
}

#[test]
fn real_posix_redirection_aliases_arbitrary_descriptors_then_closes_them() {
    let temp = TempDir::new("redirection-alias");
    let output = temp.path.join("combined.bin");
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let name = fixture.file_name().expect("fixture has a name");
    let plan = build_real(
        &format!(
            "^{0} both-closed 3 3 3>{1} 1>&3 2>&1 3>&-",
            name.to_string_lossy(),
            output.display(),
        ),
        &fixture,
    );

    assert_eq!(
        execute_foreground_pipeline(&plan, &PosixPlatform),
        Ok(vec![ProcessStatus::Exited(3)])
    );
    assert_eq!(
        fs::read(output).expect("combined output should exist"),
        b"xxxx"
    );
}

#[test]
fn real_posix_local_output_overrides_pipeline_plumbing_without_delaying_eof() {
    let temp = TempDir::new("redirection-pipeline-override");
    let output = temp.path.join("producer.bin");
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let name = fixture.file_name().expect("fixture has a name");
    let plan = build_real(
        &format!(
            "^{0} source 8192 7 >{1} | ^{0} sink 0 9",
            name.to_string_lossy(),
            output.display(),
        ),
        &fixture,
    );

    assert_eq!(
        execute_foreground_pipeline(&plan, &PosixPlatform),
        Ok(vec![ProcessStatus::Exited(7), ProcessStatus::Exited(9)])
    );
    assert_eq!(
        fs::read(output)
            .expect("producer output should exist")
            .len(),
        8192
    );
}

fn build_real(text: &str, fixture: &Path) -> ExecutionPlan {
    let file = source(text);
    let syntax = pipeline(&file);
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    plan_pipeline(
        &syntax,
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture.to_owned()),
    )
    .expect("real execution plan should build")
}

struct ExactProbe(PathBuf);

impl ExecutableProbe for ExactProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        path == self.0.as_os_str()
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "flashshell-runtime-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("temporary directory should be created");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).expect("temporary directory should be removed");
    }
}

#[test]
fn every_external_pipeline_member_joins_one_process_group() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    let name = fixture.file_name().expect("fixture has a name");
    let text = format!(
        "^{0} source 0 0 | ^{0} relay 0 | ^{0} sink 0 0",
        name.to_string_lossy(),
    );
    let file = source(&text);
    let syntax = pipeline(&file);
    let plan = plan_pipeline(
        &syntax,
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture),
    )
    .expect("the grouped pipeline plan should build");
    let platform = RecordingPlatform::new(FakePlatform::full());
    let spawns = platform.spawn_log();

    execute_foreground_pipeline(&plan, &platform).expect("the fake pipeline should run");

    let records = spawns.records();
    assert_eq!(records.len(), 3);
    let leader = records[0]
        .process_group()
        .expect("the first member leads the group");
    assert_eq!(records[0].requested(), ProcessGroup::New);
    for record in &records[1..] {
        // The later members join rather than lead: a pipeline that created one
        // group per stage could not be stopped or signalled as a single job.
        assert_eq!(record.requested(), ProcessGroup::Join(leader));
        assert_eq!(record.process_group(), Some(leader));
    }
}

#[test]
fn a_platform_without_process_groups_still_runs_an_external_pipeline() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    let name = fixture.file_name().expect("fixture has a name");
    let text = format!("^{0} source 0 0 | ^{0} sink 0 0", name.to_string_lossy());
    let file = source(&text);
    let syntax = pipeline(&file);
    let plan = plan_pipeline(
        &syntax,
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture),
    )
    .expect("the ungrouped pipeline plan should build");
    let ungrouped = Capabilities::full_without(Capability::ProcessGroups);
    let platform = RecordingPlatform::new(FakePlatform::new(ungrouped));
    let spawns = platform.spawn_log();

    execute_foreground_pipeline(&plan, &platform).expect("the pipeline still runs ungrouped");

    // Job control is a capability, not a requirement: a platform without
    // process groups keeps the pre-job-control behavior instead of failing.
    for record in spawns.records() {
        assert_eq!(record.requested(), ProcessGroup::Inherit);
        assert_eq!(record.process_group(), None);
    }
}

#[test]
fn a_foreground_pipeline_owns_the_terminal_and_gives_it_back() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    let name = fixture.file_name().expect("fixture has a name");
    let text = format!("^{0} source 0 0 | ^{0} sink 0 0", name.to_string_lossy());
    let file = source(&text);
    let syntax = pipeline(&file);
    let plan = plan_pipeline(
        &syntax,
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture),
    )
    .expect("the foreground pipeline plan should build");
    let interactive = FakePlatform::with_terminal(
        Capabilities::full(),
        true,
        flashshell_platform::TerminalSize::new(80, 24),
    );
    let platform = RecordingPlatform::new(interactive);
    let log = platform.log();
    let spawns = platform.spawn_log();

    execute_foreground_pipeline(&plan, &platform).expect("the pipeline should run");

    let group = spawns.records()[0]
        .process_group()
        .expect("the pipeline leads a group");
    // The terminal goes to the job's own group, not to a single stage, and it
    // is back with the shell before the next prompt could be drawn.
    assert_eq!(log.foreground_handovers(), vec![group]);
    assert_eq!(log.foreground_releases(), 1);
}

#[test]
fn a_pipeline_takes_no_terminal_when_the_session_has_none() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    let name = fixture.file_name().expect("fixture has a name");
    let text = format!("^{0} source 0 0 | ^{0} sink 0 0", name.to_string_lossy());
    let file = source(&text);
    let syntax = pipeline(&file);
    let plan = plan_pipeline(
        &syntax,
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture),
    )
    .expect("the redirected pipeline plan should build");
    // A redirected session has a real process group but no terminal to give:
    // taking one would be a handover to a descriptor that is not a terminal.
    let platform = RecordingPlatform::new(FakePlatform::full());
    let log = platform.log();

    execute_foreground_pipeline(&plan, &platform).expect("the pipeline should run");

    assert!(log.foreground_handovers().is_empty());
    assert_eq!(log.foreground_releases(), 0);
}

#[test]
fn a_platform_without_terminal_ownership_still_runs_a_foreground_pipeline() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    let name = fixture.file_name().expect("fixture has a name");
    let text = format!("^{0} source 0 0 | ^{0} sink 0 0", name.to_string_lossy());
    let file = source(&text);
    let syntax = pipeline(&file);
    let plan = plan_pipeline(
        &syntax,
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture),
    )
    .expect("the unowned pipeline plan should build");
    let no_ownership = FakePlatform::with_terminal(
        Capabilities::full_without(Capability::ForegroundTerminal),
        true,
        flashshell_platform::TerminalSize::new(80, 24),
    );
    let platform = RecordingPlatform::new(no_ownership);
    let log = platform.log();

    execute_foreground_pipeline(&plan, &platform).expect("the pipeline still runs");

    // Terminal ownership is a capability, not a requirement: without it the
    // job runs in the shell's terminal exactly as it did before job control.
    assert!(log.foreground_handovers().is_empty());
}

/// Plan one external stage per entry of `stages`, each running the fixture with
/// the given arguments.
///
/// The fixture is never actually executed by these tests, which drive a fake
/// platform; it supplies a path a real executable probe accepts, so the plan is
/// the same shape a real pipeline would produce.
fn fixture_pipeline_plan(stages: &[&str]) -> ExecutionPlan {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    let name = fixture.file_name().expect("fixture has a name");
    let text = stages
        .iter()
        .map(|arguments| format!("^{} {arguments}", name.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" | ");
    let file = source(&text);
    let syntax = pipeline(&file);
    plan_pipeline(
        &syntax,
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture),
    )
    .expect("the fixture pipeline plan should build")
}

#[test]
fn a_stopped_foreground_job_is_resumed_in_place_and_still_completes() {
    let platform = RecordingPlatform::new(FakePlatform::with_stopping_children(
        Capabilities::full(),
        1,
    ));
    let signals = platform.signal_log();
    let spawns = platform.spawn_log();
    let plan = fixture_pipeline_plan(&["source 0 0", "sink 0 0"]);

    let statuses = execute_foreground_pipeline(&plan, &platform).expect("the pipeline completes");

    assert_eq!(
        statuses,
        vec![ProcessStatus::Exited(0), ProcessStatus::Exited(0)]
    );
    let leader = spawns.records()[0]
        .process_group()
        .expect("the first member leads the group");
    let records = signals.records();
    // One resume per observed stop, each addressed to the group the pipeline
    // actually joined rather than to any group at all. The fake gives every
    // child its own stop counter, so a two-stage pipeline reports two
    // independent stops; that a single real resume signal reaches every member
    // at once is a host property, proven by the adapter rather than here.
    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record.signal(), JobSignal::Continue);
        assert_eq!(record.group(), leader);
    }
}

#[test]
fn a_foreground_wait_allows_sixteen_automatic_resumptions_per_member() {
    let platform = RecordingPlatform::new(FakePlatform::with_stopping_children(
        Capabilities::full(),
        16,
    ));
    let signals = platform.signal_log();
    let plan = fixture_pipeline_plan(&["source 0 0"]);

    assert_eq!(
        execute_foreground_pipeline(&plan, &platform),
        Ok(vec![ProcessStatus::Exited(0)])
    );
    assert_eq!(signals.records().len(), 16);
}

#[test]
fn a_foreground_wait_rejects_a_seventeenth_stop_without_another_resume() {
    let platform = RecordingPlatform::new(FakePlatform::with_stopping_children(
        Capabilities::full(),
        17,
    ));
    let signals = platform.signal_log();
    let plan = fixture_pipeline_plan(&["source 0 0"]);

    let error = execute_foreground_pipeline(&plan, &platform)
        .expect_err("the seventeenth stop must end the blocking operation");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::RepeatedStop {
            signal: FAKE_STOP_SIGNAL
        }
    ));
    assert_eq!(error.span(), plan.stages()[0].span());
    assert_eq!(signals.records().len(), 16);
}

#[test]
fn a_stop_that_cannot_be_resumed_is_an_error_rather_than_a_spin() {
    let platform =
        FakePlatform::with_stopping_children(Capabilities::full_without(Capability::Signals), 1);
    let plan = fixture_pipeline_plan(&["source 0 0"]);

    match execute_foreground_pipeline(&plan, &platform) {
        // The group exists here, so only the missing capability can produce
        // this: a platform error rather than a host delivery failure.
        Err(error) => assert!(
            matches!(
                error.kind(),
                RuntimeErrorKind::JobSignal(SignalError::Platform(_)),
            ),
            "expected a signal capability failure, got {:?}",
            error.kind(),
        ),
        Ok(statuses) => panic!("a job that cannot be resumed must not report {statuses:?}"),
    }
}

#[test]
fn a_stop_without_an_established_group_is_an_error_rather_than_a_spin() {
    // Without process groups every member stays in the shell's own group, so a
    // terminal stop would have stopped the shell too. Observing one anyway is a
    // state the executor must report rather than retry forever. The kind is
    // asserted exactly, so this cannot pass for the missing-capability reason
    // the neighbouring test pins.
    let platform = FakePlatform::with_stopping_children(
        Capabilities::full_without(Capability::ProcessGroups),
        1,
    );
    let plan = fixture_pipeline_plan(&["source 0 0"]);

    match execute_foreground_pipeline(&plan, &platform) {
        Err(error) => assert!(
            matches!(error.kind(), RuntimeErrorKind::UngroupedStop),
            "expected an ungrouped stop, got {:?}",
            error.kind(),
        ),
        Ok(statuses) => panic!("an ungrouped stop must not report {statuses:?}"),
    }
}
