#![forbid(unsafe_code)]

//! Acceptance coverage for the persistent interactive session driver.
//!
//! A `Session` retains scope, environment, logical cwd, and last status across
//! independently submitted edit buffers, dispatches single-stage internal
//! built-ins against that state, executes external foreground pipelines, and
//! surfaces recoverable failures without discarding the accumulated state. It
//! never depends on a real process, terminal, or clock. Most tests drive the
//! host-free `FakePlatform`; file-boundary acceptance uses the POSIX adapter
//! against isolated temporary directories.

use std::any::Any;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use flashshell_platform::{
    Capabilities, ChildProcess, DescriptorEndpoint, FakePlatform, FileActionError, FileOpenRequest,
    JobSignal, PipeEndpoints, PipeError, Platform, ProcessGroup, ProcessGroupId, ProcessStatus,
    ProcessTransition, RecordingPlatform, SignalError, SpawnError, SpawnRequest, TerminalSize,
    TerminateError, WaitError,
};
use flashshell_platform_posix::PosixPlatform;
use flashshell_runtime::eval::FakeClock;
use flashshell_runtime::job::{JobMemberState, JobState, ProcessId};
use flashshell_runtime::plan::SessionOptions;
use flashshell_runtime::resolve::ExecutableProbe;
use flashshell_runtime::session::{
    BackgroundFailureReason, JobNoticeKind, LiveJobState, Session, SubmitOutcome,
};
use flashshell_runtime::{Duration, Environment, Status};

#[derive(Default)]
struct Probe {
    paths: Vec<PathBuf>,
}

impl Probe {
    fn new(paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            paths: paths.into_iter().map(Into::into).collect(),
        }
    }
}

impl ExecutableProbe for Probe {
    fn is_executable(&self, path: &OsStr) -> bool {
        self.paths
            .iter()
            .any(|candidate| candidate.as_os_str() == path)
    }
}

fn environment() -> Environment {
    Environment::from_snapshot([
        ("PATH", OsString::from("/bin")),
        ("HOME", OsString::from("/home/me")),
    ])
}

fn session() -> Session {
    Session::new("/work", environment(), SessionOptions::default())
}

fn terminal_platform() -> FakePlatform {
    FakePlatform::with_terminal(Capabilities::full(), true, TerminalSize::new(80, 24))
}

#[derive(Debug)]
struct BackgroundEndpoint;

impl DescriptorEndpoint for BackgroundEndpoint {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

type ChildStep = Result<ProcessTransition, WaitError>;

struct BackgroundChild {
    id: u64,
    group: ProcessGroupId,
    steps: mpsc::Receiver<ChildStep>,
    wait_entries: mpsc::Sender<usize>,
    waits: usize,
    terminate_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl fmt::Debug for BackgroundChild {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackgroundChild")
            .field("id", &self.id)
            .field("group", &self.group)
            .finish_non_exhaustive()
    }
}

impl BackgroundChild {
    fn next(&mut self) -> ChildStep {
        self.waits += 1;
        self.wait_entries
            .send(self.waits)
            .expect("the test retains the wait-entry receiver");
        self.steps.recv().expect("the test releases every wait")
    }
}

impl ChildProcess for BackgroundChild {
    fn id(&self) -> u64 {
        self.id
    }

    fn process_group(&self) -> Option<ProcessGroupId> {
        Some(self.group)
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        loop {
            match self.next()? {
                ProcessTransition::Stopped { .. } => {}
                ProcessTransition::Completed(status) => return Ok(status),
            }
        }
    }

    fn wait_for_transition(&mut self) -> Result<ProcessTransition, WaitError> {
        self.next()
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        self.terminate_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct BackgroundChildControl {
    steps: mpsc::Sender<ChildStep>,
    wait_entries: mpsc::Receiver<usize>,
    terminate_calls: Arc<std::sync::atomic::AtomicUsize>,
}

struct ControlledBackgroundPlatform {
    children: Mutex<VecDeque<Box<dyn ChildProcess>>>,
    signals: Mutex<Vec<(ProcessGroupId, JobSignal)>>,
    /// When set, every group signal is refused with this message.
    signal_refusal: Mutex<Option<String>>,
}

impl ControlledBackgroundPlatform {
    /// Refuse every subsequent group signal, as a host with no such group would.
    fn refuse_signals(&self, message: &str) {
        *self.signal_refusal.lock().expect("refusal lock") = Some(message.to_owned());
    }

    fn new(ids: &[u64]) -> (Self, Vec<BackgroundChildControl>) {
        let group = ProcessGroupId::new(ids[0]).expect("test group is nonzero");
        let mut children: VecDeque<Box<dyn ChildProcess>> = VecDeque::new();
        let mut controls = Vec::new();
        for &id in ids {
            let (step_sender, step_receiver) = mpsc::channel();
            let (wait_sender, wait_receiver) = mpsc::channel();
            let terminate_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            children.push_back(Box::new(BackgroundChild {
                id,
                group,
                steps: step_receiver,
                wait_entries: wait_sender,
                waits: 0,
                terminate_calls: Arc::clone(&terminate_calls),
            }));
            controls.push(BackgroundChildControl {
                steps: step_sender,
                wait_entries: wait_receiver,
                terminate_calls,
            });
        }
        (
            Self {
                children: Mutex::new(children),
                signals: Mutex::new(Vec::new()),
                signal_refusal: Mutex::new(None),
            },
            controls,
        )
    }
}

impl Platform for ControlledBackgroundPlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities::full()
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        Ok(PipeEndpoints::new(
            Box::new(BackgroundEndpoint),
            Box::new(BackgroundEndpoint),
        ))
    }

