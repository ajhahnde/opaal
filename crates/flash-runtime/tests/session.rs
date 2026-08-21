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
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};

use flash_platform::{
    Capabilities, Capability, ChildProcess, DescriptorEndpoint, DescriptorReadError, FakePlatform,
    FileActionError, FileIoEndpoint, FileOpenRequest, ForegroundTerminalGuard, JobSignal,
    PipeEndpoints, PipeError, Platform, PlatformError, ProcessGroup, ProcessGroupId, ProcessStatus,
    ProcessTransition, RecordingPlatform, SignalError, SpawnError, SpawnRequest, TerminalSize,
    TerminateError, WaitError, WorkingDirectoryError, WorkingDirectoryRequest,
};
use flash_platform_posix::PosixPlatform;
use flash_runtime::builtin::standard_registry;
use flash_runtime::command::{CommandLifecycle, CommandNamespaceEntry, CommandRegistry};
use flash_runtime::eval::{Clock, FakeClock, Instant};
use flash_runtime::job::{JobMemberState, JobPlacement, JobState, ProcessId};
use flash_runtime::plan::SessionOptions;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::execute_script;
use flash_runtime::session::{
    BackgroundFailureReason, JobNoticeKind, LiveJobState, Session, SubmitOutcome,
};
use flash_runtime::{Duration, Environment, ScopeStack, Status, Value};

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

#[derive(Debug, Default)]
struct TickingClock {
    now: AtomicU64,
}

impl Clock for TickingClock {
    fn now(&self) -> Instant {
        Instant::from_nanos(self.now.fetch_add(10, Ordering::SeqCst))
    }
}

#[derive(Debug, Default)]
struct FailingOutput;

impl io::Write for FailingOutput {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "the injected session output failed",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct BackgroundEndpoint {
    active: Arc<AtomicUsize>,
    read_failure: Option<usize>,
    write_failure: Option<usize>,
    write_failure_barrier: Option<Arc<Barrier>>,
    read_data: Option<Vec<u8>>,
}

impl BackgroundEndpoint {
    fn new(
        active: Arc<AtomicUsize>,
        read_failure: Option<usize>,
        write_failure: Option<usize>,
        write_failure_barrier: Option<Arc<Barrier>>,
        read_data: Option<Vec<u8>>,
    ) -> Self {
        active.fetch_add(1, Ordering::SeqCst);
        Self {
            active,
            read_failure,
            write_failure,
            write_failure_barrier,
            read_data,
        }
    }
}

impl Drop for BackgroundEndpoint {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl DescriptorEndpoint for BackgroundEndpoint {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, DescriptorReadError> {
        if let Some(index) = self.read_failure {
            Err(DescriptorReadError::Operation {
                kind: io::ErrorKind::Other,
                message: format!("the injected mixed-pipeline read failed at pipe {index}"),
            })
        } else if let Some(data) = self.read_data.take() {
            let amount = data.len().min(buffer.len());
            buffer[..amount].copy_from_slice(&data[..amount]);
            Ok(amount)
        } else {
            Ok(0)
        }
    }

    fn write(&mut self, buffer: &[u8]) -> Result<usize, flash_platform::DescriptorWriteError> {
        if let Some(index) = self.write_failure {
            if let Some(barrier) = &self.write_failure_barrier {
                barrier.wait();
            }
            Err(flash_platform::DescriptorWriteError::Operation {
                kind: io::ErrorKind::Other,
                message: format!("the injected mixed-pipeline write failed at pipe {index}"),
            })
        } else {
            Ok(buffer.len())
        }
    }
}

impl FileIoEndpoint for BackgroundEndpoint {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, FileActionError> {
        if let Some(data) = self.read_data.take() {
            let amount = data.len().min(buffer.len());
            buffer[..amount].copy_from_slice(&data[..amount]);
            Ok(amount)
        } else {
            Ok(0)
        }
    }

    fn write(&mut self, buffer: &[u8]) -> Result<usize, FileActionError> {
        Ok(buffer.len())
    }
}

type ChildStep = Result<ProcessTransition, WaitError>;
type ContinuedSteps = (
    ProcessGroupId,
    mpsc::Sender<ChildStep>,
    Vec<ProcessTransition>,
);

struct BackgroundChild {
    id: u64,
    group: ProcessGroupId,
    steps: mpsc::Receiver<ChildStep>,
    wait_entries: mpsc::Sender<usize>,
    waits: usize,
    require_observer_thread: bool,
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
        if self.require_observer_thread
            && !std::thread::current()
                .name()
                .is_some_and(|name| name.starts_with("flash-child-observer-"))
        {
            return Err(WaitError::new(
                io::ErrorKind::Other,
                "the child was not transferred to a prepared observer",
            ));
        }
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
                ProcessTransition::Continued => {}
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
    operations: Arc<Mutex<Vec<ForegroundOperation>>>,
    continue_steps: Mutex<Vec<ContinuedSteps>>,
    foreground_steps: Mutex<Vec<(ProcessGroupId, mpsc::Sender<ChildStep>, ProcessTransition)>>,
    foreground_attempts: AtomicUsize,
    foreground_handovers: Mutex<Vec<ProcessGroupId>>,
    foreground_releases: Arc<AtomicUsize>,
    foreground_refusal: Mutex<Option<String>>,
    foreground_restore_refusal: Mutex<Option<String>>,
    /// When set, every group signal is refused with this message.
    signal_refusal: Mutex<Option<String>>,
    pipe_calls: AtomicUsize,
    pipe_read_failure: Mutex<Option<usize>>,
    pipe_write_failures: Mutex<Vec<usize>>,
    pipe_write_failure_barrier: Mutex<Option<Arc<Barrier>>>,
    pipe_read_data: Mutex<BTreeMap<usize, Vec<u8>>>,
    descriptor_read_data: Mutex<VecDeque<Vec<u8>>>,
    spawn_calls: AtomicUsize,
    spawn_failure: Mutex<Option<usize>>,
    active_endpoints: Arc<AtomicUsize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ForegroundOperation {
    Handover(u64),
    Signal(u64, JobSignal),
    Restore,
}

#[derive(Debug)]
struct ControlledForegroundGuard {
    releases: Arc<AtomicUsize>,
    operations: Arc<Mutex<Vec<ForegroundOperation>>>,
    refusal: Option<String>,
    released: bool,
}

impl ForegroundTerminalGuard for ControlledForegroundGuard {
    fn restore(&mut self) -> Result<(), PlatformError> {
        if !self.released {
            self.released = true;
            self.releases.fetch_add(1, Ordering::SeqCst);
            self.operations
                .lock()
                .expect("foreground operation lock")
                .push(ForegroundOperation::Restore);
            if let Some(reason) = self.refusal.take() {
                return Err(PlatformError::Unavailable {
                    capability: Capability::ForegroundTerminal,
                    reason,
                });
            }
        }
        Ok(())
    }

    fn previous_owner(&self) -> Option<ProcessGroupId> {
        None
    }
}

impl Drop for ControlledForegroundGuard {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.releases.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl ControlledBackgroundPlatform {
    /// Refuse every subsequent group signal, as a host with no such group would.
    fn refuse_signals(&self, message: &str) {
        *self.signal_refusal.lock().expect("refusal lock") = Some(message.to_owned());
    }

    /// Refuse every subsequent foreground-terminal handoff.
    fn refuse_foreground(&self, message: &str) {
        *self
            .foreground_refusal
            .lock()
            .expect("foreground refusal lock") = Some(message.to_owned());
    }

    /// Refuse every subsequent explicit return of terminal ownership.
    fn refuse_foreground_restore(&self, message: &str) {
        *self
            .foreground_restore_refusal
            .lock()
            .expect("foreground restore refusal lock") = Some(message.to_owned());
    }

    fn foreground_attempts(&self) -> usize {
        self.foreground_attempts.load(Ordering::SeqCst)
    }

    fn foreground_handovers(&self) -> Vec<u64> {
        self.foreground_handovers
            .lock()
            .expect("foreground handover lock")
            .iter()
            .map(|group| group.get())
            .collect()
    }

    fn foreground_releases(&self) -> usize {
        self.foreground_releases.load(Ordering::SeqCst)
    }

    fn foreground_operations(&self) -> Vec<ForegroundOperation> {
        self.operations
            .lock()
            .expect("foreground operation lock")
            .clone()
    }

    fn fail_pipe_read(&self, pipe_index: usize) {
        *self.pipe_read_failure.lock().expect("pipe failure lock") = Some(pipe_index);
    }

    fn fail_pipe_write(&self, pipe_index: usize) {
        self.pipe_write_failures
            .lock()
            .expect("pipe failure lock")
            .push(pipe_index);
    }

    fn feed_pipe_read(&self, pipe_index: usize, data: impl Into<Vec<u8>>) {
        self.pipe_read_data
            .lock()
            .expect("pipe data lock")
            .insert(pipe_index, data.into());
    }

    fn feed_descriptor_read(&self, data: impl Into<Vec<u8>>) {
        self.descriptor_read_data
            .lock()
            .expect("descriptor data lock")
            .push_back(data.into());
    }

    fn synchronize_pipe_write_failures(&self, count: usize) {
        *self
            .pipe_write_failure_barrier
            .lock()
            .expect("pipe failure barrier lock") = Some(Arc::new(Barrier::new(count)));
    }

    fn fail_spawn(&self, spawn_index: usize) {
        *self.spawn_failure.lock().expect("spawn failure lock") = Some(spawn_index);
    }

    fn active_endpoints(&self) -> usize {
        self.active_endpoints.load(Ordering::SeqCst)
    }

    fn endpoint(
        &self,
        read_failure: Option<usize>,
        write_failure: Option<usize>,
        write_failure_barrier: Option<Arc<Barrier>>,
        read_data: Option<Vec<u8>>,
    ) -> BackgroundEndpoint {
        BackgroundEndpoint::new(
            Arc::clone(&self.active_endpoints),
            read_failure,
            write_failure,
            write_failure_barrier,
            read_data,
        )
    }

    /// Complete one stopped child after the shell successfully continues it.
    fn complete_on_continue(
        &self,
        group: u64,
        control: &BackgroundChildControl,
        status: ProcessStatus,
    ) {
        self.step_on_continue(group, control, ProcessTransition::Completed(status));
    }

    /// Publish one controlled transition after a successful group continuation.
    fn step_on_continue(
        &self,
        group: u64,
        control: &BackgroundChildControl,
        transition: ProcessTransition,
    ) {
        self.steps_on_continue(group, control, vec![transition]);
    }

    /// Publish controlled transitions after one successful group continuation.
    fn steps_on_continue(
        &self,
        group: u64,
        control: &BackgroundChildControl,
        transitions: Vec<ProcessTransition>,
    ) {
        self.continue_steps
            .lock()
            .expect("continue step lock")
            .push((
                ProcessGroupId::new(group).expect("test group is nonzero"),
                control.steps.clone(),
                transitions,
            ));
    }

    /// Publish one controlled transition after the group receives the terminal.
    fn step_on_foreground(
        &self,
        group: u64,
        control: &BackgroundChildControl,
        transition: ProcessTransition,
    ) {
        self.foreground_steps
            .lock()
            .expect("foreground step lock")
            .push((
                ProcessGroupId::new(group).expect("test group is nonzero"),
                control.steps.clone(),
                transition,
            ));
    }

    fn new(ids: &[u64]) -> (Self, Vec<BackgroundChildControl>) {
        Self::in_groups(&[ids])
    }

    /// Script one process group per inner slice, sharing that slice's first id.
    ///
    /// Separate jobs need separate groups before a signal test can tell which
    /// job a delivery reached.
    fn in_groups(groups: &[&[u64]]) -> (Self, Vec<BackgroundChildControl>) {
        Self::in_groups_with_waiter(groups, true)
    }

    /// Script children that the legacy synchronous executor must keep waiting.
    fn in_groups_for_session(groups: &[&[u64]]) -> (Self, Vec<BackgroundChildControl>) {
        Self::in_groups_with_waiter(groups, false)
    }

    fn in_groups_with_waiter(
        groups: &[&[u64]],
        require_observer_thread: bool,
    ) -> (Self, Vec<BackgroundChildControl>) {
        let mut children: VecDeque<Box<dyn ChildProcess>> = VecDeque::new();
        let mut controls = Vec::new();
        for ids in groups {
            let group = ProcessGroupId::new(ids[0]).expect("test group is nonzero");
            for &id in *ids {
                let (step_sender, step_receiver) = mpsc::channel();
                let (wait_sender, wait_receiver) = mpsc::channel();
                let terminate_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                children.push_back(Box::new(BackgroundChild {
                    id,
                    group,
                    steps: step_receiver,
                    wait_entries: wait_sender,
                    waits: 0,
                    require_observer_thread,
                    terminate_calls: Arc::clone(&terminate_calls),
                }));
                controls.push(BackgroundChildControl {
                    steps: step_sender,
                    wait_entries: wait_receiver,
                    terminate_calls,
                });
            }
        }
        (
            Self {
                children: Mutex::new(children),
                signals: Mutex::new(Vec::new()),
                operations: Arc::new(Mutex::new(Vec::new())),
                continue_steps: Mutex::new(Vec::new()),
                foreground_steps: Mutex::new(Vec::new()),
                foreground_attempts: AtomicUsize::new(0),
                foreground_handovers: Mutex::new(Vec::new()),
                foreground_releases: Arc::new(AtomicUsize::new(0)),
                foreground_refusal: Mutex::new(None),
                foreground_restore_refusal: Mutex::new(None),
                signal_refusal: Mutex::new(None),
                pipe_calls: AtomicUsize::new(0),
                pipe_read_failure: Mutex::new(None),
                pipe_write_failures: Mutex::new(Vec::new()),
                pipe_write_failure_barrier: Mutex::new(None),
                pipe_read_data: Mutex::new(BTreeMap::new()),
                descriptor_read_data: Mutex::new(VecDeque::new()),
                spawn_calls: AtomicUsize::new(0),
                spawn_failure: Mutex::new(None),
                active_endpoints: Arc::new(AtomicUsize::new(0)),
            },
            controls,
        )
    }
}

impl Platform for ControlledBackgroundPlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities::full()
    }

    fn is_terminal(&self) -> bool {
        true
    }

    // Structured output is only rendered for an interactive terminal, so a
    // controlled job table has to be inspectable through the same boundary a
    // real session uses.
    fn is_output_terminal(&self) -> bool {
        true
    }

    fn shell_executable(&self) -> Result<PathBuf, PlatformError> {
        self.require(Capability::ShellExecutable)?;
        Ok(PathBuf::from("/fake/fsh"))
    }

    fn ignore_hangup(&self) -> Result<(), PlatformError> {
        self.require(Capability::HangupDisposition)
    }

    fn resolve_working_directory(
        &self,
        request: WorkingDirectoryRequest<'_>,
    ) -> Result<PathBuf, WorkingDirectoryError> {
        self.require(Capability::WorkingDirectory)?;
        Ok(if request.path().is_absolute() {
            request.path().to_owned()
        } else {
            request.cwd().join(request.path())
        })
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        let index = self.pipe_calls.fetch_add(1, Ordering::SeqCst);
        let read_failure = self
            .pipe_read_failure
            .lock()
            .expect("pipe failure lock")
            .filter(|candidate| *candidate == index);
        let write_failure = self
            .pipe_write_failures
            .lock()
            .expect("pipe failure lock")
            .contains(&index)
            .then_some(index);
        let read_data = self
            .pipe_read_data
            .lock()
            .expect("pipe data lock")
            .remove(&index);
        let write_failure_barrier = write_failure.and_then(|_| {
            self.pipe_write_failure_barrier
                .lock()
                .expect("pipe failure barrier lock")
                .clone()
        });
        Ok(PipeEndpoints::new(
            Box::new(self.endpoint(read_failure, None, None, read_data)),
            Box::new(self.endpoint(None, write_failure, write_failure_barrier, None)),
        ))
    }

    fn open_file(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Ok(Box::new(self.endpoint(None, None, None, None)))
    }

    fn open_file_io(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn FileIoEndpoint>, FileActionError> {
        Ok(Box::new(self.endpoint(None, None, None, None)))
    }

    fn inherit_descriptor(
        &self,
        _descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Ok(Box::new(self.endpoint(None, None, None, None)))
    }

    fn read_descriptor(
        &self,
        _endpoint: &dyn DescriptorEndpoint,
        buffer: &mut [u8],
    ) -> Result<usize, DescriptorReadError> {
        let Some(data) = self
            .descriptor_read_data
            .lock()
            .expect("descriptor data lock")
            .pop_front()
        else {
            return Ok(0);
        };
        let amount = data.len().min(buffer.len());
        buffer[..amount].copy_from_slice(&data[..amount]);
        Ok(amount)
    }

    fn enter_foreground(
        &self,
        group: ProcessGroupId,
    ) -> Result<Box<dyn ForegroundTerminalGuard>, PlatformError> {
        self.foreground_attempts.fetch_add(1, Ordering::SeqCst);
        if let Some(reason) = self
            .foreground_refusal
            .lock()
            .expect("foreground refusal lock")
            .as_ref()
            .cloned()
        {
            return Err(PlatformError::Unavailable {
                capability: Capability::ForegroundTerminal,
                reason,
            });
        }
        self.foreground_handovers
            .lock()
            .expect("foreground handover lock")
            .push(group);
        self.operations
            .lock()
            .expect("foreground operation lock")
            .push(ForegroundOperation::Handover(group.get()));
        let transition = {
            let mut steps = self.foreground_steps.lock().expect("foreground step lock");
            steps
                .iter()
                .position(|(candidate, _, _)| *candidate == group)
                .map(|index| steps.remove(index))
        };
        if let Some((_, steps, transition)) = transition {
            let _ = steps.send(Ok(transition));
        }
        Ok(Box::new(ControlledForegroundGuard {
            releases: Arc::clone(&self.foreground_releases),
            operations: Arc::clone(&self.operations),
            refusal: self
                .foreground_restore_refusal
                .lock()
                .expect("foreground restore refusal lock")
                .clone(),
            released: false,
        }))
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        let index = self.spawn_calls.fetch_add(1, Ordering::SeqCst);
        if self
            .spawn_failure
            .lock()
            .expect("spawn failure lock")
            .is_some_and(|candidate| candidate == index)
        {
            return Err(SpawnError::Operation {
                kind: io::ErrorKind::Other,
                message: "the injected mixed-pipeline spawn failed".to_owned(),
            });
        }
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
        self.operations
            .lock()
            .expect("foreground operation lock")
            .push(ForegroundOperation::Signal(group.get(), signal));
        if signal == JobSignal::Continue {
            let transition = {
                let mut steps = self.continue_steps.lock().expect("continue step lock");
                steps
                    .iter()
                    .position(|(candidate, _, _)| *candidate == group)
                    .map(|index| steps.remove(index))
            };
            if let Some((_, steps, transitions)) = transition {
                for transition in transitions {
                    let _ = steps.send(Ok(transition));
                }
            }
        }
        Ok(())
    }
}

fn wait_for_notice(session: &mut Session) -> flash_runtime::session::JobNotice {
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

fn submit_error(text: &str) -> String {
    let mut session = session();
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            text,
            &Probe::default(),
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("submission should be rejected")
        .render()
        .to_owned()
}

#[test]
fn job_specs_are_exact_and_argument_anchored() {
    for (source, message, location) in [
        (
            "fg %0",
            "job identity must be a nonzero decimal number",
            "<interactive>:1:4",
        ),
        (
            "fg 1",
            "job arguments use `%n`, not a bare process or job number",
            "<interactive>:1:4",
        ),
        (
            "fg %-1",
            "job identity must contain only ASCII decimal digits",
            "<interactive>:1:4",
        ),
        (
            "fg %+",
            "job identity must contain only ASCII decimal digits",
            "<interactive>:1:4",
        ),
        (
            "fg %1x",
            "job identity must contain only ASCII decimal digits",
            "<interactive>:1:4",
        ),
        ("wait %1 %1", "job `%1` is repeated", "<interactive>:1:9"),
    ] {
        let rendered = submit_error(source);
        assert!(
            rendered.contains(message),
            "`{source}` should report `{message}`:\n{rendered}"
        );
        assert!(
            rendered.contains(location),
            "`{source}` should anchor at {location}:\n{rendered}"
        );
    }
}

#[test]
fn job_command_arity_and_kill_selectors_are_source_anchored() {
    for (source, message, location) in [
        (
            "jobs %1",
            "jobs accepts no job arguments",
            "<interactive>:1:6",
        ),
        (
            "fg %1 %2",
            "fg accepts at most one job argument",
            "<interactive>:1:7",
        ),
        (
            "kill --bogus %1",
            "unknown signal selector `--bogus`",
            "<interactive>:1:6",
        ),
        (
            "kill --stop --kill %1",
            "kill accepts only one signal selector",
            "<interactive>:1:13",
        ),
        (
            "kill %1 --stop",
            "a signal selector must precede every job argument",
            "<interactive>:1:9",
        ),
        (
            "kill --stop",
            "kill requires at least one explicit `%n` target",
            "<interactive>:1:1",
        ),
    ] {
        let rendered = submit_error(source);
        assert!(
            rendered.contains(message),
            "`{source}` should report `{message}`:\n{rendered}"
        );
        assert!(
            rendered.contains(location),
            "`{source}` should anchor at {location}:\n{rendered}"
        );
    }
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
fn a_failing_statement_discards_its_pending_scope_and_environment() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "mut keep = 0\n\
             export COMMITTED = 'yes'\n\
             while $keep < 1 {\n\
                 $keep = 1\n\
                 export LEAK = 'no'\n\
                 $missing\n\
             }",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("the reached unknown binding should fail the final statement");

    assert!(
        error.render().contains("unknown binding \"missing\""),
        "{}",
        error.render()
    );
    assert_eq!(
        session.environment().get("COMMITTED"),
        Some(OsStr::new("yes"))
    );
    assert_eq!(session.environment().get("LEAK"), None);
    assert_eq!(
        submit(&mut session, "export RETAINED = $keep", &probe),
        SubmitOutcome::Continued
    );
    assert_eq!(session.environment().get("RETAINED"), Some(OsStr::new("0")));
}

#[test]
fn catch_handles_host_runtime_errors_and_rolls_back_session_state() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(
            &mut session,
            "try {\n\
                 cd /changed\n\
                 export LEAK = 'no'\n\
                 check\n\
             } catch error {\n\
                 export CAUGHT = $error.category\n\
             }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(session.cwd(), Path::new("/work"));
    assert_eq!(session.environment().get("LEAK"), None);
    assert_eq!(
        session.environment().get("CAUGHT"),
        Some(OsStr::new("control"))
    );
}

#[test]
fn unsuccessful_status_and_explicit_exit_are_not_caught() {
    let clock = Arc::new(FakeClock::new());
    let mut active = session();
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[690]]);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(7))))
        .expect("complete the external command unsuccessfully");
    let probe = Probe::new(["/bin/tool"]);
    let mut sink = Vec::new();

    assert_eq!(
        active
            .submit(
                "<interactive>",
                "mut caught = false\n\
                 try { ^tool } catch error { $caught = true }\n\
                 export CAUGHT = $caught",
                &probe,
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect("an ordinary unsuccessful status is not a runtime error"),
        SubmitOutcome::Continued
    );
    assert_eq!(
        active.environment().get("CAUGHT"),
        Some(OsStr::new("false"))
    );
    assert_eq!(active.current_status().and_then(Status::code), Some(7));

    let mut exiting = session();
    assert_eq!(
        submit(
            &mut exiting,
            "try { exit 9 } catch error { export CAUGHT = 'yes' }",
            &Probe::default(),
        ),
        SubmitOutcome::Exit(9)
    );
    assert_eq!(exiting.environment().get("CAUGHT"), None);
}

#[test]
fn fatal_output_failure_bypasses_catch() {
    let mut session = session();
    let mut output = FailingOutput;
    let error = session
        .submit(
            "<interactive>",
            "try { pwd } catch error { export CAUGHT = 'yes' }",
            &Probe::default(),
            &terminal_platform(),
            &FakeClock::new(),
            &mut output,
        )
        .expect_err("fatal output failure must bypass language catch");

    assert!(error.render().is_empty());
    assert_eq!(session.environment().get("CAUGHT"), None);
}

#[test]
fn nested_commands_run_only_in_selected_control_flow() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(
            &mut session,
            "if false { cd /skipped }\nif true { cd /selected }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(session.cwd(), Path::new("/selected"));
    assert_eq!(session.current_status().and_then(Status::code), Some(0));
}

#[test]
fn current_status_reads_are_live_across_foreground_commands() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(
            &mut session,
            "if $status == null { export BEFORE_STATUS = 'none' }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.environment().get("BEFORE_STATUS"),
        Some(OsStr::new("none"))
    );
    assert_eq!(
        submit(&mut session, "def current_code() { $status.code }", &probe,),
        SubmitOutcome::Continued
    );

    assert_eq!(
        submit(&mut session, "cd /status-read", &probe),
        SubmitOutcome::Continued
    );
    assert_eq!(
        submit(
            &mut session,
            "if $status.ok && current_code() == 0 && $status.signal == null {\n\
                 export AFTER_STATUS = 'success'\n\
             }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.environment().get("AFTER_STATUS"),
        Some(OsStr::new("success"))
    );
}

#[test]
fn nested_commands_compose_through_conditions_loops_match_and_grouping() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(
            &mut session,
            "if cd /condition { cd selected } else { cd /skipped }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(session.cwd(), Path::new("/condition/selected"));

    assert_eq!(
        submit(
            &mut session,
            "mut iteration = 0\n\
             while true {\n\
                 $iteration = $iteration + 1\n\
                 if $iteration == 1 {\n\
                     cd continued\n\
                     continue\n\
                 }\n\
                 cd broken\n\
                 break\n\
             }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.cwd(),
        Path::new("/condition/selected/continued/broken")
    );

    assert_eq!(
        submit(
            &mut session,
            "for path in ['/for-first', '/for-last'] { cd $path }\n\
             match 2 {\n\
                 1 => { cd /skipped }\n\
                 2 => { cd /matched }\n\
             }\n\
             let grouped = (cd /grouped)",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(session.cwd(), Path::new("/grouped"));
    assert_eq!(session.current_status().and_then(Status::code), Some(0));
}

#[test]
fn callable_failures_and_exit_keep_the_statement_transaction_boundary() {
    let probe = Probe::default();

    let mut runtime_failure = session();
    assert_eq!(
        submit(
            &mut runtime_failure,
            "def fail() {\n\
                 cd /runtime-leak\n\
                 missing-command\n\
             }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    let mut sink = Vec::new();
    runtime_failure
        .submit(
            "<interactive>",
            "fail()",
            &probe,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("a command resolution failure should escape the callable");
    assert_eq!(runtime_failure.cwd(), Path::new("/work"));
    assert!(runtime_failure.current_status().is_none());

    let mut output_failure = session();
    assert_eq!(
        submit(
            &mut output_failure,
            "def fail_output() {\n\
                 cd /output-leak\n\
                 pwd\n\
             }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    let error = output_failure
        .submit(
            "<interactive>",
            "fail_output()",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut FailingOutput,
        )
        .expect_err("a fatal output failure should escape the callable");
    let flash_runtime::session::SubmitError::Output(error) = error else {
        panic!("a callable sink failure must retain its output-error channel: {error:?}");
    };
    assert!(
        error
            .to_string()
            .contains("the injected session output failed")
    );
    assert_eq!(output_failure.cwd(), Path::new("/work"));
    assert!(output_failure.current_status().is_none());

    let mut explicit_exit = session();
    assert_eq!(
        submit(
            &mut explicit_exit,
            "def leave() {\n\
                 cd /committed-exit\n\
                 exit 7\n\
             }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(
        submit(&mut explicit_exit, "leave()", &probe),
        SubmitOutcome::Exit(7)
    );
    assert_eq!(explicit_exit.cwd(), Path::new("/committed-exit"));
}

#[test]
fn named_functions_and_closures_reuse_the_active_session_host() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(
            &mut session,
            "def enter(path) {\n\
                 cd $path\n\
                 return $(pwd)\n\
             }\n\
             let entered = enter('/function')\n\
             export ENTERED = $entered",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(session.cwd(), Path::new("/function"));
    assert_eq!(
        session.environment().get("ENTERED"),
        Some(OsStr::new("/function"))
    );

    assert_eq!(
        submit(
            &mut session,
            "let enter_closure = {|path| cd $path}\n\
             let result = $enter_closure('/closure')\n\
             if $result { export CLOSURE_REACHED = 'yes' }",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(session.cwd(), Path::new("/closure"));
    assert_eq!(
        session.environment().get("CLOSURE_REACHED"),
        Some(OsStr::new("yes"))
    );
    assert_eq!(session.current_status().and_then(Status::code), Some(0));
}

#[test]
fn command_substitution_captures_only_when_reached() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(
            &mut session,
            "if false { let ignored = $(cd /skipped) }\n\
             let captured = $(pwd)\n\
             let label = \"cwd=$(pwd)\"\n\
             export CAPTURED = $captured\n\
             export CAPTURED_LABEL = $label",
            &probe,
        ),
        SubmitOutcome::Continued
    );
    assert_eq!(session.cwd(), Path::new("/work"));
    assert_eq!(
        session.environment().get("CAPTURED"),
        Some(OsStr::new("/work"))
    );
    assert_eq!(
        session.environment().get("CAPTURED_LABEL"),
        Some(OsStr::new("cwd=/work"))
    );
}

#[test]
fn command_substitution_captures_a_mixed_internal_tail() {
    let clock = FakeClock::new();
    let mut session = session();
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[901]]);
    let probe = Probe::new(["/bin/tool"]);
    platform.feed_pipe_read(0, b"abc".to_vec());
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the external producer");
    let mut sink = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "let captured = $(^tool | decode utf8)\nexport CAPTURED = $captured",
                &probe,
                &platform,
                &clock,
                &mut sink,
            )
            .expect("the mixed internal tail should capture"),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.environment().get("CAPTURED"),
        Some(OsStr::new("abc"))
    );
    assert!(sink.is_empty());
    assert_eq!(platform.active_endpoints(), 0);
}

#[test]
fn command_substitution_captures_a_mixed_external_tail() {
    let clock = FakeClock::new();
    let mut session = session();
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[902]]);
    let probe = Probe::new(["/bin/tool"]);
    platform.feed_descriptor_read(b"external tail\r\n".to_vec());
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(7))))
        .expect("complete the external tail");
    let mut sink = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "let captured = $(which pwd | get kind | encode utf8 | ^tool)\n\
                 export CAPTURED = $captured",
                &probe,
                &platform,
                &clock,
                &mut sink,
            )
            .expect("the mixed external tail should capture"),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.environment().get("CAPTURED"),
        Some(OsStr::new("external tail"))
    );
    assert_eq!(session.current_status().and_then(Status::code), Some(7));
    assert!(sink.is_empty());
    assert_eq!(platform.active_endpoints(), 0);
}