    fn open_file(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Ok(Box::new(BackgroundEndpoint))
    }

    fn inherit_descriptor(
        &self,
        _descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Ok(Box::new(BackgroundEndpoint))
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        let child = self
            .children
            .lock()
            .expect("child queue lock")
            .pop_front()
            .expect("one child is scripted per stage");
        match request.process_group() {
            ProcessGroup::New | ProcessGroup::Join(_) => Ok(child),
            ProcessGroup::Inherit => panic!("background startup must request a process group"),
        }
    }

    fn signal_process_group(
        &self,
        group: ProcessGroupId,
        signal: JobSignal,
    ) -> Result<(), SignalError> {
        if let Some(message) = self
            .signal_refusal
            .lock()
            .expect("refusal lock")
            .as_ref()
            .cloned()
        {
            return Err(SignalError::Operation {
                kind: io::ErrorKind::NotFound,
                message,
            });
        }
        self.signals
            .lock()
            .expect("signal lock")
            .push((group, signal));
        Ok(())
    }
}

fn wait_for_notice(session: &mut Session) -> flashshell_runtime::session::JobNotice {
    (0..10_000)
        .find_map(|_| {
            let notice = session.next_job_notice();
            if notice.is_none() {
                std::thread::yield_now();
            }
            notice
        })
        .expect("the scripted child should produce a notice")
}

/// Submit one buffer with a fresh throwaway output sink, asserting success.
fn submit(session: &mut Session, text: &str, probe: &dyn ExecutableProbe) -> SubmitOutcome {
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            text,
            probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("submission should succeed")
}

#[test]
fn pure_bindings_persist_across_submissions() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(&mut session, "let base = 41", &probe),
        SubmitOutcome::Continued
    );
    // A later submission observes the earlier binding through the same scope.
    assert_eq!(
        submit(&mut session, "export DERIVED = $base", &probe),
        SubmitOutcome::Continued
    );

    assert_eq!(session.environment().get("DERIVED"), Some(OsStr::new("41")));
}

#[test]
fn cd_updates_the_logical_cwd_across_submissions() {
    let mut session = session();
    let probe = Probe::default();

    submit(&mut session, "cd /srv", &probe);
    assert_eq!(session.cwd(), Path::new("/srv"));

    // A relative target resolves against the retained logical cwd.
    submit(&mut session, "cd data", &probe);
    assert_eq!(session.cwd(), Path::new("/srv/data"));
}

#[test]
fn exit_with_an_explicit_code_requests_termination() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(&mut session, "exit 7", &probe),
        SubmitOutcome::Exit(7)
    );
}

#[test]
fn exit_without_an_argument_uses_the_last_status() {
    let mut session = session();
    let probe = Probe::new(["/bin/tool"]);

    // A successful external leaves status zero, which a bare exit then reports.
    submit(&mut session, "^tool", &probe);
    assert_eq!(submit(&mut session, "exit", &probe), SubmitOutcome::Exit(0));
}

#[test]
fn external_commands_execute_and_record_their_status() {
    let mut session = session();
    let probe = Probe::new(["/bin/tool"]);

    assert_eq!(
        submit(&mut session, "^tool", &probe),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.current_status().and_then(|status| status.code()),
        Some(0)
    );
}

#[test]
fn background_work_without_a_job_control_opt_in_is_rejected_at_the_marker() {
    let mut session = session();
    let probe = Probe::new(["/bin/tool"]);
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<script>",
            "^tool &",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("background work needs a coordinator");

    // Not "interactive": a script may now opt in as well, so the missing thing
    // is job control itself rather than a terminal.
    assert!(error.render().contains("without job control"));
    assert!(error.render().contains("^tool &"));
}

#[test]
fn a_background_conditional_chain_is_rejected_at_its_operator() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "^tool && ^other &",
            &probe,
            &terminal_platform(),
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("background conditional chains need an owned supervisor");

    assert!(error.render().contains("conditional"));
    assert!(error.render().contains("&&"));
}

#[test]
fn a_background_internal_stage_is_rejected_at_the_command() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "pwd &",
            &Probe::default(),
            &terminal_platform(),
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("background internal commands remain unsupported");

    assert!(error.render().contains("internal"));
    assert!(error.render().contains("pwd &"));
}

#[test]
fn interactive_background_pipelines_publish_complete_jobs_and_zero_launch_status() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let terminal = platform.log();
    let mut sink = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "^tool | ^other &",
                &probe,
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect("the background external pipeline should launch"),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.current_status(),
        Some(&Status::exit(0, Duration::ZERO).expect("zero duration is valid"))
    );
    assert!(terminal.foreground_handovers().is_empty());

    let started = session
        .next_job_notice()
        .expect("background launch queues a notice");
    assert_eq!(started.job().get(), 1);
    assert_eq!(started.id().get(), 1);
    assert!(matches!(started.kind(), JobNoticeKind::Started { .. }));
    let job = session
        .background_job(started.job())
        .expect("the complete job is addressable before its notice");
    assert_eq!(job.state(), JobState::Background);
    assert_eq!(job.members().count(), 2);
    assert_eq!(
        session.next_job_notice().as_ref().map(|notice| notice.id()),
        Some(started.id()),
        "retrieval must not acknowledge a notice"
    );
    session
        .acknowledge_job_notice(started.id())
        .expect("the rendered launch notice should acknowledge");

    let completed = (0..10_000)
        .find_map(|_| {
            let notice = session.next_job_notice();
            if notice.is_none() {
                std::thread::yield_now();
            }
            notice
        })
        .expect("the fake background children should complete");
    assert_eq!(completed.job(), started.job());
    assert_eq!(completed.id().get(), 2);
    assert_eq!(completed.kind(), &JobNoticeKind::Completed);
    assert_eq!(
        session
            .background_completion(completed.job())
            .expect("completion remains addressable until acknowledgement")
            .stages()
            .len(),
        2
    );
    session
        .acknowledge_job_notice(completed.id())
        .expect("completion acknowledgement should reap the record");
    assert!(session.background_job(completed.job()).is_none());
}

#[test]
fn background_completion_aggregates_source_order_pipefail_and_latest_time() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = Session::new(
        "/work",
        environment(),
        SessionOptions::default().with_pipefail(true),
    );
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    let (platform, controls) = ControlledBackgroundPlatform::new(&[101, 102]);
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "^tool | ^other &",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the controlled pipeline should launch");
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    let started = session.next_job_notice().expect("launch notice");
    session
        .acknowledge_job_notice(started.id())
        .expect("acknowledge launch");

    clock.advance(20);
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(7))))
        .expect("complete the second source stage first");
    for _ in 0..10_000 {
        assert!(
            session.next_job_notice().is_none(),
            "a partial completion must not publish a notice"
        );
        let second_completed = session
            .background_job(started.job())
            .and_then(|job| {
                job.members()
                    .find(|(process, _)| *process == ProcessId::new(102).unwrap())
                    .map(|(_, state)| matches!(state, JobMemberState::Completed(_)))
            })
            .unwrap_or(false);
        if second_completed {
            break;
        }
        std::thread::yield_now();
    }
    clock.advance(10);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the first source stage last");

    let completed = wait_for_notice(&mut session);
    assert_eq!(completed.kind(), &JobNoticeKind::Completed);
    let status = session
        .background_completion(started.job())
        .expect("aggregate completion remains addressable");
    assert_eq!(status.code(), Some(7));
    assert_eq!(status.duration(), Duration::from_nanos(30));
    assert_eq!(
        status
            .stages()
            .iter()
            .map(|stage| (stage.code(), stage.duration()))
            .collect::<Vec<_>>(),
        vec![
            (Some(0), Duration::from_nanos(30)),
            (Some(7), Duration::from_nanos(20)),
        ]
    );
    assert_eq!(
        session.current_status(),
        Some(&Status::exit(0, Duration::ZERO).expect("zero status is valid")),
        "background completion must not overwrite the launch status"
    );
    session
        .acknowledge_job_notice(completed.id())
        .expect("completion acknowledgement should reap the job");
}