#[test]
fn typed_command_substitution_preserves_bytes_and_explicit_text() {
    let clock = FakeClock::new();
    let mut session = session();
    let (platform, controls) =
        ControlledBackgroundPlatform::in_groups_for_session(&[&[903], &[904]]);
    let probe = Probe::new(["/bin/tool"]);
    platform.feed_descriptor_read(vec![0, 0xff, b'\n']);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(7))))
        .expect("complete the byte producer");
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the text producer");
    let mut sink = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "let binary = $(bytes: ^tool)",
                &probe,
                &platform,
                &clock,
                &mut sink,
            )
            .expect("both typed capture modes should succeed"),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.scope().get("binary"),
        Some(&Value::bytes(vec![0, 0xff, b'\n']))
    );
    assert_eq!(session.current_status().and_then(Status::code), Some(7));

    platform.feed_descriptor_read(b"line\r\n".to_vec());
    assert_eq!(
        session
            .submit(
                "<interactive>",
                "let text = $(text: ^tool)",
                &probe,
                &platform,
                &clock,
                &mut sink,
            )
            .expect("explicit text capture should succeed"),
        SubmitOutcome::Continued
    );
    assert_eq!(session.scope().get("text"), Some(&Value::string("line")));
    assert_eq!(session.current_status().and_then(Status::code), Some(0));
    assert!(sink.is_empty());
    assert_eq!(platform.active_endpoints(), 0);
}

#[test]
fn byte_capture_uses_the_shared_callable_mixed_pipeline_and_reachability_paths() {
    let clock = FakeClock::new();
    let mut session = session();
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[905]]);
    let probe = Probe::new(["/bin/tool"]);
    platform.feed_pipe_read(0, b"callable bytes\n".to_vec());
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(9))))
        .expect("complete the external producer");
    let mut sink = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "def collect() { return $(bytes: ^tool | decode utf8 | encode utf8) }\n\
                 if false { let skipped = $(bytes: ^missing) }\n\
                 let binary = collect()",
                &probe,
                &platform,
                &clock,
                &mut sink,
            )
            .expect("reached callable capture should use the ordinary mixed pipeline"),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.scope().get("binary"),
        Some(&Value::bytes(b"callable bytes\n".to_vec()))
    );
    assert_eq!(session.current_status().and_then(Status::code), Some(0));
    assert!(sink.is_empty());
    assert_eq!(platform.active_endpoints(), 0);
}

#[test]
fn typed_byte_capture_honors_exact_and_overflow_limits_after_reaping() {
    let probe = Probe::new(["/bin/tool"]);

    let mut exact = Session::new(
        "/work",
        environment(),
        SessionOptions::default().with_capture_limit(3),
    );
    let (exact_platform, exact_controls) =
        ControlledBackgroundPlatform::in_groups_for_session(&[&[906]]);
    exact_platform.feed_descriptor_read(b"abc".to_vec());
    exact_controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the exact-limit producer");
    let mut sink = Vec::new();
    exact
        .submit(
            "<interactive>",
            "let binary = $(bytes: ^tool)",
            &probe,
            &exact_platform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the exact raw-byte limit should succeed");
    assert_eq!(
        exact.scope().get("binary"),
        Some(&Value::bytes(b"abc".to_vec()))
    );
    assert_eq!(exact_platform.active_endpoints(), 0);

    let mut overflow = Session::new(
        "/work",
        environment(),
        SessionOptions::default().with_capture_limit(2),
    );
    let (overflow_platform, overflow_controls) =
        ControlledBackgroundPlatform::in_groups_for_session(&[&[907]]);
    overflow_platform.feed_descriptor_read(b"abc".to_vec());
    overflow_controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the overflowing producer");
    let error = overflow
        .submit(
            "<interactive>",
            "let binary = $(bytes: ^tool)",
            &probe,
            &overflow_platform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("one byte beyond the raw limit must fail");
    assert!(error.render().contains("2-byte capture limit"));
    assert_eq!(overflow.scope().get("binary"), None);
    assert_eq!(overflow_platform.active_endpoints(), 0);
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
fn an_alias_runs_through_the_canonical_session_executor() {
    let standard = standard_registry();
    let registry = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(
                standard.lookup("pwd").expect("standard pwd").clone(),
                CommandLifecycle::introduced(1),
            ),
            CommandNamespaceEntry::alias("cwd", "pwd", CommandLifecycle::introduced(1)),
        ],
    )
    .expect("valid namespace");
    let mut session = Session::with_scope_and_registry(
        ScopeStack::new(),
        "/work",
        environment(),
        SessionOptions::default(),
        registry,
    );
    let mut output = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "cwd",
                &Probe::default(),
                &terminal_platform(),
                &FakeClock::new(),
                &mut output,
            )
            .expect("alias should execute through pwd"),
        SubmitOutcome::Continued
    );
    assert!(
        String::from_utf8(output)
            .expect("UTF-8 output")
            .contains("/work")
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
fn a_background_conditional_chain_launches_exactly_one_shell_member() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "^tool && ^other &",
                &probe,
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect("a background conditional chain should launch"),
        SubmitOutcome::Continued
    );

    let started = session
        .next_job_notice()
        .expect("background launch queues a notice");
    let job = session
        .background_job(started.job())
        .expect("the complete job is addressable before its notice");
    assert_eq!(
        job.members().count(),
        1,
        "one shell process supervises the complete chain"
    );

    let records = platform.spawn_log().records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].executable(), Path::new("/fake/fsh"));
    assert_eq!(
        records[0].argv(),
        [
            OsString::from("/fake/fsh"),
            OsString::from("--async-chain"),
            OsString::from("^tool && ^other"),
            OsString::from("--async-capture-limit"),
            OsString::from(SessionOptions::DEFAULT_CAPTURE_LIMIT.to_string()),
        ]
    );
    assert_eq!(records[0].requested(), ProcessGroup::New);
}

#[test]
fn a_reserved_background_head_is_not_classified_as_an_external_pipeline() {
    let registry = CommandRegistry::try_from_entries(
        1,
        [CommandNamespaceEntry::reserved(
            "future",
            1,
            "future command",
            None,
        )],
    )
    .expect("valid namespace");
    let clock = Arc::new(FakeClock::at(100));
    let mut session = Session::with_scope_and_registry(
        ScopeStack::new(),
        "/work",
        environment(),
        SessionOptions::default(),
        registry,
    );
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/future"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "future &",
                &probe,
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect("reserved background work should use shell re-execution"),
        SubmitOutcome::Continued
    );

    let records = platform.spawn_log().records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].executable(), Path::new("/fake/fsh"));
    assert_eq!(
        records[0].argv(),
        [
            OsString::from("/fake/fsh"),
            OsString::from("--async-chain"),
            OsString::from("future"),
            OsString::from("--async-capture-limit"),
            OsString::from(SessionOptions::DEFAULT_CAPTURE_LIMIT.to_string()),
        ]
    );
}

#[test]
fn a_single_external_background_pipeline_still_spawns_directly() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    let platform = RecordingPlatform::new(terminal_platform());
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
            .expect("an external background pipeline should launch directly"),
        SubmitOutcome::Continued
    );

    let started = session
        .next_job_notice()
        .expect("background launch queues a notice");
    let job = session
        .background_job(started.job())
        .expect("the complete job is addressable before its notice");
    assert_eq!(job.members().count(), 2);

    let records = platform.spawn_log().records();
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|record| record.executable() != Path::new("/fake/fsh")),
        "the established external-pipeline path must not add a shell process"
    );
}

#[test]
fn a_direct_background_pipeline_expands_environment_backed_names() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "^tool $HOME &",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the inherited environment should supply the direct argument");

    let records = platform.spawn_log().records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].executable(), Path::new("/bin/tool"));
    assert_eq!(
        records[0].argv(),
        [OsString::from("tool"), OsString::from("/home/me")]
    );
}

#[test]
fn a_background_shell_launch_forwards_session_options() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = Session::new(
        "/work",
        environment(),
        SessionOptions::default()
            .with_pipefail(true)
            .with_capture_limit(4096),
    );
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "^tool || ^other &",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the configured background chain should launch");

    let records = platform.spawn_log().records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].argv(),
        [
            OsString::from("/fake/fsh"),
            OsString::from("--async-chain"),
            OsString::from("^tool || ^other"),
            OsString::from("--async-pipefail"),
            OsString::from("--async-capture-limit"),
            OsString::from("4096"),
        ]
    );
}

#[test]
fn an_unavailable_shell_executable_rejects_the_chain_before_launch() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    let platform = RecordingPlatform::new(FakePlatform::with_terminal(
        Capabilities::full_without(Capability::ShellExecutable),
        true,
        TerminalSize::new(80, 24),
    ));
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "^tool && ^other &",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("the parent needs its executable path before launch");

    assert!(error.render().contains("shell re-execution"));
    assert!(error.render().contains("^tool && ^other"));
    assert!(platform.spawn_log().records().is_empty());
    assert!(session.next_job_notice().is_none());
}

#[test]
fn a_background_chain_reading_a_shell_local_binding_is_rejected_before_launch() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "let n = 5",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the declaration should succeed");

    let error = session
        .submit(
            "<interactive>",
            "^tool $n &",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a shell-local read cannot cross into the subshell");

    assert!(error.render().contains("$n"));
    assert!(
        platform.spawn_log().records().is_empty(),
        "the rejection must happen before any process exists"
    );
}

#[test]
fn a_background_chain_reading_an_environment_backed_binding_is_accepted() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "let NAME = 'value'\nexport NAME = $NAME",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the environment-backed binding should be established");

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "^tool $NAME &",
                &probe,
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect("an environment-backed read can cross into the subshell"),
        SubmitOutcome::Continued
    );
}

#[test]
fn background_chain_validation_reaches_spreads_redirections_and_braced_words() {
    for (declaration, background, reference) in [
        ("let args = ['one']", "^tool ...$args &", "$args"),
        ("let file = 'output'", "^tool > $file &", "$file"),
        ("let n = 5", "^tool pre${$n}post &", "$n"),
    ] {
        let clock = Arc::new(FakeClock::at(100));
        let mut session = session();
        session.enable_interactive_job_control(clock.clone());
        let probe = Probe::new(["/bin/tool"]);
        let platform = RecordingPlatform::new(terminal_platform());
        let mut sink = Vec::new();

        session
            .submit(
                "<interactive>",
                declaration,
                &probe,
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect("the shell-local binding should be established");

        let error = session
            .submit(
                "<interactive>",
                background,
                &probe,
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect_err("every parent-only read should be rejected before launch");

        assert!(error.render().contains(reference));
        assert!(
            platform.spawn_log().records().is_empty(),
            "{background:?} must fail before spawning"
        );
    }
}

#[test]
fn a_background_command_shadowed_by_a_shell_function_is_rejected_before_launch() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/mark"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "def mark() {\n    0\n}",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the function definition should succeed");

    let error = session
        .submit(
            "<interactive>",
            "mark &",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a shell function cannot cross into the subshell");

    assert!(error.render().contains("shell function"));
    assert!(error.render().contains("mark"));
    assert!(platform.spawn_log().records().is_empty());
}

#[test]
fn a_forced_external_background_command_bypasses_a_same_named_shell_function() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/mark"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "def mark() {\n    0\n}",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the function definition should succeed");

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "^mark &",
                &probe,
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect("forced external resolution should bypass the function"),
        SubmitOutcome::Continued
    );
}

#[test]
fn a_background_function_call_is_rejected_as_parent_only_state() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "def mark() {\n    0\n}",
            &Probe::default(),
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the function definition should succeed");

    let error = session
        .submit(
            "<interactive>",
            "mark() &",
            &Probe::default(),
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a shell function call cannot cross into the subshell");

    assert!(error.render().contains("shell function"));
    assert!(error.render().contains("mark()"));
    assert!(platform.spawn_log().records().is_empty());
}

#[test]
fn a_background_closure_parameter_is_not_treated_as_parent_state() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "which pwd | each {|row| $row} &",
                &Probe::default(),
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect("the closure-local reference should launch in the child shell"),
        SubmitOutcome::Continued
    );
    let records = platform.spawn_log().records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].executable(), Path::new("/fake/fsh"));
}

#[test]
fn a_background_internal_pipeline_launches_through_the_shell() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    assert_eq!(
        session
            .submit(
                "<interactive>",
                "pwd &",
                &Probe::default(),
                &platform,
                clock.as_ref(),
                &mut sink,
            )
            .expect("the child shell should own the internal pipeline"),
        SubmitOutcome::Continued
    );

    let started = session
        .next_job_notice()
        .expect("background launch queues a notice");
    let job = session
        .background_job(started.job())
        .expect("the complete job is addressable before its notice");
    assert_eq!(job.members().count(), 1);

    let records = platform.spawn_log().records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].executable(), Path::new("/fake/fsh"));
    assert!(
        records[0].argv().iter().any(|argument| argument == "pwd"),
        "the child shell receives the internal pipeline source"
    );
}

#[test]
fn a_mixed_background_pipeline_launches_through_the_shell() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let probe = Probe::new(["/bin/tool"]);
    let platform = RecordingPlatform::new(terminal_platform());
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd | ^tool &",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("the child shell should own the mixed pipeline");

    let records = platform.spawn_log().records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].executable(), Path::new("/fake/fsh"));
    assert!(
        records[0]
            .argv()
            .iter()
            .any(|argument| argument == "which pwd | ^tool")
    );
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
fn a_join_remains_unbounded_after_sixteen_automatic_resumptions() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[146]);
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
        .expect("release the initial stop");
    assert_eq!(controls[0].wait_entries.recv().expect("second wait"), 2);
    let stopped = wait_for_notice(&mut session);
    session
        .acknowledge_job_notice(stopped.id())
        .expect("acknowledge the stop");

    for signal in 20..36 {
        platform.steps_on_continue(
            146,
            &controls[0],
            vec![
                ProcessTransition::Continued,
                ProcessTransition::Stopped { signal },
            ],
        );
    }
    platform.steps_on_continue(
        146,
        &controls[0],
        vec![
            ProcessTransition::Continued,
            ProcessTransition::Completed(ProcessStatus::Exited(0)),
        ],
    );

    let failures = session.join_background_jobs(&platform);

    assert!(failures.is_empty());
    assert_eq!(
        delivered(&platform),
        vec![(146, JobSignal::Continue); 17],
        "lifetime join does not inherit the command-level resume bound"
    );
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 0);
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