#[test]
fn a_stopped_background_job_is_not_resumed_and_remains_after_notice_acknowledgement() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[111]);
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "^tool &",
            &Probe::new(["/bin/tool"]),
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the controlled job should launch");
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    let started = session.next_job_notice().expect("launch notice");
    session
        .acknowledge_job_notice(started.id())
        .expect("acknowledge launch");

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("release the stop");
    assert_eq!(controls[0].wait_entries.recv().expect("second wait"), 2);
    let stopped = wait_for_notice(&mut session);
    assert_eq!(stopped.kind(), &JobNoticeKind::Stopped);
    assert_eq!(
        session.next_job_notice().as_ref().map(|notice| notice.id()),
        Some(stopped.id())
    );
    let job = session
        .background_job(started.job())
        .expect("stopped job remains addressable");
    assert!(matches!(job.state(), JobState::Stopped { .. }));
    assert!(matches!(
        job.members().next().map(|(_, state)| state),
        Some(JobMemberState::Stopped { signal: 19 })
    ));
    assert!(
        platform.signals.lock().expect("signal lock").is_empty(),
        "a background stop must not be automatically resumed"
    );
    session
        .acknowledge_job_notice(stopped.id())
        .expect("acknowledge stopped notice");
    assert!(session.background_job(started.job()).is_some());

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("finish the stopped test child");
    let completed = wait_for_notice(&mut session);
    assert_eq!(completed.kind(), &JobNoticeKind::Completed);
    assert_eq!(
        session.next_job_notice().as_ref().map(|notice| notice.id()),
        Some(completed.id()),
        "completion retrieval must remain repeatable until acknowledgement"
    );
    session
        .acknowledge_job_notice(completed.id())
        .expect("reap the completed record");
}

#[test]
fn background_job_and_notice_identities_increase_across_launches() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool"]);
    let mut sink = Vec::new();

    for _ in 0..2 {
        session
            .submit(
                "<interactive>",
                "^tool &",
                &probe,
                &terminal_platform(),
                clock.as_ref(),
                &mut sink,
            )
            .expect("each fake job should launch");
    }

    let first = session.next_job_notice().expect("first launch notice");
    assert_eq!((first.job().get(), first.id().get()), (1, 1));
    session
        .acknowledge_job_notice(first.id())
        .expect("acknowledge first launch");
    let second = session.next_job_notice().expect("second launch notice");
    assert_eq!((second.job().get(), second.id().get()), (2, 2));
    session
        .acknowledge_job_notice(second.id())
        .expect("acknowledge second launch");

    for _ in 0..2 {
        let completion = wait_for_notice(&mut session);
        session
            .acknowledge_job_notice(completion.id())
            .expect("reap each fake job");
    }
}

/// Launch one controlled background job and acknowledge its launch notice.
///
/// Every lifetime test starts from the same place: a live job whose transitions
/// the test still owns, and an empty notice queue.
fn launch_controlled_job(
    session: &mut Session,
    clock: &Arc<FakeClock>,
    platform: &ControlledBackgroundPlatform,
    source: &str,
    probe: &Probe,
) {
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            source,
            probe,
            platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the controlled background job should launch");
    let started = session.next_job_notice().expect("launch notice");
    session
        .acknowledge_job_notice(started.id())
        .expect("acknowledge launch");
}

#[test]
fn a_session_without_job_control_reports_no_live_jobs() {
    let session = session();

    assert!(
        session.live_background_jobs().is_empty(),
        "a session that cannot start background work has none to report"
    );
}

#[test]
fn a_join_waits_every_live_member_and_reports_a_nonzero_aggregate() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[131, 132]);
    launch_controlled_job(
        &mut session,
        &clock,
        &platform,
        "^tool | ^other &",
        &Probe::new(["/bin/tool", "/bin/other"]),
    );
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    assert_eq!(session.live_background_jobs().len(), 1);

    // Released but deliberately not drained: the observations queue up, so the
    // record is still live when the join starts and the join is what consumes
    // them.
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the first stage");
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(3))))
        .expect("complete the last stage");

    let failures = session.join_background_jobs(&platform);

    assert_eq!(failures.len(), 1, "a nonzero aggregate is one failure");
    assert_eq!(failures[0].job().get(), 1);
    assert!(
        matches!(
            failures[0].reason(),
            BackgroundFailureReason::Exited(status) if status.code() == Some(3)
        ),
        "the aggregate must carry the failing member's code: {:?}",
        failures[0].reason()
    );
    assert!(session.live_background_jobs().is_empty());
}