/// Submit one buffer against a controlled platform and return its rendered text.
fn rendered(
    session: &mut Session,
    text: &str,
    probe: &Probe,
    platform: &dyn Platform,
    clock: &FakeClock,
) -> String {
    let mut sink = Vec::new();
    session
        .submit("<interactive>", text, probe, platform, clock, &mut sink)
        .expect("the submission should succeed");
    String::from_utf8(sink).expect("rendered structured output is UTF-8")
}

#[test]
fn planned_help_inspects_a_function_without_executing_or_mutating_runtime_paths() {
    struct PanicProbe;

    impl ExecutableProbe for PanicProbe {
        fn is_executable(&self, _path: &OsStr) -> bool {
            panic!("help inspection must not probe executables")
        }
    }

    let mut session = session();
    let platform = RecordingPlatform::new(terminal_platform());
    let clock = FakeClock::new();
    let definition = concat!(
        "## Inspects without running.\n",
        "## The function body must remain dormant.\n",
        "def dangerous() {\n",
        "    cd /mutated\n",
        "    ^must-not-run\n",
        "}\n",
    );
    let mut definition_output = Vec::new();
    session
        .submit(
            "dangerous.fsh",
            definition,
            &Probe::default(),
            &platform,
            &clock,
            &mut definition_output,
        )
        .expect("the documented function binds without running");
    assert!(definition_output.is_empty());
    let before_cwd = session.cwd().to_owned();
    let before_status = session.current_status().cloned();

    let mut help_output = Vec::new();
    session
        .submit(
            "help.fsh",
            "help dangerous",
            &PanicProbe,
            &platform,
            &clock,
            &mut help_output,
        )
        .expect("help is planned and rendered from an immutable metadata snapshot");

    assert_eq!(
        String::from_utf8(help_output).unwrap(),
        concat!(
            "function dangerous\n",
            "  signature: def dangerous() -> Any\n",
            "  defined at: dangerous.fsh:3:5\n",
            "  summary: Inspects without running.\n",
            "  details:\n",
            "    Inspects without running.\n",
            "    The function body must remain dormant.\n",
        )
    );
    assert_eq!(session.cwd(), before_cwd);
    assert_eq!(session.current_status(), before_status.as_ref());
    assert!(platform.spawn_log().records().is_empty());
    assert!(platform.signal_log().records().is_empty());
}

#[test]
fn help_keeps_builtin_and_function_namespaces_and_orders_list_output() {
    let mut session = session();
    let platform = terminal_platform();
    let clock = FakeClock::new();
    let probe = Probe::default();

    assert_eq!(
        rendered(
            &mut session,
            concat!(
                "## User-defined help function.\n",
                "def help(value: String) -> String { $value }\n",
                "## Last function.\n",
                "def zzz() { null }\n",
            ),
            &probe,
            &platform,
            &clock,
        ),
        ""
    );

    let detail = rendered(&mut session, "help help", &probe, &platform, &clock);
    assert_eq!(
        detail,
        concat!(
            "builtin help\n",
            "  invocation: help [NAME]\n",
            "  input: Empty\n",
            "  output: ByteStream\n",
            "  summary: Inspect built-in and visible function metadata without execution.\n",
            "  details:\n",
            "    Inspect built-in and visible function metadata without execution.\n",
            "\n",
            "function help\n",
            "  signature: def help(value: String) -> String\n",
            "  defined at: <interactive>:2:5\n",
            "  summary: User-defined help function.\n",
            "  details:\n",
            "    User-defined help function.\n",
        )
    );
    let command_detail = rendered(&mut session, "help command", &probe, &platform, &clock);
    assert!(command_detail.contains("invocation: command NAME [ARG...]"));
    assert!(command_detail.contains("Run a command through explicit external resolution."));
    let check_detail = rendered(&mut session, "help check", &probe, &platform, &clock);
    assert!(check_detail.contains("invocation: check"));
    assert!(check_detail.contains("Raise a catchable error unless the upstream stage succeeded."));
    assert_eq!(
        rendered(&mut session, "help 'zzz'", &probe, &platform, &clock),
        concat!(
            "function zzz\n",
            "  signature: def zzz() -> Any\n",
            "  defined at: <interactive>:4:5\n",
            "  summary: Last function.\n",
            "  details:\n",
            "    Last function.\n",
        )
    );

    let list = rendered(&mut session, "help", &probe, &platform, &clock);
    assert!(list.ends_with('\n'));
    assert!(!list.ends_with("\n\n"));
    let keys = list
        .lines()
        .map(|line| {
            let mut fields = line.split('\t');
            (
                fields.next().expect("name").to_owned(),
                fields.next().expect("kind").to_owned(),
            )
        })
        .collect::<Vec<_>>();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    assert!(keys.contains(&("help".to_owned(), "builtin".to_owned())));
    assert!(keys.contains(&("help".to_owned(), "function".to_owned())));
    assert!(keys.contains(&("zzz".to_owned(), "function".to_owned())));
}

#[test]
fn help_rejects_dynamic_queries_and_unknown_names_at_the_query_span() {
    for text in [
        "help $name",
        "help ${$name}",
        "help $(^must-not-run)",
        "help ...$names",
        "help { |value| $value }",
        "help one two",
    ] {
        let mut session = session();
        let mut output = Vec::new();
        let error = session
            .submit(
                "help.fsh",
                text,
                &Probe::default(),
                &terminal_platform(),
                &FakeClock::new(),
                &mut output,
            )
            .expect_err("dynamic or over-arity help queries are rejected");
        assert!(output.is_empty());
        assert!(
            error.render().contains("help"),
            "the diagnostic names help for {text:?}: {}",
            error.render()
        );
    }

    let mut unknown_session = session();
    let mut output = Vec::new();
    let error = unknown_session
        .submit(
            "help.fsh",
            "help missing",
            &Probe::default(),
            &terminal_platform(),
            &FakeClock::new(),
            &mut output,
        )
        .expect_err("an unknown exact name is rejected");
    assert!(output.is_empty());
    assert!(error.render().contains("unknown help name `missing`"));
    assert!(error.render().contains("help.fsh:1:6"));

    let mut dynamic_session = session();
    submit(
        &mut dynamic_session,
        "let command_suffix = 'elp'",
        &Probe::default(),
    );
    let mut output = Vec::new();
    let error = dynamic_session
        .submit(
            "dynamic-head.fsh",
            "h${$command_suffix} missing",
            &Probe::default(),
            &terminal_platform(),
            &FakeClock::new(),
            &mut output,
        )
        .expect_err("a dynamically resolved help head is rejected");
    assert!(output.is_empty());
    assert!(
        error
            .render()
            .contains("help command head must be the static"),
        "{}",
        error.render()
    );
    assert!(error.render().contains("dynamic-head.fsh:1:1"));
}

#[test]
fn help_uses_the_ordinary_redirection_byte_path() {
    let temp = TempDir::new("help-redirection");
    let mut redirected_session =
        Session::new(temp.path(), environment(), SessionOptions::default());
    let mut sink = Vec::new();
    redirected_session
        .submit(
            "redirect.fsh",
            "help pwd > help.txt",
            &Probe::default(),
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("help bytes use ordinary file redirection");
    assert!(sink.is_empty());
    let redirected = fs::read_to_string(temp.path().join("help.txt")).unwrap();
    assert!(redirected.starts_with("builtin pwd\n  invocation: pwd\n"));
    assert!(redirected.ends_with('\n'));
}

#[test]
fn a_mixed_internal_tail_file_override_closes_the_unused_pipeline() {
    let temp = TempDir::new("mixed-internal-tail-redirection");
    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::new(["/bin/cat"]);
    let mut sink = Vec::new();

    session
        .submit(
            "redirect.fsh",
            "help pwd 3>help.txt 1>&3 | ^/bin/cat > downstream.txt",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the external successor should observe EOF on the unused pipe");

    assert!(sink.is_empty());
    let redirected = fs::read_to_string(temp.path().join("help.txt")).unwrap();
    assert!(redirected.starts_with("builtin pwd\n  invocation: pwd\n"));
    assert!(redirected.ends_with('\n'));
    assert_eq!(fs::read(temp.path().join("downstream.txt")).unwrap(), b"");
}

/// Complete every scripted child so no observer outlives the test blocked on a
/// transition the test would otherwise never release.
fn release(controls: &[BackgroundChildControl]) {
    for control in controls {
        let _ = control
            .steps
            .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))));
    }
}

#[test]
fn jobs_requires_a_session_job_coordinator() {
    let rendered = submit_error("jobs");

    assert!(
        rendered.contains("`jobs` requires a session with job control"),
        "a session without a coordinator must say so:\n{rendered}"
    );
    assert!(
        rendered.contains("<interactive>:1:1"),
        "the diagnostic anchors at the command:\n{rendered}"
    );
}

#[test]
fn jobs_reports_nothing_when_no_record_is_addressable() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());

    let listed = rendered(
        &mut session,
        "jobs",
        &Probe::default(),
        &terminal_platform(),
        clock.as_ref(),
    );

    assert_eq!(listed, "");
    assert_eq!(
        session.current_status().and_then(Status::code),
        Some(0),
        "an empty snapshot still succeeds"
    );
}

#[test]
fn jobs_lists_every_addressable_record_in_identity_order() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[211, 212]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("release the stop");
    assert_eq!(controls[0].wait_entries.recv().expect("stop applied"), 2);
    assert_eq!(
        wait_for_notice(&mut session).kind(),
        &JobNoticeKind::Stopped
    );

    assert_eq!(
        rendered(
            &mut session,
            "jobs | get job | collect",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        "[\"%1\", \"%2\"]\n"
    );
    assert_eq!(
        rendered(
            &mut session,
            "jobs | get state | collect",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        "[\"stopped\", \"running\"]\n"
    );

    release(&controls);
}

#[test]
fn a_running_job_row_carries_the_stable_seven_field_schema() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[221]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);

    assert_eq!(
        rendered(&mut session, "jobs", &probe, &platform, clock.as_ref()),
        "job | state   | placement  | group | command | status | signal\n\
         ----+---------+------------+-------+---------+--------+-------\n\
         %1  | running | background | 221   | ^tool   | null   | null  \n",
        "host identities stay strings and absent fields stay null"
    );

    release(&controls);
}

#[test]
fn a_stopped_row_reports_its_raw_stop_signal_and_resume_placement() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[231]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("release the stop");
    assert_eq!(controls[0].wait_entries.recv().expect("stop applied"), 2);
    assert_eq!(
        wait_for_notice(&mut session).kind(),
        &JobNoticeKind::Stopped
    );

    assert_eq!(
        rendered(&mut session, "jobs", &probe, &platform, clock.as_ref()),
        "job | state   | placement  | group | command | status | signal\n\
         ----+---------+------------+-------+---------+--------+-------\n\
         %1  | stopped | background | 231   | ^tool   | null   |     19\n",
        "a stopped row carries the raw platform stop number"
    );

    release(&controls);
}

#[test]
fn a_completed_row_is_listed_without_consuming_its_notice() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[241]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(3))))
        .expect("complete the controlled child");
    let completion = wait_for_notice(&mut session);
    assert_eq!(completion.kind(), &JobNoticeKind::Completed);

    let listed = rendered(&mut session, "jobs", &probe, &platform, clock.as_ref());

    assert!(
        listed.contains("| completed |"),
        "a completed record stays addressable until it is acknowledged:\n{listed}"
    );
    assert!(
        listed.contains("| exit 3 | null"),
        "a completed row carries its aggregate status:\n{listed}"
    );
    assert_eq!(
        session.next_job_notice().as_ref().map(|notice| notice.id()),
        Some(completion.id()),
        "listing a record must not acknowledge its pending notice"
    );
}

#[test]
fn a_quarantined_record_is_listed_as_quarantined() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[251]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    controls[0]
        .steps
        .send(Err(WaitError::new(
            io::ErrorKind::Interrupted,
            "initial failure",
        )))
        .expect("fail the initial observation");
    assert_eq!(controls[0].wait_entries.recv().expect("cleanup wait"), 2);
    controls[0]
        .steps
        .send(Err(WaitError::new(io::ErrorKind::Other, "cleanup failure")))
        .expect("fail the cleanup wait");
    assert!(matches!(
        wait_for_notice(&mut session).kind(),
        JobNoticeKind::ObservationFailed { .. }
    ));

    let listed = rendered(&mut session, "jobs", &probe, &platform, clock.as_ref());

    assert!(
        listed.contains("| quarantined |"),
        "an unobservable record is shown honestly, not as completed:\n{listed}"
    );
    assert!(
        listed.contains("| null   | null"),
        "quarantine never invents a completion status:\n{listed}"
    );
}

#[test]
fn jobs_feeds_the_ordinary_lazy_internal_suffix() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[261, 262]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("release the stop");
    assert_eq!(controls[1].wait_entries.recv().expect("stop applied"), 2);
    assert_eq!(
        wait_for_notice(&mut session).kind(),
        &JobNoticeKind::Stopped
    );

    assert_eq!(
        rendered(
            &mut session,
            "jobs | where {|row| $row.state == 'stopped'} | get job | first 1",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        "%2\n",
        "a read-only snapshot heads the ordinary lazy structured suffix"
    );
    let status = session
        .current_status()
        .expect("the composed pipeline records a status");
    assert_eq!(status.code(), Some(0));
    assert_eq!(
        status.stages().len(),
        4,
        "the snapshot head contributes its own stage status"
    );

    release(&controls);
}

#[test]
fn a_job_command_cannot_share_a_pipeline_with_an_external_stage() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "jobs | to json | ^cat",
            &Probe::new(["/bin/cat"]),
            &terminal_platform(),
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a session-only command must not enter the mixed executor");

    let rendered = error.render();
    assert!(
        rendered.contains("`jobs` cannot share a pipeline with an external command"),
        "the refusal names the job command:\n{rendered}"
    );
    assert!(
        rendered.contains("<interactive>:1:1"),
        "the diagnostic anchors at the job command:\n{rendered}"
    );
    assert!(sink.is_empty());
}

#[test]
fn redirected_jobs_output_still_requires_explicit_serialization() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "jobs > out.txt",
            &Probe::default(),
            &terminal_platform(),
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("redirected structured output must require serialization");

    let rendered = error.render();
    assert!(rendered.contains("redirected output"), "{rendered}");
    assert!(rendered.contains("explicit `encode`/`to`"), "{rendered}");
    assert!(sink.is_empty());
    assert!(session.current_status().is_none());
}

/// The identity the coordinator assigns to the `nth` job of a session.
fn job_id(nth: u64) -> flash_runtime::job::JobId {
    flash_runtime::job::JobId::new(nth).expect("test job identities are nonzero")
}