#[test]
fn a_join_resumes_a_stopped_job_before_waiting_on_it() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[141]);
    launch_controlled_job(
        &mut session,
        &clock,
        &platform,
        "^tool &",
        &Probe::new(["/bin/tool"]),
    );
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("release the stop");
    assert_eq!(controls[0].wait_entries.recv().expect("second wait"), 2);
    let stopped = wait_for_notice(&mut session);
    session
        .acknowledge_job_notice(stopped.id())
        .expect("acknowledge the stop");
    assert_eq!(
        session.live_background_jobs()[0].state(),
        LiveJobState::Stopped
    );

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("the resumed child finishes");
    let failures = session.join_background_jobs(&platform);

    assert!(
        failures.is_empty(),
        "a resumed job that exits zero is not a failure: {failures:?}"
    );
    assert_eq!(
        *platform.signals.lock().expect("signal lock"),
        vec![(
            ProcessGroupId::new(141).expect("test group is nonzero"),
            JobSignal::Continue
        )],
        "a stopped job must be resumed exactly once, and never hung up by a join"
    );
}

#[test]
fn a_hang_up_resumes_then_hangs_up_every_live_group() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[151]);
    launch_controlled_job(
        &mut session,
        &clock,
        &platform,
        "^tool &",
        &Probe::new(["/bin/tool"]),
    );
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("release the stop");
    assert_eq!(controls[0].wait_entries.recv().expect("second wait"), 2);
    let stopped = wait_for_notice(&mut session);
    session
        .acknowledge_job_notice(stopped.id())
        .expect("acknowledge the stop");

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Signaled(1))))
        .expect("the hung-up child dies");
    let failures = session.hang_up_background_jobs(&platform);

    let group = ProcessGroupId::new(151).expect("test group is nonzero");
    assert_eq!(
        *platform.signals.lock().expect("signal lock"),
        vec![(group, JobSignal::Continue), (group, JobSignal::Hangup)],
        "a stopped group must be resumed before it can act on a hang-up"
    );
    assert_eq!(failures.len(), 1, "a signalled death is a reported failure");
    assert!(session.live_background_jobs().is_empty());
}

#[test]
fn a_signal_failure_quarantines_the_record_without_blocking_exit() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[161]);
    launch_controlled_job(
        &mut session,
        &clock,
        &platform,
        "^tool &",
        &Probe::new(["/bin/tool"]),
    );
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    platform.refuse_signals("the host refused the group");

    let failures = session.hang_up_background_jobs(&platform);

    assert_eq!(failures.len(), 1);
    assert!(
        matches!(
            failures[0].reason(),
            BackgroundFailureReason::Signal(message) if message.contains("refused")
        ),
        "an unsignallable job is quarantined, not claimed reaped: {:?}",
        failures[0].reason()
    );
    assert!(
        session.live_background_jobs().is_empty(),
        "a quarantined record must not keep the wait alive"
    );

    // Release the child so its observer thread ends with the test rather than
    // outliving it blocked on a scripted transition.
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("release the quarantined child");
}

#[test]
fn observer_cleanup_recovery_completes_but_unrecoverable_failure_is_quarantined() {
    let clock = Arc::new(FakeClock::new());
    let mut recovered = session();
    recovered.enable_interactive_job_control(clock.clone());
    let (recovered_platform, recovered_controls) = ControlledBackgroundPlatform::new(&[121]);
    let mut sink = Vec::new();
    recovered
        .submit(
            "<interactive>",
            "^tool &",
            &Probe::new(["/bin/tool"]),
            &recovered_platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the recovered job should launch");
    assert_eq!(
        recovered_controls[0]
            .wait_entries
            .recv()
            .expect("initial wait"),
        1
    );
    let launch = recovered.next_job_notice().expect("launch notice");
    recovered
        .acknowledge_job_notice(launch.id())
        .expect("acknowledge launch");
    recovered_controls[0]
        .steps
        .send(Err(WaitError::new(
            io::ErrorKind::Interrupted,
            "initial failure",
        )))
        .expect("fail initial observation");
    assert_eq!(
        recovered_controls[0]
            .wait_entries
            .recv()
            .expect("cleanup wait"),
        2
    );
    recovered_controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(4))))
        .expect("recover terminal status");
    let completion = wait_for_notice(&mut recovered);
    assert_eq!(completion.kind(), &JobNoticeKind::Completed);
    assert_eq!(
        recovered_controls[0].terminate_calls.load(Ordering::SeqCst),
        1
    );
    recovered
        .acknowledge_job_notice(completion.id())
        .expect("reap recovered job");

    let mut failed = session();
    failed.enable_interactive_job_control(clock.clone());
    let (failed_platform, failed_controls) = ControlledBackgroundPlatform::new(&[131]);
    failed
        .submit(
            "<interactive>",
            "^tool &",
            &Probe::new(["/bin/tool"]),
            &failed_platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the failing job should launch");
    assert_eq!(
        failed_controls[0]
            .wait_entries
            .recv()
            .expect("initial wait"),
        1
    );
    let launch = failed.next_job_notice().expect("launch notice");
    failed
        .acknowledge_job_notice(launch.id())
        .expect("acknowledge launch");
    failed_controls[0]
        .steps
        .send(Err(WaitError::new(
            io::ErrorKind::Interrupted,
            "initial failure",
        )))
        .expect("fail initial observation");
    assert_eq!(
        failed_controls[0]
            .wait_entries
            .recv()
            .expect("cleanup wait"),
        2
    );
    failed_controls[0]
        .steps
        .send(Err(WaitError::new(io::ErrorKind::Other, "cleanup failure")))
        .expect("fail cleanup");
    let failure = wait_for_notice(&mut failed);
    assert!(matches!(
        failure.kind(),
        JobNoticeKind::ObservationFailed { .. }
    ));
    assert!(matches!(
        failed
            .background_job(launch.job())
            .expect("failed job remains addressable")
            .state(),
        JobState::Background
    ));
    failed
        .acknowledge_job_notice(failure.id())
        .expect("acknowledge failure diagnostic");
    assert!(failed.background_job(launch.job()).is_some());
}