/// Stop one controlled child and consume the resulting stopped notice.
fn stop_controlled_job(
    session: &mut Session,
    control: &BackgroundChildControl,
    waits_so_far: usize,
) {
    control
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("release the stop");
    assert_eq!(
        control.wait_entries.recv().expect("stop applied"),
        waits_so_far + 1
    );
    assert_eq!(wait_for_notice(session).kind(), &JobNoticeKind::Stopped);
}

/// Submit one buffer expecting a recoverable diagnostic, and return it.
fn rejected(
    session: &mut Session,
    text: &str,
    probe: &Probe,
    platform: &dyn Platform,
    clock: &FakeClock,
) -> String {
    let mut sink = Vec::new();
    session
        .submit("<interactive>", text, probe, platform, clock, &mut sink)
        .expect_err("the submission should be rejected")
        .render()
        .to_owned()
}

/// Every group signal the platform recorded, in delivery order.
fn delivered(platform: &ControlledBackgroundPlatform) -> Vec<(u64, JobSignal)> {
    platform
        .signals
        .lock()
        .expect("signal lock")
        .iter()
        .map(|(group, signal)| (group.get(), *signal))
        .collect()
}

#[test]
fn bg_continues_the_newest_stopped_job_by_default() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[311], &[312]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    stop_controlled_job(&mut session, &controls[1], 1);

    assert_eq!(
        rendered(&mut session, "bg", &probe, &platform, clock.as_ref()),
        "",
        "bg produces no output of its own"
    );

    assert_eq!(
        delivered(&platform),
        vec![(312, JobSignal::Continue)],
        "the newest stopped job is the default target"
    );
    assert_eq!(
        session
            .background_job(job_id(2))
            .expect("the continued job stays addressable")
            .state(),
        JobState::Background
    );
    assert!(
        session.next_job_notice().is_none(),
        "a successful continuation removes the stale stopped notice"
    );
    assert_eq!(session.current_status().and_then(Status::code), Some(0));

    release(&controls);
}

#[test]
fn bg_continues_an_explicit_stopped_target() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[321], &[322]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    stop_controlled_job(&mut session, &controls[0], 1);

    assert_eq!(
        rendered(&mut session, "bg %1", &probe, &platform, clock.as_ref()),
        ""
    );

    assert_eq!(delivered(&platform), vec![(321, JobSignal::Continue)]);
    assert_eq!(
        session
            .background_job(job_id(1))
            .expect("the continued job stays addressable")
            .state(),
        JobState::Background
    );

    release(&controls);
}

#[test]
fn bg_rejects_a_target_that_is_not_stopped() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[331], &[332]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);

    let running = rejected(&mut session, "bg %1", &probe, &platform, clock.as_ref());
    assert!(
        running.contains("bg: job `%1` is not stopped"),
        "a running job cannot be continued:\n{running}"
    );

    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the second job");
    assert_eq!(
        wait_for_notice(&mut session).kind(),
        &JobNoticeKind::Completed
    );
    let completed = rejected(&mut session, "bg %2", &probe, &platform, clock.as_ref());
    assert!(
        completed.contains("bg: job `%2` is not stopped"),
        "a completed job cannot be continued:\n{completed}"
    );

    let unknown = rejected(&mut session, "bg %9", &probe, &platform, clock.as_ref());
    assert!(
        unknown.contains("bg: unknown job `%9`"),
        "an unaddressable identity is named:\n{unknown}"
    );
    assert!(
        delivered(&platform).is_empty(),
        "a rejected bg delivers nothing"
    );

    release(&controls);
}

#[test]
fn bg_without_an_eligible_job_is_an_error() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[341]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);

    let rendered = rejected(&mut session, "bg", &probe, &platform, clock.as_ref());

    assert!(
        rendered.contains("bg: no stopped job to continue"),
        "an absent default target is an error, not a silent success:\n{rendered}"
    );

    release(&controls);
}

#[test]
fn bg_rejects_a_quarantined_record() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[351]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    controls[0]
        .steps
        .send(Err(WaitError::new(
            io::ErrorKind::Interrupted,
            "initial failure",
        )))
        .expect("fail the initial observation");
    assert_eq!(controls[0].wait_entries.recv().expect("cleanup wait"), 2);
    controls[0]
        .steps
        .send(Err(WaitError::new(io::ErrorKind::Other, "cleanup failure")))
        .expect("fail the cleanup wait");
    assert!(matches!(
        wait_for_notice(&mut session).kind(),
        JobNoticeKind::ObservationFailed { .. }
    ));

    let rendered = rejected(&mut session, "bg %1", &probe, &platform, clock.as_ref());

    assert!(
        rendered.contains("bg: job `%1` is quarantined"),
        "an unobservable record cannot be moved:\n{rendered}"
    );
    assert!(delivered(&platform).is_empty());
}

#[test]
fn a_refused_continue_leaves_the_record_stopped_and_its_notice_pending() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[361]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    stop_controlled_job(&mut session, &controls[0], 1);
    platform.refuse_signals("the host refused the group");

    let rendered = rejected(&mut session, "bg %1", &probe, &platform, clock.as_ref());

    assert!(
        rendered.contains("bg: job `%1` could not be signalled"),
        "a refused delivery is reported, not absorbed:\n{rendered}"
    );
    assert!(matches!(
        session
            .background_job(job_id(1))
            .expect("the record survives a refused delivery")
            .state(),
        JobState::Stopped { .. }
    ));
    assert_eq!(
        session
            .next_job_notice()
            .map(|notice| notice.kind().clone()),
        Some(JobNoticeKind::Stopped),
        "a failed continuation must not consume the stopped notice"
    );
    assert_eq!(
        session.live_background_jobs().len(),
        1,
        "a user delivery failure never quarantines the record"
    );

    release(&controls);
}

#[test]
fn kill_defaults_to_terminate_and_delivers_in_source_order() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[371], &[372]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);

    assert_eq!(
        rendered(
            &mut session,
            "kill %2 %1",
            &probe,
            &platform,
            clock.as_ref()
        ),
        ""
    );

    assert_eq!(
        delivered(&platform),
        vec![(372, JobSignal::Terminate), (371, JobSignal::Terminate)],
        "an omitted selector terminates, and targets are processed as written"
    );
    assert_eq!(
        session.live_background_jobs().len(),
        2,
        "delivery is not a terminal observation"
    );

    release(&controls);
}

#[test]
fn kill_delivers_each_named_selector() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[381]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);

    for selector in ["--stop", "--interrupt", "--hangup", "--kill", "--terminate"] {
        assert_eq!(
            rendered(
                &mut session,
                &format!("kill {selector} %1"),
                &probe,
                &platform,
                clock.as_ref(),
            ),
            ""
        );
    }

    assert_eq!(
        delivered(&platform),
        vec![
            (381, JobSignal::Stop),
            (381, JobSignal::Interrupt),
            (381, JobSignal::Hangup),
            (381, JobSignal::Kill),
            (381, JobSignal::Terminate),
        ]
    );

    release(&controls);
}

#[test]
fn kill_continues_a_stopped_target_before_a_terminating_signal() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[391]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    stop_controlled_job(&mut session, &controls[0], 1);

    assert_eq!(
        rendered(
            &mut session,
            "kill --hangup %1",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );

    assert_eq!(
        delivered(&platform),
        vec![(391, JobSignal::Continue), (391, JobSignal::Hangup)],
        "a stopped process cannot act on a hang-up until it is continued"
    );
    assert!(
        matches!(
            session
                .background_job(job_id(1))
                .expect("the record is unchanged")
                .state(),
            JobState::Stopped { .. }
        ),
        "only the observer may publish what the signal did"
    );

    release(&controls);
}

#[test]
fn kill_kills_a_stopped_target_without_a_preliminary_continue() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[401]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    stop_controlled_job(&mut session, &controls[0], 1);

    assert_eq!(
        rendered(
            &mut session,
            "kill --kill %1",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );

    assert_eq!(
        delivered(&platform),
        vec![(401, JobSignal::Kill)],
        "an unconditional kill needs no continuation"
    );

    release(&controls);
}

#[test]
fn kill_continue_moves_a_stopped_job_to_background() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[411]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    stop_controlled_job(&mut session, &controls[0], 1);

    assert_eq!(
        rendered(
            &mut session,
            "kill --continue %1",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );

    assert_eq!(delivered(&platform), vec![(411, JobSignal::Continue)]);
    assert_eq!(
        session
            .background_job(job_id(1))
            .expect("the continued job stays addressable")
            .state(),
        JobState::Background,
        "an explicit continuation shares the bg transition"
    );
    assert!(session.next_job_notice().is_none());

    let running = rejected(
        &mut session,
        "kill --continue %1",
        &probe,
        &platform,
        clock.as_ref(),
    );
    assert!(
        running.contains("kill: job `%1` is not stopped"),
        "continuing a running job is refused like bg:\n{running}"
    );

    release(&controls);
}

#[test]
fn kill_aborts_at_the_first_delivery_failure() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[421], &[422]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    platform.refuse_signals("the host refused the group");

    let rendered = rejected(
        &mut session,
        "kill %1 %2",
        &probe,
        &platform,
        clock.as_ref(),
    );

    assert!(
        rendered.contains("kill: job `%1` could not be signalled"),
        "the first failing target ends the operation:\n{rendered}"
    );
    assert!(
        delivered(&platform).is_empty(),
        "a refused delivery records nothing"
    );
    assert_eq!(
        session.live_background_jobs().len(),
        2,
        "a user delivery failure never quarantines a record"
    );

    release(&controls);
}

#[test]
fn kill_signals_a_quarantined_group_without_repairing_it() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[431]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    controls[0]
        .steps
        .send(Err(WaitError::new(
            io::ErrorKind::Interrupted,
            "initial failure",
        )))
        .expect("fail the initial observation");
    assert_eq!(controls[0].wait_entries.recv().expect("cleanup wait"), 2);
    controls[0]
        .steps
        .send(Err(WaitError::new(io::ErrorKind::Other, "cleanup failure")))
        .expect("fail the cleanup wait");
    assert!(matches!(
        wait_for_notice(&mut session).kind(),
        JobNoticeKind::ObservationFailed { .. }
    ));

    assert_eq!(
        rendered(
            &mut session,
            "kill --kill %1",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );

    assert_eq!(
        delivered(&platform),
        vec![(431, JobSignal::Kill)],
        "the retained group is the only honest control identity"
    );
    let listed = rendered(&mut session, "jobs", &probe, &platform, clock.as_ref());
    assert!(
        listed.contains("| quarantined |"),
        "a delivered signal is not a terminal observation:\n{listed}"
    );
}

#[test]
fn wait_without_targets_uses_job_identity_order_and_consumes_completions() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[441], &[442]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(7))))
        .expect("complete the newer job first");
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(5))))
        .expect("complete the older job second");

    assert_eq!(
        rendered(&mut session, "wait", &probe, &platform, clock.as_ref()),
        ""
    );

    assert_eq!(
        session.current_status().and_then(Status::code),
        Some(5),
        "the first unsuccessful aggregate in JobId order wins"
    );
    assert!(session.background_job(job_id(1)).is_none());
    assert!(session.background_job(job_id(2)).is_none());
    assert!(
        session.next_job_notice().is_none(),
        "wait consumes every selected completion notice"
    );
}

#[test]
fn wait_explicit_targets_preserve_source_order() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[451], &[452]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(3))))
        .expect("complete job one");
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(8))))
        .expect("complete job two");

    assert_eq!(
        rendered(
            &mut session,
            "wait %2 %1",
            &probe,
            &platform,
            clock.as_ref()
        ),
        ""
    );

    assert_eq!(
        session.current_status().and_then(Status::code),
        Some(8),
        "the first failure in written target order wins"
    );
}

#[test]
fn wait_includes_a_completed_but_unacknowledged_record() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[461]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(6))))
        .expect("complete the job");
    assert_eq!(
        wait_for_notice(&mut session).kind(),
        &JobNoticeKind::Completed,
        "the completion is visible but deliberately unacknowledged"
    );

    assert_eq!(
        rendered(&mut session, "wait %1", &probe, &platform, clock.as_ref()),
        ""
    );

    assert_eq!(session.current_status().and_then(Status::code), Some(6));
    assert!(session.background_job(job_id(1)).is_none());
    assert!(
        session.next_job_notice().is_none(),
        "the pending completion cannot render again after wait"
    );
}

#[test]
fn wait_with_an_empty_snapshot_returns_zero_immediately() {
    let clock = Arc::new(FakeClock::at(100));
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());

    assert_eq!(
        rendered(
            &mut session,
            "wait",
            &Probe::default(),
            &terminal_platform(),
            clock.as_ref(),
        ),
        ""
    );

    let status = session
        .current_status()
        .expect("empty wait records a status");
    assert_eq!(status.code(), Some(0));
    assert_eq!(status.duration(), Duration::ZERO);
}

#[test]
fn wait_resumes_a_stopped_target_in_background_and_drops_its_stale_notice() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[471]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    stop_controlled_job(&mut session, &controls[0], 1);
    platform.complete_on_continue(471, &controls[0], ProcessStatus::Exited(0));

    let mut sink = Vec::new();
    let outcome = session.submit(
        "<interactive>",
        "wait %1",
        &probe,
        &platform,
        clock.as_ref(),
        &mut sink,
    );
    if outcome.is_err() {
        release(&controls);
    }
    outcome.expect("wait should resume and complete the selected job");

    assert_eq!(delivered(&platform), vec![(471, JobSignal::Continue)]);
    assert_eq!(session.current_status().and_then(Status::code), Some(0));
    assert!(session.background_job(job_id(1)).is_none());
    assert!(
        session.next_job_notice().is_none(),
        "the selected stopped and completed notices are stale after wait"
    );
}

#[test]
fn wait_rejects_a_seventeenth_stop_without_consuming_the_stopped_job() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[476]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    stop_controlled_job(&mut session, &controls[0], 1);

    for signal in 20..=35 {
        platform.steps_on_continue(
            476,
            &controls[0],
            vec![
                ProcessTransition::Continued,
                ProcessTransition::Stopped { signal },
            ],
        );
    }

    let diagnostic = rejected(&mut session, "wait %1", &probe, &platform, clock.as_ref());

    assert!(
        diagnostic.contains("wait: job `%1` stopped repeatedly (latest signal 35)"),
        "the repeated-stop diagnostic retains the latest raw signal:\n{diagnostic}"
    );
    assert_eq!(
        delivered(&platform),
        vec![(476, JobSignal::Continue); 16],
        "the seventeenth stop does not trigger another continuation"
    );
    let job = session
        .background_job(job_id(1))
        .expect("the selected job remains addressable");
    assert!(matches!(job.state(), JobState::Stopped { .. }));
    assert_eq!(
        job.members().next(),
        Some((
            ProcessId::new(476).expect("test process is nonzero"),
            &JobMemberState::Stopped { signal: 35 },
        ))
    );
    assert_eq!(
        session
            .next_job_notice()
            .map(|notice| notice.kind().clone()),
        Some(JobNoticeKind::Stopped),
        "the latest stopped notice remains pending"
    );

    release(&controls);
}