#[test]
fn pwd_renders_the_logical_cwd_to_the_output_sink() {
    let mut session = session();
    let probe = Probe::default();
    submit(&mut session, "cd /srv", &probe);

    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "pwd",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("pwd should succeed");

    let rendered = String::from_utf8(sink).expect("pwd output is UTF-8");
    assert!(
        rendered.contains("/srv"),
        "pwd should print the logical cwd, got {rendered:?}"
    );
}

#[test]
fn an_internal_structured_pipeline_preserves_values_until_final_presentation() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd missing | select name | get name | first 1",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the all-internal structured pipeline should execute");

    assert_eq!(String::from_utf8(sink).unwrap(), "pwd\n");
    let status = session.current_status().expect("pipeline records a status");
    assert_eq!(status.code(), Some(0));
    assert_eq!(status.stages().len(), 4);
}

#[test]
fn a_terminal_structured_command_can_materialize_under_its_bound() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd missing | length",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("length should consume the live value stream");

    assert_eq!(String::from_utf8(sink).unwrap(), "2\n");
}

#[test]
fn closure_free_reshapers_compose_without_serializing_an_edge() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd missing | get kind | lines | sort | last 1 | collect",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the closure-free structured commands should compose");

    assert_eq!(
        String::from_utf8(sink).unwrap(),
        "[\"internalmissing\"]\n",
        "lines treats adjacent String values as chunks of one logical text stream"
    );
}

#[test]
fn ls_is_a_live_structured_source() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "ls | length",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the fake platform's empty directory should be a live stream");

    assert_eq!(String::from_utf8(sink).unwrap(), "0\n");
}

#[test]
fn a_typed_argument_to_a_word_only_builtin_is_rejected() {
    let mut session = session();
    let probe = Probe::default();
    let original = session.cwd().to_owned();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "cd {|| 1}",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("cd must not reinterpret a callable as a native path");

    assert!(error.render().contains("expected a word argument"));
    assert_eq!(session.cwd(), original);
    assert!(sink.is_empty());
}

#[test]
fn a_lazy_structured_failure_does_not_commit_pipeline_status() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "which pwd | get absent",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("the missing field is discovered when presentation pulls");

    assert!(error.render().contains("record has no field `absent`"));
    assert!(session.current_status().is_none());
    assert!(sink.is_empty());
}

#[test]
fn each_and_where_execute_captured_closures_lazily() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "let wanted = 'internal'\n\
             which pwd missing | where {|row| $row.kind == $wanted} | \
             each {|row| $row.name} | first 1",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("where and each should execute their captured closures");

    assert_eq!(String::from_utf8(sink).unwrap(), "pwd\n");
}

#[test]
fn update_supports_closure_and_static_replacements() {
    let probe = Probe::default();

    let mut closure_session = session();
    let mut closure_sink = Vec::new();
    closure_session
        .submit(
            "<interactive>",
            "which pwd | update kind {|kind| 'changed'} | get kind",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut closure_sink,
        )
        .expect("update should apply a closure to the current field");
    assert_eq!(String::from_utf8(closure_sink).unwrap(), "changed\n");

    let mut static_session = session();
    let mut static_sink = Vec::new();
    static_session
        .submit(
            "<interactive>",
            "which pwd | update kind known | get kind",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut static_sink,
        )
        .expect("update should accept a static text replacement");
    assert_eq!(String::from_utf8(static_sink).unwrap(), "known\n");
}

#[test]
fn a_successful_lazy_closure_pipeline_commits_its_environment() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "def mark(row) {\n\
                 export SEEN = 'yes'\n\
                 return $row\n\
             }\n\
             which pwd | each {|row| mark($row)} | first 1",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("a successful closure application should commit its environment");

    assert_eq!(session.environment().get("SEEN"), Some(OsStr::new("yes")));
}

#[test]
fn a_failing_lazy_closure_pipeline_rolls_back_its_environment() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "def mark(row) {\n\
                 export LEAK = 'no'\n\
                 return $row\n\
             }\n\
             which pwd | each {|row| mark($row)} | get absent",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("a downstream lazy failure should reject closure side effects");

    assert!(error.render().contains("record has no field `absent`"));
    assert_eq!(session.environment().get("LEAK"), None);
    assert!(sink.is_empty());
}

#[test]
fn explicit_codec_boundaries_round_trip_live_bytes() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd | get kind | encode utf8 | decode utf8 | encode utf8",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("explicit codec boundaries should keep bytes byte-correct");

    assert_eq!(sink, b"internal");
}

#[test]
fn live_file_and_format_boundaries_preserve_json_and_text() {
    let temp = TempDir::new("session-boundaries");
    fs::write(
        temp.path().join("input.json"),
        br#"{"name":"FlashOS","active":true}"#,
    )
    .expect("JSON fixture should be written");
    fs::write(temp.path().join("input.txt"), b"one\r\ntwo\n")
        .expect("text fixture should be written");
    fs::write(temp.path().join("input.bin"), [0, 0xff, 7])
        .expect("binary fixture should be written");

    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::default();
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "open input.json | from json | to json | save output.json\n\
             open input.txt | from text | first 1 | to text | save first.txt\n\
             open input.bin | decode bytes | encode bytes | save output.bin",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("file and format boundaries should execute end to end");

    assert!(sink.is_empty());
    assert_eq!(
        fs::read(temp.path().join("output.json")).unwrap(),
        br#"{"name":"FlashOS","active":true}"#
    );
    assert_eq!(fs::read(temp.path().join("first.txt")).unwrap(), b"one\n");
    assert_eq!(
        fs::read(temp.path().join("output.bin")).unwrap(),
        [0, 0xff, 7]
    );
}

#[test]
fn mixed_process_boundaries_stream_in_both_directions() {
    let temp = TempDir::new("session-mixed-boundaries");
    fs::write(temp.path().join("lines.txt"), b"one\ntwo\n")
        .expect("text fixture should be written");
    let binary: Vec<u8> = (0u8..=255).cycle().take(2 * 1024 * 1024).collect();
    fs::write(temp.path().join("input.bin"), &binary).expect("binary fixture should be written");

    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::new(["/bin/cat"]);
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "^/bin/cat < lines.txt | from text | first 1 | to text\n\
             open input.bin | decode bytes | encode bytes | ^/bin/cat > output.bin\n\
             ^/bin/cat < input.bin | decode bytes | encode bytes | \
             ^/bin/cat > roundtrip.bin",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("mixed boundaries should stream without capture");

    assert_eq!(sink, b"one\n");
    assert_eq!(fs::read(temp.path().join("output.bin")).unwrap(), binary);
    assert_eq!(fs::read(temp.path().join("roundtrip.bin")).unwrap(), binary);
}

#[test]
fn an_early_external_exit_stops_the_internal_byte_producer() {
    let temp = TempDir::new("session-mixed-early-exit");
    fs::write(temp.path().join("large.bin"), vec![b'x'; 2 * 1024 * 1024])
        .expect("large fixture should be written");

    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::new(["/usr/bin/head"]);
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "open large.bin | ^/usr/bin/head -c 1 > first.bin",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("a closed consumer pipe should stop the internal producer");

    assert!(sink.is_empty());
    assert_eq!(fs::read(temp.path().join("first.bin")).unwrap(), b"x");
    assert_eq!(session.current_status().and_then(Status::code), Some(0),);
}

#[test]
fn mixed_pipeline_statuses_aggregate_in_source_order() {
    let temp = TempDir::new("session-mixed-status");
    let mut session = Session::new(
        temp.path(),
        environment(),
        SessionOptions::default().with_pipefail(true),
    );
    let probe = Probe::new(["/usr/bin/false", "/bin/cat"]);
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "^/usr/bin/false | decode bytes | encode bytes | ^/bin/cat > output.bin",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("nonzero process completion should remain a normal status");

    let status = session
        .current_status()
        .expect("pipeline status should commit");
    assert_eq!(status.code(), Some(1));
    assert_eq!(status.stages().len(), 4);
    assert_eq!(
        status.stages().iter().map(Status::code).collect::<Vec<_>>(),
        vec![Some(1), Some(0), Some(0), Some(0)]
    );
}