#[test]
fn wait_accepts_a_completion_that_races_command_entry() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[481]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(9))))
        .expect("race completion against wait entry");

    assert_eq!(
        rendered(&mut session, "wait %1", &probe, &platform, clock.as_ref()),
        ""
    );

    assert_eq!(session.current_status().and_then(Status::code), Some(9));
    assert!(session.background_job(job_id(1)).is_none());
}

#[test]
fn wait_returns_the_first_unsuccessful_selected_status() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[491], &[492], &[493]]);
    let probe = Probe::new(["/bin/tool"]);
    for _ in 0..3 {
        launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    }
    for control in &controls {
        assert_eq!(control.wait_entries.recv().expect("initial wait"), 1);
    }
    for (control, code) in controls.iter().zip([0, 4, 7]) {
        control
            .steps
            .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(
                code,
            ))))
            .expect("complete the selected job");
    }

    assert_eq!(
        rendered(
            &mut session,
            "wait %1 %2 %3",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );

    assert_eq!(session.current_status().and_then(Status::code), Some(4));
}

#[test]
fn wait_returns_the_last_status_when_every_selected_job_succeeds() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[501], &[502]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    clock.advance(10);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete job one");
    while session.background_completion(job_id(1)).is_none() {
        session.refresh_background_jobs();
        std::thread::yield_now();
    }
    clock.advance(15);
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete job two");

    assert_eq!(
        rendered(
            &mut session,
            "wait %1 %2",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );

    let status = session
        .current_status()
        .expect("wait records its selection");
    assert_eq!(status.code(), Some(0));
    assert_eq!(
        status.duration(),
        Duration::from_nanos(25),
        "the exact last selected aggregate is returned"
    );
}

#[test]
fn wait_rejects_a_quarantined_target_without_signalling_it() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[511]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    controls[0]
        .steps
        .send(Err(WaitError::new(
            io::ErrorKind::Interrupted,
            "initial failure",
        )))
        .expect("fail the initial observation");
    assert_eq!(controls[0].wait_entries.recv().expect("cleanup wait"), 2);
    controls[0]
        .steps
        .send(Err(WaitError::new(io::ErrorKind::Other, "cleanup failure")))
        .expect("fail the cleanup wait");
    assert!(matches!(
        wait_for_notice(&mut session).kind(),
        JobNoticeKind::ObservationFailed { .. }
    ));

    let rendered = rejected(&mut session, "wait %1", &probe, &platform, clock.as_ref());

    assert!(
        rendered.contains("wait: job `%1` is quarantined"),
        "wait cannot block on an abandoned observer:\n{rendered}"
    );
    assert!(delivered(&platform).is_empty());
    assert!(session.background_job(job_id(1)).is_some());
}

#[test]
fn wait_applies_unselected_events_without_consuming_their_notices() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[521], &[522]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(3))))
        .expect("complete the unselected job");
    while session.background_completion(job_id(1)).is_none() {
        session.refresh_background_jobs();
        std::thread::yield_now();
    }
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the selected job");

    assert_eq!(
        rendered(&mut session, "wait %2", &probe, &platform, clock.as_ref()),
        ""
    );

    assert!(session.background_job(job_id(2)).is_none());
    assert_eq!(
        session
            .background_completion(job_id(1))
            .and_then(Status::code),
        Some(3),
        "the unrelated observation is still applied"
    );
    let notice = session
        .next_job_notice()
        .expect("the unrelated completion notice remains queued");
    assert_eq!(notice.job(), job_id(1));
    assert_eq!(notice.kind(), &JobNoticeKind::Completed);
    session
        .acknowledge_job_notice(notice.id())
        .expect("clean up the unselected record");
}

#[test]
fn an_exact_foreground_external_pipeline_is_observed_and_consumed_without_a_notice() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[601, 602]);
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    for control in &controls {
        control
            .steps
            .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
            .expect("complete the managed foreground member");
    }

    assert_eq!(
        rendered(
            &mut session,
            "^tool | ^other",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );

    for control in &controls {
        assert_eq!(
            control.wait_entries.recv().expect("observer-owned wait"),
            1,
            "every member is transferred to a prepared observer"
        );
    }
    let status = session
        .current_status()
        .expect("foreground completion records its aggregate");
    assert_eq!(status.code(), Some(0));
    assert_eq!(status.stages().len(), 2);
    assert!(session.background_job(job_id(1)).is_none());
    assert!(
        session.next_job_notice().is_none(),
        "ordinary foreground completion is consumed without a Done notice"
    );
    assert_eq!(platform.foreground_handovers(), vec![601]);
    assert_eq!(platform.foreground_releases(), 1);
}

#[test]
fn an_aggregate_foreground_stop_retains_one_complete_addressable_job() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[611, 612]);
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    assert_eq!(
        rendered(&mut session, "jobs", &probe, &platform, clock.as_ref()),
        ""
    );
    let status_before_stop = session.current_status().cloned();
    for control in &controls {
        control
            .steps
            .send(Ok(ProcessTransition::Stopped { signal: 19 }))
            .expect("stop the managed foreground member");
    }

    assert_eq!(
        rendered(
            &mut session,
            "^tool | ^other",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );

    for control in &controls {
        assert_eq!(control.wait_entries.recv().expect("observer-owned stop"), 1);
    }
    assert_eq!(
        session.current_status(),
        status_before_stop.as_ref(),
        "a nonterminal stop must not overwrite the last completed status"
    );
    let job = session
        .background_job(job_id(1))
        .expect("the stopped foreground job remains addressable");
    assert!(matches!(job.state(), JobState::Stopped { .. }));
    assert_eq!(
        job.placement(),
        Some(flash_runtime::job::JobPlacement::Foreground)
    );
    assert_eq!(
        job.members().count(),
        2,
        "the complete pipeline is published"
    );
    let stopped = session
        .next_job_notice()
        .expect("the aggregate stop queues one notice");
    assert_eq!(stopped.job(), job_id(1));
    assert_eq!(stopped.kind(), &JobNoticeKind::Stopped);
    assert_eq!(platform.foreground_handovers(), vec![611]);
    assert_eq!(platform.foreground_releases(), 1);

    for control in &controls {
        control
            .steps
            .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
            .expect("release the retained observer");
    }
    assert_eq!(
        rendered(&mut session, "wait %1", &probe, &platform, clock.as_ref()),
        ""
    );
    assert!(session.background_job(job_id(1)).is_none());
}

#[test]
fn a_nested_foreground_stop_discards_pending_state_and_retains_the_job() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[613]);
    let probe = Probe::new(["/bin/tool"]);

    assert_eq!(
        rendered(
            &mut session,
            "def stop_nested() {\n    cd /pending-stop\n    ^tool\n}",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("stop the nested managed foreground member");

    assert_eq!(
        rendered(
            &mut session,
            "stop_nested()",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );
    assert_eq!(
        controls[0]
            .wait_entries
            .recv()
            .expect("observer-owned nested stop"),
        1
    );
    assert_eq!(
        session.cwd(),
        Path::new("/work"),
        "the stopped call must discard its pending cwd mutation"
    );
    assert!(
        session.current_status().is_none(),
        "the stopped call must not publish a pending status"
    );
    let job = session
        .background_job(job_id(1))
        .expect("the nested stopped foreground job remains addressable");
    assert!(matches!(job.state(), JobState::Stopped { .. }));
    assert_eq!(
        job.placement(),
        Some(flash_runtime::job::JobPlacement::Foreground)
    );
    let stopped = session
        .next_job_notice()
        .expect("the nested stop queues one notice");
    assert_eq!(stopped.job(), job_id(1));
    assert_eq!(stopped.kind(), &JobNoticeKind::Stopped);

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("release the retained nested observer");
    assert_eq!(
        rendered(&mut session, "wait %1", &probe, &platform, clock.as_ref()),
        ""
    );
    assert!(session.background_job(job_id(1)).is_none());
}

#[test]
fn a_foreground_handoff_failure_cleans_every_started_member_without_publication() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[621, 622]]);
    platform.refuse_foreground("the test terminal refused ownership");
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    for control in &controls {
        control
            .steps
            .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
            .expect("let cleanup reap the started member");
    }
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "^tool | ^other",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a real terminal handoff refusal is fatal");

    assert!(
        error
            .render()
            .contains("terminal handover to the job failed"),
        "{}",
        error.render()
    );
    assert_eq!(platform.foreground_attempts(), 1);
    assert!(platform.foreground_handovers().is_empty());
    assert_eq!(platform.foreground_releases(), 0);
    for control in &controls {
        assert_eq!(
            control.terminate_calls.load(Ordering::SeqCst),
            1,
            "every started child is terminated before its final wait"
        );
    }
    assert!(session.background_job(job_id(1)).is_none());
    assert!(session.next_job_notice().is_none());
    assert!(session.current_status().is_none());
    assert!(sink.is_empty());
}

#[test]
fn a_managed_foreground_wait_preserves_an_unrelated_background_completion() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[631], &[632]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(
        controls[0]
            .wait_entries
            .recv()
            .expect("background observer wait"),
        1
    );
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(3))))
        .expect("complete the unrelated background job");
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the managed foreground job");

    assert_eq!(
        rendered(&mut session, "^tool", &probe, &platform, clock.as_ref()),
        ""
    );

    assert_eq!(
        controls[1]
            .wait_entries
            .recv()
            .expect("foreground observer wait"),
        1
    );
    assert!(session.background_job(job_id(2)).is_none());
    let notice = wait_for_notice(&mut session);
    assert_eq!(
        session
            .background_completion(job_id(1))
            .and_then(Status::code),
        Some(3),
        "the unrelated event is eventually applied without a cross-worker ordering assumption"
    );
    assert_eq!(notice.job(), job_id(1));
    assert_eq!(notice.kind(), &JobNoticeKind::Completed);
    session
        .acknowledge_job_notice(notice.id())
        .expect("clean up the unrelated completion");
}

#[test]
fn an_observed_external_continuation_clears_stopped_state_and_notice() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[641]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("initial wait"), 1);

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("stop the background job");
    assert_eq!(controls[0].wait_entries.recv().expect("continued wait"), 2);
    assert_eq!(
        wait_for_notice(&mut session).kind(),
        &JobNoticeKind::Stopped
    );

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Continued))
        .expect("report an external continuation");
    assert_eq!(controls[0].wait_entries.recv().expect("completion wait"), 3);
    for _ in 0..10_000 {
        session.refresh_background_jobs();
        if session
            .background_job(job_id(1))
            .is_some_and(|job| job.state() == JobState::Background)
        {
            break;
        }
        std::thread::yield_now();
    }

    assert_eq!(
        session
            .background_job(job_id(1))
            .expect("the externally continued job remains addressable")
            .state(),
        JobState::Background
    );
    assert!(
        session.next_job_notice().is_none(),
        "the continuation removes the stale stopped notice without adding one"
    );

    release(&controls);
}

#[test]
fn a_queued_stop_continuation_and_completion_leave_only_the_completion_notice() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[651]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("initial wait"), 1);

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("queue the stop");
    assert_eq!(controls[0].wait_entries.recv().expect("continued wait"), 2);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Continued))
        .expect("queue the continuation");
    assert_eq!(controls[0].wait_entries.recv().expect("completion wait"), 3);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(4))))
        .expect("queue the completion");

    for _ in 0..10_000 {
        session.refresh_background_jobs();
        if session.background_completion(job_id(1)).is_some() {
            break;
        }
        std::thread::yield_now();
    }

    assert_eq!(
        session
            .background_completion(job_id(1))
            .and_then(Status::code),
        Some(4)
    );
    let completion = session
        .next_job_notice()
        .expect("the terminal transition leaves one notice");
    assert_eq!(completion.job(), job_id(1));
    assert_eq!(completion.kind(), &JobNoticeKind::Completed);
    session
        .acknowledge_job_notice(completion.id())
        .expect("remove the completed record");
    assert!(session.next_job_notice().is_none());
}

#[test]
fn fg_moves_an_explicit_running_background_job_without_continuing_it() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[661]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("initial wait"), 1);
    platform.step_on_foreground(
        661,
        &controls[0],
        ProcessTransition::Completed(ProcessStatus::Exited(7)),
    );

    assert_eq!(
        rendered(&mut session, "fg %1", &probe, &platform, clock.as_ref()),
        ""
    );

    assert_eq!(session.current_status().and_then(Status::code), Some(7));
    assert!(session.background_job(job_id(1)).is_none());
    assert!(session.next_job_notice().is_none());
    assert!(delivered(&platform).is_empty());
    assert_eq!(platform.foreground_handovers(), vec![661]);
    assert_eq!(platform.foreground_releases(), 1);
    // This contract transfers group ownership only. Per-job terminal-mode
    // snapshots and restoration remain explicitly unclaimed.
}

#[test]
fn fg_defaults_to_the_newest_eligible_job_and_hands_off_before_continue() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups(&[&[671], &[672]]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("first wait"), 1);
    assert_eq!(controls[1].wait_entries.recv().expect("second wait"), 1);
    stop_controlled_job(&mut session, &controls[1], 1);
    platform.complete_on_continue(672, &controls[1], ProcessStatus::Exited(0));

    assert_eq!(
        rendered(&mut session, "fg", &probe, &platform, clock.as_ref()),
        ""
    );

    assert!(session.background_job(job_id(1)).is_some());
    assert!(session.background_job(job_id(2)).is_none());
    assert_eq!(
        platform.foreground_operations(),
        vec![
            ForegroundOperation::Handover(672),
            ForegroundOperation::Signal(672, JobSignal::Continue),
            ForegroundOperation::Restore,
        ],
        "the stopped group receives the terminal before its continuation"
    );
    assert!(session.next_job_notice().is_none());

    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the unrelated job");
    assert_eq!(
        rendered(&mut session, "wait %1", &probe, &platform, clock.as_ref()),
        ""
    );
}

#[test]
fn fg_restop_retains_one_foreground_record_and_does_not_overwrite_status() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[681]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("initial wait"), 1);
    stop_controlled_job(&mut session, &controls[0], 1);
    let status_before_fg = session.current_status().cloned();
    platform.step_on_continue(681, &controls[0], ProcessTransition::Stopped { signal: 20 });

    assert_eq!(
        rendered(&mut session, "fg %1", &probe, &platform, clock.as_ref()),
        ""
    );

    assert_eq!(controls[0].wait_entries.recv().expect("second stop"), 3);
    assert_eq!(session.current_status(), status_before_fg.as_ref());
    let job = session
        .background_job(job_id(1))
        .expect("the re-stopped job remains addressable");
    assert!(matches!(job.state(), JobState::Stopped { .. }));
    assert_eq!(job.placement(), Some(JobPlacement::Foreground));
    let notice = session
        .next_job_notice()
        .expect("the second stop leaves one pending notice");
    assert_eq!(notice.job(), job_id(1));
    assert_eq!(notice.kind(), &JobNoticeKind::Stopped);
    session
        .acknowledge_job_notice(notice.id())
        .expect("acknowledge the re-stop");
    assert!(session.next_job_notice().is_none());

    platform.complete_on_continue(681, &controls[0], ProcessStatus::Exited(0));
    assert_eq!(
        rendered(&mut session, "wait %1", &probe, &platform, clock.as_ref()),
        ""
    );
}