#[test]
fn a_lazy_byte_boundary_failure_does_not_commit_status() {
    let temp = TempDir::new("session-byte-failure");
    fs::write(temp.path().join("bad.txt"), [0xff]).expect("byte fixture should be written");
    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::default();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "open bad.txt | decode utf8 | encode utf8",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("strict decoding should surface malformed input lazily");

    assert!(error.render().contains("malformed input at byte offset 0"));
    assert!(sink.is_empty());
    assert!(session.current_status().is_none());
}

#[test]
fn structured_output_requires_an_interactive_output_terminal() {
    let mut session = session();
    let probe = Probe::default();
    let platform = FakePlatform::with_terminal_ends(
        Capabilities::full(),
        true,
        false,
        TerminalSize::new(80, 24),
    );
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "pwd",
            &probe,
            &platform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("nonterminal structured output must require serialization");

    assert!(error.render().contains("explicit `encode`/`to`"));
    assert!(sink.is_empty());
    assert!(session.current_status().is_none());
}

#[test]
fn a_structured_stdout_redirection_is_refused_before_execution() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "pwd > out.txt",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("redirected structured output must require serialization");

    assert!(error.render().contains("redirected output"));
    assert!(error.render().contains("explicit `encode`/`to`"));
    assert!(sink.is_empty());
    assert!(session.current_status().is_none());
}

#[test]
fn a_recoverable_error_preserves_state_and_renders_a_diagnostic() {
    let mut session = session();
    let probe = Probe::default();
    submit(&mut session, "let keep = 5", &probe);

    let mut sink = Vec::new();
    let error = session
        .submit(
            "<interactive>",
            "$missing",
            &probe,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("an unknown binding is a recoverable failure");
    assert!(
        !error.render().is_empty(),
        "the failure must render a diagnostic"
    );

    // The earlier binding survives the failed submission.
    assert_eq!(
        submit(&mut session, "export STILL = $keep", &probe),
        SubmitOutcome::Continued
    );
    assert_eq!(session.environment().get("STILL"), Some(OsStr::new("5")));
}

#[test]
fn several_statements_in_one_buffer_run_in_source_order() {
    let mut session = session();
    let probe = Probe::default();

    submit(&mut session, "let a = 1\nexport B = $a", &probe);
    assert_eq!(session.environment().get("B"), Some(OsStr::new("1")));
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("flashshell-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("temporary directory should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).expect("temporary directory should be removed");
    }
}

#[test]
fn the_external_members_around_an_internal_island_share_one_process_group() {
    let temp = TempDir::new("session-mixed-group");
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-job-observer-fixture"));
    let mut environment = environment();
    environment.set(
        "FLASH_PROBE_REPORT",
        temp.path().join("report.bin").into_os_string(),
    );
    environment.set(
        "FLASH_PROBE_GROUP_REPORT",
        temp.path().as_os_str().to_os_string(),
    );
    let mut session = Session::new(temp.path(), environment, SessionOptions::default());
    let probe = Probe::new([fixture.as_os_str()]);
    let observer = fixture.to_string_lossy().into_owned();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            format!("^{observer} | from text | to text | ^{observer}"),
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the mixed pipeline should execute");

    // An internal island splits the pipeline but not the job: the external
    // stages on either side of it stay members of the same group, and that
    // group is not the shell's own.
    let groups = reported_groups(temp.path());
    assert_eq!(groups.len(), 2, "both external members report a group");
    assert_eq!(groups[0], groups[1]);
    assert_ne!(groups[0], shell_group());
}

/// The group of a child spawned without a placement, which is the shell's own.
fn shell_group() -> u64 {
    let temp = TempDir::new("session-shell-group");
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-job-observer-fixture"));
    let argv = [OsString::from("inheritor")];
    let environment = [
        (
            OsString::from("FLASH_PROBE_REPORT"),
            temp.path().join("report.bin").into_os_string(),
        ),
        (
            OsString::from("FLASH_PROBE_GROUP_REPORT"),
            temp.path().as_os_str().to_os_string(),
        ),
    ];
    let request = SpawnRequest::new(&fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid");
    let mut child = PosixPlatform.spawn(&request).expect("the fixture spawns");
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));

    let groups = reported_groups(temp.path());
    assert_eq!(groups.len(), 1);
    groups[0]
}

/// Every process group reported by the observers that ran in `directory`.
fn reported_groups(directory: &Path) -> Vec<u64> {
    let mut groups: Vec<u64> = fs::read_dir(directory)
        .expect("the probe directory should be readable")
        .map(|entry| entry.expect("the entry should be readable").path())
        .filter(|path| path.extension() == Some(OsStr::new("group")))
        .map(|path| {
            fs::read_to_string(path)
                .expect("the fixture should report its process group")
                .trim()
                .parse()
                .expect("a process group is an integer")
        })
        .collect();
    groups.sort_unstable();
    groups
}