#[test]
fn fg_reports_terminal_restoration_failure_after_consuming_completion() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[691]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("initial wait"), 1);
    let status_before_fg = session.current_status().cloned();
    platform.step_on_foreground(
        691,
        &controls[0],
        ProcessTransition::Completed(ProcessStatus::Exited(4)),
    );
    platform.refuse_foreground_restore("the shell terminal could not be restored");

    let rendered = rejected(&mut session, "fg %1", &probe, &platform, clock.as_ref());

    assert!(
        rendered.contains("the shell terminal could not be restored"),
        "the restoration failure takes precedence over the completed result:\n{rendered}"
    );
    assert_eq!(session.current_status(), status_before_fg.as_ref());
    assert!(session.background_job(job_id(1)).is_none());
    assert!(session.next_job_notice().is_none());
    assert_eq!(platform.foreground_releases(), 1);
}

#[test]
fn fg_rejects_a_completed_record_without_consuming_its_notice() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[701]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("initial wait"), 1);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the job");
    let completion = wait_for_notice(&mut session);
    assert_eq!(completion.kind(), &JobNoticeKind::Completed);

    let rendered = rejected(&mut session, "fg %1", &probe, &platform, clock.as_ref());

    assert!(
        rendered.contains("fg: job `%1` has completed"),
        "{rendered}"
    );
    assert_eq!(
        session.next_job_notice().map(|notice| notice.id()),
        Some(completion.id())
    );
    session
        .acknowledge_job_notice(completion.id())
        .expect("clean up the completed record");
}

#[test]
fn fg_rejects_a_quarantined_record_without_handing_off_the_terminal() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[711]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("initial wait"), 1);
    controls[0]
        .steps
        .send(Err(WaitError::new(
            io::ErrorKind::Interrupted,
            "initial failure",
        )))
        .expect("fail the initial observation");
    assert_eq!(controls[0].wait_entries.recv().expect("cleanup wait"), 2);
    controls[0]
        .steps
        .send(Err(WaitError::new(io::ErrorKind::Other, "cleanup failure")))
        .expect("fail cleanup");
    assert!(matches!(
        wait_for_notice(&mut session).kind(),
        JobNoticeKind::ObservationFailed { .. }
    ));

    let rendered = rejected(&mut session, "fg %1", &probe, &platform, clock.as_ref());

    assert!(
        rendered.contains("fg: job `%1` is quarantined"),
        "{rendered}"
    );
    assert!(platform.foreground_handovers().is_empty());
    assert!(session.background_job(job_id(1)).is_some());
}

#[test]
fn fg_requires_a_real_foreground_terminal() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::new(&[721]);
    let probe = Probe::new(["/bin/tool"]);
    launch_controlled_job(&mut session, &clock, &platform, "^tool &", &probe);
    assert_eq!(controls[0].wait_entries.recv().expect("initial wait"), 1);
    let no_terminal = FakePlatform::full();

    let rendered = rejected(&mut session, "fg %1", &probe, &no_terminal, clock.as_ref());

    assert!(
        rendered.contains("fg: a foreground terminal is required"),
        "{rendered}"
    );
    let job = session
        .background_job(job_id(1))
        .expect("a refused foreground move keeps the job");
    assert_eq!(job.placement(), Some(JobPlacement::Background));
    assert!(platform.foreground_handovers().is_empty());
    release(&controls);
}

#[test]
fn a_conditional_chain_keeps_the_legacy_resume_in_place_path() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) =
        ControlledBackgroundPlatform::in_groups_for_session(&[&[641], &[642]]);
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("stop the first conditional term");
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the resumed first term");
    controls[1]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the reached second term");

    assert_eq!(
        rendered(
            &mut session,
            "^tool && ^other",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        ""
    );

    assert_eq!(delivered(&platform), vec![(641, JobSignal::Continue)]);
    assert_eq!(platform.foreground_handovers(), vec![641, 642]);
    assert_eq!(platform.foreground_releases(), 2);
    assert!(session.background_job(job_id(1)).is_none());
    assert!(session.next_job_notice().is_none());
    assert_eq!(session.current_status().and_then(Status::code), Some(0));
}

#[test]
fn a_source_ordered_foreground_wait_cleans_up_after_the_seventeenth_stop() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[646]]);
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    for signal in 60..=76 {
        controls[0]
            .steps
            .send(Ok(ProcessTransition::Stopped { signal }))
            .expect("queue the next stop");
        if signal < 76 {
            controls[0]
                .steps
                .send(Ok(ProcessTransition::Continued))
                .expect("queue the observed continuation");
        }
    }
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let failure cleanup reap the child");
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "^tool && ^other",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("the source-ordered wait must reject the seventeenth stop");

    assert!(
        error
            .render()
            .contains("the job stopped repeatedly (latest signal 76)"),
        "{}",
        error.render()
    );
    assert_eq!(delivered(&platform), vec![(646, JobSignal::Continue); 16]);
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().count(),
        34,
        "the bounded wait terminates and performs one final cleanup wait"
    );
    assert!(sink.is_empty());
}

#[test]
fn a_mixed_foreground_pipeline_keeps_the_legacy_session_executor() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[651]]);
    let probe = Probe::new(["/bin/tool"]);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("complete the mixed external member");

    assert_eq!(
        rendered(
            &mut session,
            "^tool | decode bytes | length",
            &probe,
            &platform,
            clock.as_ref(),
        ),
        "0\n"
    );

    assert_eq!(platform.foreground_handovers(), vec![651]);
    assert_eq!(platform.foreground_releases(), 1);
    assert!(session.background_job(job_id(1)).is_none());
    assert!(session.next_job_notice().is_none());
    let status = session.current_status().expect("mixed status is retained");
    assert_eq!(status.code(), Some(0));
    assert_eq!(status.stages().len(), 3);
}

#[test]
fn a_mixed_foreground_wait_cleans_up_after_the_seventeenth_stop() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[656]]);
    let probe = Probe::new(["/bin/tool"]);
    for signal in 40..=56 {
        controls[0]
            .steps
            .send(Ok(ProcessTransition::Stopped { signal }))
            .expect("queue the next stop");
        if signal < 56 {
            controls[0]
                .steps
                .send(Ok(ProcessTransition::Continued))
                .expect("queue the observed continuation");
        }
    }
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let failure cleanup reap the child");
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "^tool | decode bytes | length",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("the mixed wait must reject the seventeenth stop");

    assert!(
        error
            .render()
            .contains("the job stopped repeatedly (latest signal 56)"),
        "{}",
        error.render()
    );
    assert_eq!(delivered(&platform), vec![(656, JobSignal::Continue); 16]);
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().count(),
        34,
        "the bounded wait terminates and performs one final cleanup wait"
    );
    assert_eq!(sink, b"0\n");
}

#[test]
fn a_mixed_foreground_handoff_failure_terminates_and_reaps_every_child() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[657]]);
    platform.refuse_foreground("the test terminal refused ownership");
    let probe = Probe::new(["/bin/tool"]);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let failure cleanup reap the mixed child");
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "^tool | decode bytes | length",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a real terminal handoff refusal is fatal");

    assert!(
        error
            .render()
            .contains("terminal handover to the job failed"),
        "{}",
        error.render()
    );
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().collect::<Vec<_>>(),
        vec![1],
        "the started child receives one final cleanup wait"
    );
    assert_eq!(platform.foreground_attempts(), 1);
    assert_eq!(platform.foreground_releases(), 0);
    assert!(session.current_status().is_none());
    assert!(sink.is_empty());
}

#[test]
fn a_mixed_wait_failure_terminates_and_reaps_the_failed_child() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[658]]);
    let probe = Probe::new(["/bin/tool"]);
    controls[0]
        .steps
        .send(Err(WaitError::new(
            io::ErrorKind::Other,
            "the injected transition wait failed",
        )))
        .expect("inject the source-ordered wait failure");
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let failure cleanup reap the mixed child");
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "^tool | decode bytes | length",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a failed external wait is fatal");

    assert!(
        error
            .render()
            .contains("the injected transition wait failed"),
        "{}",
        error.render()
    );
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().collect::<Vec<_>>(),
        vec![1, 2],
        "the failed transition wait is followed by one final cleanup wait"
    );
    assert_eq!(platform.foreground_releases(), 1);
    assert!(session.current_status().is_none());
    assert_eq!(sink, b"0\n");
}

#[test]
fn a_mixed_pipe_read_failure_cancels_children_and_releases_endpoints() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[659]]);
    platform.fail_pipe_read(0);
    let probe = Probe::new(["/bin/tool"]);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let cancellation reap the mixed child");
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "^tool | decode bytes | length",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("the injected segment read must fail");

    assert!(
        error
            .render()
            .contains("the injected mixed-pipeline read failed"),
        "{}",
        error.render()
    );
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(platform.active_endpoints(), 0);
    assert_eq!(platform.foreground_releases(), 1);
    assert!(session.current_status().is_none());
    assert!(sink.is_empty());
}

#[test]
fn a_mixed_pipe_write_failure_cancels_children_and_releases_endpoints() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[660]]);
    platform.fail_pipe_write(0);
    let probe = Probe::new(["/bin/tool"]);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let cancellation reap the mixed child");
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "which pwd | get kind | encode utf8 | ^tool",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("the injected segment write must fail");

    assert!(
        error
            .render()
            .contains("the injected mixed-pipeline write failed"),
        "{}",
        error.render()
    );
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(platform.active_endpoints(), 0);
    assert_eq!(platform.foreground_releases(), 1);
    assert!(session.current_status().is_none());
    assert!(sink.is_empty());
}

#[test]
fn simultaneous_mixed_failures_select_the_earliest_source_stage() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[665, 666]]);
    platform.fail_pipe_write(0);
    platform.fail_pipe_write(2);
    platform.feed_pipe_read(1, b"later segment bytes".to_vec());
    platform.synchronize_pipe_write_failures(2);
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    for control in &controls {
        control
            .steps
            .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
            .expect("let deterministic cancellation reap every mixed child");
    }
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "which pwd | get kind | encode utf8 | ^tool | \
             decode bytes | encode bytes | ^other",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("both segment drains have injected genuine failures");

    assert!(
        error
            .render()
            .contains("the injected mixed-pipeline write failed at pipe 0"),
        "the earliest source failure must win:\n{}",
        error.render()
    );
    for control in &controls {
        assert_eq!(control.terminate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(control.wait_entries.try_iter().collect::<Vec<_>>(), vec![1]);
    }
    assert_eq!(platform.active_endpoints(), 0);
    assert_eq!(platform.foreground_releases(), 1);
    assert!(session.current_status().is_none());
    assert!(sink.is_empty());
}

#[test]
fn a_mixed_output_failure_cancels_children_before_returning() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[661, 662]]);
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    for control in &controls {
        control
            .steps
            .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
            .expect("let output-failure cancellation reap every mixed child");
    }
    let mut sink = FailingOutput;

    let error = session
        .submit(
            "<interactive>",
            "^tool | decode bytes | encode bytes | ^other | decode bytes | length",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("the injected output failure is fatal");

    let flash_runtime::session::SubmitError::Output(error) = error else {
        panic!("the injected sink failure must retain its output-error channel");
    };
    assert!(
        error
            .to_string()
            .contains("the injected session output failed")
    );
    for control in &controls {
        assert_eq!(control.terminate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(control.wait_entries.try_iter().collect::<Vec<_>>(), vec![1]);
    }
    assert_eq!(platform.active_endpoints(), 0);
    assert_eq!(platform.foreground_releases(), 1);
    assert!(session.current_status().is_none());
}

#[test]
fn a_mixed_spawn_failure_cleans_every_earlier_child_and_endpoint() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[663, 664]]);
    platform.fail_spawn(1);
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let spawn-failure cleanup reap the earlier child");
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "^tool | decode bytes | encode bytes | ^other",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("the injected later spawn failure is fatal");

    assert!(
        error
            .render()
            .contains("the injected mixed-pipeline spawn failed"),
        "{}",
        error.render()
    );
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(controls[1].terminate_calls.load(Ordering::SeqCst), 0);
    assert!(controls[1].wait_entries.try_iter().next().is_none());
    assert_eq!(platform.active_endpoints(), 0);
    assert_eq!(platform.foreground_attempts(), 0);
    assert!(session.current_status().is_none());
    assert!(sink.is_empty());
}

#[test]
fn a_state_changing_job_command_must_be_the_only_pipeline_stage() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    session.enable_interactive_job_control(clock.clone());
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "bg | bg",
            &Probe::default(),
            &terminal_platform(),
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a process-effect command cannot be a pipeline member");

    let rendered = error.render();
    assert!(
        rendered.contains("`bg` must be the only stage of its pipeline"),
        "the refusal names the command:\n{rendered}"
    );
    assert!(sink.is_empty());
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
fn a_terminal_record_stream_renders_as_one_table() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd missing",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the record stream should be presentable");

    assert_eq!(
        String::from_utf8(sink).unwrap(),
        "name    | kind     | target | path\n--------+----------+--------+-----\npwd     | internal | null   | null\nmissing | missing  | null   | null\n"
    );
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
fn a_carrier_valid_multi_island_plan_executes_in_source_order() {
    let temp = TempDir::new("session-multi-island-boundary");
    fs::write(temp.path().join("input.bin"), b"arbitrary topology")
        .expect("binary fixture should be written");

    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::new(["/bin/cat"]);
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "^/bin/cat < input.bin | decode bytes | encode bytes | \
             ^/bin/cat | decode bytes | encode bytes | \
             ^/bin/cat > output.bin",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("every carrier-valid internal segment should execute");

    assert!(sink.is_empty());
    assert_eq!(
        fs::read(temp.path().join("output.bin")).unwrap(),
        b"arbitrary topology"
    );
    assert_eq!(
        session
            .current_status()
            .expect("the aggregate status should commit")
            .stages()
            .len(),
        7
    );
}

#[test]
fn multi_island_interactive_and_script_execution_are_identical() {
    let temp = TempDir::new("multi-island-frontend-parity");
    fs::write(temp.path().join("input.bin"), b"frontend parity")
        .expect("binary fixture should be written");
    let source = "^/bin/cat < input.bin | decode bytes | encode bytes | \
                  ^/bin/cat | decode bytes | encode bytes";
    let probe = Probe::new(["/bin/cat"]);

    let mut interactive = Session::new(temp.path(), environment(), SessionOptions::default());
    let mut interactive_output = Vec::new();
    interactive
        .submit(
            "<interactive>",
            source,
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut interactive_output,
        )
        .expect("the interactive frontend should execute every segment");

    let mut script_environment = environment();
    let mut script_output = Vec::new();
    let script = execute_script(
        "parity.fsh",
        source,
        temp.path(),
        &mut script_environment,
        &standard_registry(),
        &probe,
        &SessionOptions::default(),
        &PosixPlatform,
        Arc::new(FakeClock::new()),
        &mut script_output,
    )
    .expect("the script frontend should execute the same segments");

    assert_eq!(interactive_output, b"frontend parity");
    assert_eq!(script_output, interactive_output);
    let interactive_status = interactive.current_status().expect("status should commit");
    let script_status = script.status().expect("script status should commit");
    assert_eq!(script_status, interactive_status);
}

#[test]
fn mixed_segment_preparation_is_source_ordered_while_eager_drains_overlap() {
    let temp = TempDir::new("session-mixed-preparation");
    let nested = temp.path().join("nested");
    fs::create_dir(&nested).expect("nested working directory should be created");
    fs::write(temp.path().join("input.bin"), b"prepare in order")
        .expect("binary fixture should be written");

    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::new(["/bin/cat"]);
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "open input.bin | ^/bin/cat | save first.bin | cd nested | pwd | to text | \
             ^/bin/cat | save pwd.bin | pwd | to text | ^/bin/cat > final.txt",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("eager segment preparation should not block later drains");

    let expected = format!(
        "{}\n",
        fs::canonicalize(&nested)
            .expect("nested working directory should canonicalize")
            .display()
    );
    assert!(sink.is_empty());
    assert_eq!(
        fs::read(temp.path().join("first.bin")).unwrap(),
        b"prepare in order"
    );
    assert_eq!(
        fs::read(temp.path().join("pwd.bin")).unwrap(),
        expected.as_bytes()
    );
    assert_eq!(
        fs::read(temp.path().join("final.txt")).unwrap(),
        expected.as_bytes()
    );
}

#[test]
fn successful_mixed_transactions_commit_state_and_source_ordered_closure_deltas() {
    let temp = TempDir::new("session-mixed-transaction-success");
    let nested = temp.path().join("nested");
    fs::create_dir(&nested).expect("the committed working directory should be created");
    fs::write(temp.path().join("input.txt"), b"transaction\n")
        .expect("the transaction fixture should be written");

    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::new(["/bin/cat"]);
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            format!(
                "def early(row) {{\n\
                     export WINNER = 'early'\n\
                     export LOCAL = 'visible'\n\
                     return $row\n\
                 }}\n\
                 def observe(row) {{\n\
                     export WITHIN_SEGMENT = 'visible'\n\
                     return $row\n\
                 }}\n\
                 def late(row) {{\n\
                     export WINNER = 'late'\n\
                     return $row\n\
                 }}\n\
                 ^/bin/cat < input.txt | save observed.txt | cd {} | pwd | to text | \
                 ^/bin/cat | from text | each {{|row| early($row)}} | \
                 each {{|row| observe($row)}} | to text | ^/bin/cat | from text | \
                 each {{|row| late($row)}} | to text",
                nested.display(),
            ),
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the complete mixed transaction should commit once");

    let canonical_nested = fs::canonicalize(&nested).expect("the nested directory should exist");
    assert_eq!(session.cwd(), canonical_nested);
    assert_eq!(
        session.environment().get("PWD"),
        Some(canonical_nested.as_os_str())
    );
    assert_eq!(
        session.environment().get("WINNER"),
        Some(OsStr::new("late")),
        "a later segment delta must win deterministically"
    );
    assert_eq!(
        session.environment().get("LOCAL"),
        Some(OsStr::new("visible")),
        "the first lazy closure stage must retain its delta"
    );
    assert_eq!(
        session.environment().get("WITHIN_SEGMENT"),
        Some(OsStr::new("visible")),
        "every lazy closure stage contributes to the shared segment delta"
    );
    assert_eq!(session.current_status().and_then(Status::code), Some(0));
    assert_eq!(
        fs::read(temp.path().join("observed.txt")).unwrap(),
        b"transaction\n"
    );
    assert_eq!(sink, format!("{}\n", canonical_nested.display()).as_bytes());
}

fn stateful_mixed_source(tail: &str) -> String {
    format!(
        "def mark(row) {{\n\
             export LEAK = 'no'\n\
             return $row\n\
         }}\n\
         cd /next | which pwd | each {{|row| mark($row)}} | get kind | \
         encode utf8 | {tail}"
    )
}

fn session_with_baseline_status() -> Session {
    let mut session = session();
    assert_eq!(
        submit(&mut session, "pwd", &Probe::default()),
        SubmitOutcome::Continued
    );
    session
}

fn assert_mixed_state_rolled_back(session: &Session) {
    assert_eq!(session.cwd(), Path::new("/work"));
    assert_eq!(session.environment().get("LEAK"), None);
    assert_eq!(session.environment().get("PWD"), None);
    assert_eq!(session.current_status().and_then(Status::code), Some(0));
}

#[test]
fn mixed_runtime_failure_rolls_back_all_pending_state() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session_with_baseline_status();
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[683]]);
    platform.feed_pipe_read(1, vec![0xff]);
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let runtime-failure cancellation reap the child");
    let probe = Probe::new(["/bin/tool"]);
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            stateful_mixed_source("^tool | decode utf8"),
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a later lazy decode failure should roll back the transaction");

    assert!(error.render().contains("malformed input at byte offset 0"));
    assert_mixed_state_rolled_back(&session);
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(platform.active_endpoints(), 0);
    assert!(sink.is_empty());
}

#[test]
fn deferred_check_failure_rolls_back_state_but_keeps_observed_output() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session_with_baseline_status();
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[684]]);
    platform.feed_pipe_read(1, b"bytes".to_vec());
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(7))))
        .expect("complete the checked external stage unsuccessfully");
    let probe = Probe::new(["/bin/tool"]);
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            stateful_mixed_source("^tool | check | decode bytes | length"),
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a deferred check failure should reject pending state");

    assert!(
        error
            .render()
            .contains("checked command was unsuccessful: exit 7")
    );
    assert_mixed_state_rolled_back(&session);
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        controls[0].wait_entries.try_iter().collect::<Vec<_>>(),
        vec![1]
    );
    assert!(
        !sink.is_empty(),
        "already-rendered output is not transactional"
    );
}

#[test]
fn mixed_output_failure_rolls_back_all_pending_state() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session_with_baseline_status();
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[685]]);
    platform.feed_pipe_read(1, b"output".to_vec());
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let output-failure cancellation reap the child");
    let probe = Probe::new(["/bin/tool"]);
    let mut sink = FailingOutput;

    let error = session
        .submit(
            "<interactive>",
            stateful_mixed_source("^tool | decode bytes | length"),
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a final presentation failure should reject pending state");

    let flash_runtime::session::SubmitError::Output(error) = error else {
        panic!("the injected sink failure must retain its output-error channel");
    };
    assert!(
        error
            .to_string()
            .contains("the injected session output failed")
    );
    assert_mixed_state_rolled_back(&session);
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(platform.active_endpoints(), 0);
}

#[test]
fn mixed_wait_failure_rolls_back_all_pending_state() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session_with_baseline_status();
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[686]]);
    controls[0]
        .steps
        .send(Err(WaitError::new(
            io::ErrorKind::Other,
            "the injected transaction wait failed",
        )))
        .expect("inject the transaction wait failure");
    controls[0]
        .steps
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("let wait-failure cleanup reap the child");
    let probe = Probe::new(["/bin/tool"]);
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            stateful_mixed_source("^tool"),
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect_err("a child wait failure should reject pending state");

    assert!(
        error
            .render()
            .contains("the injected transaction wait failed")
    );
    assert_mixed_state_rolled_back(&session);
    assert_eq!(controls[0].terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        controls[0].wait_entries.try_iter().collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(platform.active_endpoints(), 0);
    assert!(sink.is_empty());
}

#[test]
fn a_middle_island_exit_cancels_blocked_peers_and_commits_prior_state() {
    let (completion, finished) = mpsc::channel();

    std::thread::spawn(move || {
        let temp = TempDir::new("session-mixed-middle-exit");
        let prior = temp.path().join("prior");
        let later = temp.path().join("later");
        fs::create_dir(&prior).expect("the source-prior directory should be created");
        fs::create_dir(&later).expect("the source-later directory should be created");
        fs::write(temp.path().join("large.txt"), b"line\n".repeat(512 * 1024))
            .expect("the source-prior stream should be large enough to apply backpressure");

        let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
        let probe = Probe::new(["/bin/sh", "/bin/cat"]);
        let mut sink = Vec::new();
        let outcome = session
            .submit(
                "<interactive>",
                format!(
                    "def prior_mark(row) {{\n\
                         export PRIOR_SEEN = 'yes'\n\
                         return $row\n\
                     }}\n\
                     def exit_mark(row) {{\n\
                         export EXIT_SEEN = 'yes'\n\
                         return $row\n\
                     }}\n\
                     open large.txt | from text | each {{|row| prior_mark($row)}} | \
                     to text | ^/bin/sh -c 'sleep 1; printf y; exec /bin/sleep 10' | \
                     decode utf8 | first 1 | \
                     each {{|row| exit_mark($row)}} | encode utf8 | save captured.bin | \
                     cd {} | exit 9 | cd {} | pwd | to text | ^/bin/cat",
                    prior.display(),
                    later.display(),
                ),
                &probe,
                &PosixPlatform,
                &FakeClock::new(),
                &mut sink,
            )
            .expect("explicit exit should complete after cancelling blocked peers");

        completion
            .send((
                outcome,
                session.cwd().to_owned(),
                session.environment().get("PRIOR_SEEN").map(OsStr::to_owned),
                session.environment().get("EXIT_SEEN").map(OsStr::to_owned),
                fs::metadata(temp.path().join("captured.bin"))
                    .expect("the eager source-prior save should remain observable")
                    .len(),
                sink,
            ))
            .expect("the test receiver should remain available");
    });

    let (outcome, cwd, prior_seen, exit_seen, captured_len, sink) = finished
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("middle-island exit must cancel external children before joining segment workers");
    assert_eq!(outcome, SubmitOutcome::Exit(9));
    assert_eq!(cwd.file_name(), Some(OsStr::new("prior")));
    assert_eq!(prior_seen.as_deref(), Some(OsStr::new("yes")));
    assert_eq!(exit_seen.as_deref(), Some(OsStr::new("yes")));
    assert!(captured_len > 0);
    assert!(sink.is_empty());
}

#[test]
fn a_middle_island_exit_terminates_and_reaps_every_external_child() {
    let clock = Arc::new(FakeClock::new());
    let mut session = session();
    let (platform, controls) = ControlledBackgroundPlatform::in_groups_for_session(&[&[687, 688]]);
    platform.feed_pipe_read(0, b"exit input".to_vec());
    let probe = Probe::new(["/bin/tool", "/bin/other"]);
    for control in &controls {
        control
            .steps
            .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
            .expect("let explicit-exit cancellation reap every child");
    }
    let mut sink = Vec::new();

    let outcome = session
        .submit(
            "<interactive>",
            "^tool | decode bytes | first 1 | encode bytes | save captured.bin | \
             cd /prior | exit 9 | cd /later | pwd | to text | ^other",
            &probe,
            &platform,
            clock.as_ref(),
            &mut sink,
        )
        .expect("explicit exit should complete after exact child cleanup");

    assert_eq!(outcome, SubmitOutcome::Exit(9));
    assert_eq!(session.cwd(), Path::new("/prior"));
    for control in &controls {
        assert_eq!(control.terminate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(control.wait_entries.try_iter().collect::<Vec<_>>(), vec![1]);
    }
    assert_eq!(platform.active_endpoints(), 0);
    assert_eq!(platform.foreground_releases(), 1);
    assert!(sink.is_empty());
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
fn mixed_pipeline_status_slots_preserve_selection_signals_and_durations() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flash-status-fixture"));
    let temp = TempDir::new("session-mixed-status-slots");
    let source = format!(
        "^{0} exit 7 | decode bytes | encode bytes | \
         ^{0} signal | decode bytes | encode bytes | ^{0} exit 0",
        fixture.display()
    );
    let probe = Probe::new([fixture]);

    for (pipefail, expected_code, expect_signal) in [(false, Some(0), false), (true, None, true)] {
        let mut session = Session::new(
            temp.path(),
            environment(),
            SessionOptions::default().with_pipefail(pipefail),
        );
        session
            .submit(
                "<interactive>",
                &source,
                &probe,
                &PosixPlatform,
                &TickingClock::default(),
                &mut Vec::new(),
            )
            .expect("the complete mixed status vector should aggregate");

        let status = session.current_status().expect("status should commit");
        assert_eq!(status.code(), expected_code);
        assert_eq!(status.signal().is_some(), expect_signal);
        assert_eq!(status.stages().len(), 7);
        assert_eq!(
            status.stages().iter().map(Status::code).collect::<Vec<_>>(),
            vec![Some(7), Some(0), Some(0), None, Some(0), Some(0), Some(0)]
        );
        assert!(status.stages()[3].signal().is_some());
        assert!(status.duration() > Duration::ZERO);
        for index in [0, 3, 6] {
            assert!(status.stages()[index].duration() > Duration::ZERO);
        }
        for index in [1, 2, 4, 5] {
            assert_eq!(status.stages()[index].duration(), Duration::ZERO);
        }
    }
}

#[test]
fn deferred_check_forwards_successful_external_bytes() {
    let temp = TempDir::new("session-deferred-check-success");
    let probe = Probe::new(["/bin/echo"]);
    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "^/bin/echo -n xxxxxxxxxxxxxxxxx | check | decode bytes | encode bytes",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("a successful deferred check should forward without capture");

    assert_eq!(sink, vec![b'x'; 17]);
    assert_eq!(
        session
            .current_status()
            .expect("aggregate status should commit")
            .stages()
            .iter()
            .map(Status::code)
            .collect::<Vec<_>>(),
        vec![Some(0), Some(0), Some(0), Some(0)]
    );
}

#[test]
fn deferred_check_failure_aborts_the_chain_and_preserves_session_status() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flash-status-fixture"));
    let temp = TempDir::new("session-deferred-check");
    let probe = Probe::new([fixture.clone()]);
    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            format!("^{} exit 23", fixture.display()),
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the baseline status should complete normally");

    for (connector, label) in [("&&", "and"), ("||", "or")] {
        let marker = temp.path().join(format!("unreached-{label}.txt"));
        let error = session
            .submit(
                "<interactive>",
                format!(
                    "^{0} exit 7 | check | decode bytes | encode bytes {2} \
                     ^{0} late 0 {1} 0",
                    fixture.display(),
                    marker.display(),
                    connector
                ),
                &probe,
                &PosixPlatform,
                &FakeClock::new(),
                &mut sink,
            )
            .expect_err("an unsuccessful deferred check should remain a runtime error");

        assert!(
            error
                .render()
                .contains("checked command was unsuccessful: exit 7"),
            "{}",
            error.render()
        );
        assert!(!marker.exists(), "a runtime error must abort the chain");
        assert_eq!(session.current_status().and_then(Status::code), Some(23));
    }
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
            std::env::temp_dir().join(format!("flash-{label}-{}-{nonce}", std::process::id()));
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
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flash-job-observer-fixture"));
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
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flash-job-observer-fixture"));
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
