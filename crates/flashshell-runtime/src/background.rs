//! Background child observation without job-table or terminal ownership.
//!
//! Observer workers receive exactly one owned child, block on the platform's
//! transition seam, and enqueue immutable observations. They never mutate
//! session state or write output.

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread;

use flashshell_platform::{
    ChildProcess, Platform, ProcessGroupId, ProcessStatus, ProcessTransition, WaitError,
};

use crate::Status;
use crate::eval::{Clock, Instant, RuntimeError, RuntimeErrorKind};
use crate::execute::{aggregate_statuses, elapsed, language_status, start_background_pipeline};
use crate::job::{Job, JobId, JobPlacement, JobState, JobTransitionError, ProcessId};
use crate::plan::ExecutionPlan;

/// One immutable transition produced by a background child observer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildObservation {
    /// The child stopped without reaching a terminal state.
    Stopped {
        /// Shell-assigned job identity.
        job: JobId,
        /// Platform process identity.
        process: ProcessId,
        /// Platform signal number that stopped the child.
        signal: i32,
    },
    /// The child completed and was reaped by the blocking observation.
    Completed {
        /// Shell-assigned job identity.
        job: JobId,
        /// Platform process identity.
        process: ProcessId,
        /// Low-level terminal process status.
        status: ProcessStatus,
        /// Runtime-clock reading taken after the terminal observation.
        observed_at: Instant,
    },
    /// Observation failed and one termination plus final-wait cleanup attempt
    /// did not recover a terminal status.
    Failed {
        /// Shell-assigned job identity.
        job: JobId,
        /// Platform process identity.
        process: ProcessId,
        /// Initial blocking-observation failure.
        error: WaitError,
        /// Final-wait failure, when cleanup also failed.
        cleanup: Option<WaitError>,
    },
}

/// The one owned child and its immutable identities assigned to an observer.
#[derive(Debug)]
pub struct ObserverAssignment {
    job: JobId,
    process: ProcessId,
    child: Box<dyn ChildProcess>,
    started_at: Instant,
}

impl ObserverAssignment {
    /// Build one observer assignment.
    #[must_use]
    pub fn new(
        job: JobId,
        process: ProcessId,
        child: Box<dyn ChildProcess>,
        started_at: Instant,
    ) -> Self {
        Self {
            job,
            process,
            child,
            started_at,
        }
    }

    /// The shell-assigned job identity.
    #[must_use]
    pub const fn job(&self) -> JobId {
        self.job
    }

    /// The platform process identity.
    #[must_use]
    pub const fn process(&self) -> ProcessId {
        self.process
    }

    /// The runtime-clock reading taken before the child was spawned.
    #[must_use]
    pub const fn started_at(&self) -> Instant {
        self.started_at
    }

    /// Consume the assignment into all of its owned parts.
    #[must_use]
    pub fn into_parts(self) -> (JobId, ProcessId, Box<dyn ChildProcess>, Instant) {
        (self.job, self.process, self.child, self.started_at)
    }
}

/// Failure while preparing idle observer workers.
#[derive(Debug)]
pub enum ObserverPrepareError {
    /// The host refused to create one planned observer thread.
    ThreadSpawn(io::Error),
    /// A created observer exited before confirming readiness.
    WorkerUnavailable,
}

impl fmt::Display for ObserverPrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadSpawn(error) => write!(formatter, "cannot start child observer: {error}"),
            Self::WorkerUnavailable => {
                formatter.write_str("a child observer exited before becoming ready")
            }
        }
    }
}

impl Error for ObserverPrepareError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ThreadSpawn(error) => Some(error),
            Self::WorkerUnavailable => None,
        }
    }
}

/// Failure while joining observer workers during an explicit shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObserverShutdownError;

impl fmt::Display for ObserverShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a child observer panicked")
    }
}

impl Error for ObserverShutdownError {}

/// Prepared observer workers awaiting source-ordered child assignments.
pub struct ObserverSlots {
    assignments: VecDeque<mpsc::SyncSender<ObserverAssignment>>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl ObserverSlots {
    /// Create `count` idle workers and wait for every readiness confirmation.
    ///
    /// Failure closes every assignment channel and joins every worker created
    /// before the failure, so the caller can return before creating a child.
    pub fn prepare(
        count: usize,
        clock: Arc<dyn Clock>,
        events: mpsc::Sender<ChildObservation>,
    ) -> Result<Self, ObserverPrepareError> {
        let (ready_sender, ready_receiver) = mpsc::channel();
        let mut assignments = VecDeque::with_capacity(count);
        let mut workers = Vec::with_capacity(count);

        for index in 0..count {
            let (assignment_sender, assignment_receiver) = mpsc::sync_channel(0);
            let worker_clock = Arc::clone(&clock);
            let worker_events = events.clone();
            let worker_ready = ready_sender.clone();
            let worker = thread::Builder::new()
                .name(format!("flashshell-child-observer-{index}"))
                .spawn(move || {
                    if worker_ready.send(()).is_err() {
                        return;
                    }
                    let Ok(assignment) = assignment_receiver.recv() else {
                        return;
                    };
                    observe_child(assignment, worker_clock.as_ref(), &worker_events);
                });
            match worker {
                Ok(worker) => {
                    assignments.push_back(assignment_sender);
                    workers.push(worker);
                }
                Err(error) => {
                    drop(ready_sender);
                    close_and_join(assignments, workers);
                    return Err(ObserverPrepareError::ThreadSpawn(error));
                }
            }
        }
        drop(ready_sender);

        for _ in 0..count {
            if ready_receiver.recv().is_err() {
                close_and_join(assignments, workers);
                return Err(ObserverPrepareError::WorkerUnavailable);
            }
        }

        Ok(Self {
            assignments,
            workers,
        })
    }

    /// The number of ready workers that have not received a child.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.assignments.len()
    }

    /// Transfer one child to the next source-ordered observer.
    ///
    /// A failed transfer returns the complete assignment so the coordinator can
    /// terminate and wait the child without losing ownership.
    pub fn assign(&mut self, assignment: ObserverAssignment) -> Result<(), ObserverAssignment> {
        let Some(sender) = self.assignments.pop_front() else {
            return Err(assignment);
        };
        sender.send(assignment).map_err(|error| error.0)
    }

    /// Close idle assignment channels and join every observer worker.
    ///
    /// Callers must release any test- or platform-controlled child wait before
    /// shutdown; a worker assigned a live child remains responsible for waiting
    /// it.
    pub fn shutdown(mut self) -> Result<(), ObserverShutdownError> {
        self.assignments.clear();
        let mut panicked = false;
        for worker in self.workers.drain(..) {
            panicked |= worker.join().is_err();
        }
        if panicked {
            Err(ObserverShutdownError)
        } else {
            Ok(())
        }
    }
}

/// Opaque identity of one queued job notice.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct JobNoticeId(u64);

impl JobNoticeId {
    /// The monotonic session-local notice number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Structured transition that an interactive editor may render.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobNoticeKind {
    /// A complete background job was published.
    Started {
        /// Common process group of every external member.
        group: ProcessGroupId,
    },
    /// Every live member is stopped.
    Stopped,
    /// Every member completed and was reaped by its observer.
    Completed,
    /// Observation could not recover a terminal child status.
    ObservationFailed {
        /// Prompt-safe failure description.
        message: String,
    },
}

/// One prompt-safe structured job notice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobNotice {
    id: JobNoticeId,
    job: JobId,
    kind: JobNoticeKind,
    command: String,
}

impl JobNotice {
    /// Opaque notice identity used for acknowledgement.
    #[must_use]
    pub const fn id(&self) -> JobNoticeId {
        self.id
    }

    /// Shell-assigned job identity.
    #[must_use]
    pub const fn job(&self) -> JobId {
        self.job
    }

    /// Structured notice transition.
    #[must_use]
    pub const fn kind(&self) -> &JobNoticeKind {
        &self.kind
    }

    /// Escaped one-line command label.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }
}

/// Failure to acknowledge the currently pending notice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobNoticeError {
    /// The supplied identity is not the notice currently awaiting rendering.
    NotPending { notice: JobNoticeId },
    /// The pure job lifecycle rejected the acknowledgement.
    Transition(JobTransitionError),
    /// A completed observer worker panicked before it could be joined.
    ObserverPanicked,
}

impl fmt::Display for JobNoticeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPending { notice } => {
                write!(formatter, "job notice {} is not pending", notice.get())
            }
            Self::Transition(error) => error.fmt(formatter),
            Self::ObserverPanicked => formatter.write_str("a child observer panicked"),
        }
    }
}

impl Error for JobNoticeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transition(error) => Some(error),
            Self::NotPending { .. } | Self::ObserverPanicked => None,
        }
    }
}

impl From<JobTransitionError> for JobNoticeError {
    fn from(error: JobTransitionError) -> Self {
        Self::Transition(error)
    }
}

struct JobRecord {
    job: Job,
    group: ProcessGroupId,
    source_order: Vec<ProcessId>,
    member_started: BTreeMap<ProcessId, Instant>,
    completed: BTreeMap<ProcessId, Status>,
    pipefail: bool,
    pipeline_started: Instant,
    latest_terminal: Option<Instant>,
    completion: Option<Status>,
    command: String,
    observation_failed: bool,
    observers: Option<ObserverSlots>,
}

/// Session-owned background job table and raw-observation coordinator.
pub(crate) struct BackgroundJobs {
    clock: Arc<dyn Clock>,
    event_sender: mpsc::Sender<ChildObservation>,
    event_receiver: mpsc::Receiver<ChildObservation>,
    records: BTreeMap<JobId, JobRecord>,
    notices: VecDeque<JobNotice>,
    next_job: Option<u64>,
    next_notice: Option<u64>,
}

impl BackgroundJobs {
    pub(crate) fn new(clock: Arc<dyn Clock>) -> Self {
        let (event_sender, event_receiver) = mpsc::channel();
        Self {
            clock,
            event_sender,
            event_receiver,
            records: BTreeMap::new(),
            notices: VecDeque::new(),
            next_job: Some(1),
            next_notice: Some(1),
        }
    }

    pub(crate) fn start(
        &mut self,
        plan: &ExecutionPlan,
        platform: &dyn Platform,
        command: String,
    ) -> Result<JobId, RuntimeError> {
        let job_id = self.reserve_job(plan)?;
        let started_notice = self.reserve_notice(plan)?;
        let mut job = Job::new(job_id);
        job.begin_starting()
            .map_err(|error| background_state(error, plan))?;

        let mut observers = ObserverSlots::prepare(
            plan.stages().len(),
            Arc::clone(&self.clock),
            self.event_sender.clone(),
        )
        .map_err(|error| {
            RuntimeError::new(
                RuntimeErrorKind::BackgroundObserverUnavailable {
                    message: error.to_string(),
                },
                plan.span(),
            )
        })?;
        let started = match start_background_pipeline(plan, platform, self.clock.as_ref()) {
            Ok(started) => started,
            Err(error) => {
                let _ = observers.shutdown();
                return Err(error);
            }
        };
        let group = started.group();
        let pipeline_started = started.started_at();
        let mut members: VecDeque<_> = started.into_members().into();
        let mut source_order = Vec::with_capacity(members.len());
        let mut member_started = BTreeMap::new();

        for member in &members {
            job.add_process(member.process())
                .map_err(|error| background_state(error, plan))?;
            source_order.push(member.process());
            member_started.insert(member.process(), member.started_at());
        }

        while let Some(member) = members.pop_front() {
            let (process, child, started_at) = member.into_parts();
            let assignment = ObserverAssignment::new(job_id, process, child, started_at);
            if let Err(assignment) = observers.assign(assignment) {
                cleanup_assignment(assignment);
                for member in members {
                    let (_, child, _) = member.into_parts();
                    cleanup_child(child);
                }
                return Err(RuntimeError::new(
                    RuntimeErrorKind::BackgroundAssignmentUnavailable,
                    plan.span(),
                ));
            }
        }

        job.finish_starting(JobPlacement::Background)
            .map_err(|error| background_state(error, plan))?;
        self.records.insert(
            job_id,
            JobRecord {
                job,
                group,
                source_order,
                member_started,
                completed: BTreeMap::new(),
                pipefail: plan.pipefail(),
                pipeline_started,
                latest_terminal: None,
                completion: None,
                command: command.clone(),
                observation_failed: false,
                observers: Some(observers),
            },
        );
        self.notices.push_back(JobNotice {
            id: started_notice,
            job: job_id,
            kind: JobNoticeKind::Started { group },
            command,
        });
        Ok(job_id)
    }

    pub(crate) fn next_notice(&mut self) -> Option<JobNotice> {
        if let Some(notice) = self.notices.front() {
            return Some(notice.clone());
        }
        while self.notices.is_empty() {
            let Ok(observation) = self.event_receiver.try_recv() else {
                break;
            };
            self.apply_observation(observation);
        }
        self.notices.front().cloned()
    }

    pub(crate) fn acknowledge(&mut self, notice: JobNoticeId) -> Result<(), JobNoticeError> {
        let Some(pending) = self.notices.front() else {
            return Err(JobNoticeError::NotPending { notice });
        };
        if pending.id != notice {
            return Err(JobNoticeError::NotPending { notice });
        }
        let pending = self
            .notices
            .pop_front()
            .expect("the pending notice was just observed");
        match pending.kind {
            JobNoticeKind::Started { .. } | JobNoticeKind::ObservationFailed { .. } => Ok(()),
            JobNoticeKind::Stopped => {
                let record = self
                    .records
                    .get_mut(&pending.job)
                    .ok_or(JobNoticeError::NotPending { notice })?;
                record.job.acknowledge_stopped_notice()?;
                Ok(())
            }
            JobNoticeKind::Completed => {
                let mut record = self
                    .records
                    .remove(&pending.job)
                    .ok_or(JobNoticeError::NotPending { notice })?;
                record.job.mark_notified()?;
                record.job.mark_reaped()?;
                if let Some(observers) = record.observers.take() {
                    observers
                        .shutdown()
                        .map_err(|_| JobNoticeError::ObserverPanicked)?;
                }
                Ok(())
            }
        }
    }

    pub(crate) fn job(&self, job: JobId) -> Option<&Job> {
        self.records.get(&job).map(|record| &record.job)
    }

    pub(crate) fn completion(&self, job: JobId) -> Option<&Status> {
        self.records
            .get(&job)
            .and_then(|record| record.completion.as_ref())
    }

    pub(crate) fn group(&self, job: JobId) -> Option<ProcessGroupId> {
        self.records.get(&job).map(|record| record.group)
    }

    fn reserve_job(&mut self, plan: &ExecutionPlan) -> Result<JobId, RuntimeError> {
        let Some(value) = self.next_job else {
            return Err(RuntimeError::new(
                RuntimeErrorKind::BackgroundIdentityExhausted,
                plan.span(),
            ));
        };
        let id = JobId::new(value).expect("background job allocation starts at one");
        self.next_job = value.checked_add(1);
        Ok(id)
    }

    fn reserve_notice(&mut self, plan: &ExecutionPlan) -> Result<JobNoticeId, RuntimeError> {
        let Some(value) = self.next_notice else {
            return Err(RuntimeError::new(
                RuntimeErrorKind::BackgroundIdentityExhausted,
                plan.span(),
            ));
        };
        self.next_notice = value.checked_add(1);
        Ok(JobNoticeId(value))
    }

    fn queue_notice(&mut self, job: JobId, kind: JobNoticeKind, command: String) {
        let Some(value) = self.next_notice else {
            return;
        };
        if matches!(kind, JobNoticeKind::Stopped | JobNoticeKind::Completed) {
            self.notices.retain(|notice| {
                notice.job != job
                    || !matches!(
                        notice.kind,
                        JobNoticeKind::Stopped | JobNoticeKind::Completed
                    )
            });
        }
        self.next_notice = value.checked_add(1);
        self.notices.push_back(JobNotice {
            id: JobNoticeId(value),
            job,
            kind,
            command,
        });
    }

    fn apply_observation(&mut self, observation: ChildObservation) {
        let (job_id, process) = match &observation {
            ChildObservation::Stopped { job, process, .. }
            | ChildObservation::Completed { job, process, .. }
            | ChildObservation::Failed { job, process, .. } => (*job, *process),
        };
        let mut queued = None;
        let Some(record) = self.records.get_mut(&job_id) else {
            return;
        };
        match observation {
            ChildObservation::Stopped { signal, .. } => {
                let was_stopped = matches!(record.job.state(), JobState::Stopped { .. });
                if record.job.observe_stopped(process, signal).is_ok()
                    && !was_stopped
                    && matches!(record.job.state(), JobState::Stopped { .. })
                {
                    queued = Some(JobNoticeKind::Stopped);
                }
            }
            ChildObservation::Completed {
                status,
                observed_at,
                ..
            } => {
                let was_stopped = matches!(record.job.state(), JobState::Stopped { .. });
                let Some(started_at) = record.member_started.get(&process).copied() else {
                    return;
                };
                let status = language_status(status, elapsed(started_at, observed_at));
                if record
                    .job
                    .observe_completed(process, status.clone())
                    .is_err()
                {
                    return;
                }
                record.completed.insert(process, status);
                record.latest_terminal = Some(
                    record
                        .latest_terminal
                        .map_or(observed_at, |latest| latest.max(observed_at)),
                );
                if record.job.state() == JobState::Completed {
                    let stages = record
                        .source_order
                        .iter()
                        .filter_map(|process| record.completed.get(process).cloned())
                        .collect();
                    let duration = elapsed(
                        record.pipeline_started,
                        record
                            .latest_terminal
                            .expect("a completed job has a terminal observation"),
                    );
                    record.completion = Some(aggregate_statuses(stages, record.pipefail, duration));
                    queued = Some(JobNoticeKind::Completed);
                } else if !was_stopped && matches!(record.job.state(), JobState::Stopped { .. }) {
                    queued = Some(JobNoticeKind::Stopped);
                }
            }
            ChildObservation::Failed { error, cleanup, .. } => {
                if !record.observation_failed {
                    record.observation_failed = true;
                    let message = cleanup.map_or_else(
                        || error.to_string(),
                        |cleanup| format!("{error}; cleanup failed: {cleanup}"),
                    );
                    queued = Some(JobNoticeKind::ObservationFailed {
                        message: escape_job_label(&message),
                    });
                }
            }
        }
        if let Some(kind) = queued {
            let command = record.command.clone();
            let _ = record;
            self.queue_notice(job_id, kind, command);
        }
    }
}

/// Escape a command label into one printable terminal line.
#[must_use]
pub fn escape_job_label(source: &str) -> String {
    let mut escaped = String::new();
    for character in source.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{1b}' => escaped.push_str("\\x1b"),
            control if control.is_control() => {
                escaped.push_str(&format!("\\u{{{:x}}}", u32::from(control)));
            }
            printable => escaped.push(printable),
        }
    }
    escaped
}

fn background_state(error: JobTransitionError, plan: &ExecutionPlan) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::BackgroundJobState {
            message: error.to_string(),
        },
        plan.span(),
    )
}

fn cleanup_assignment(assignment: ObserverAssignment) {
    let (_, _, child, _) = assignment.into_parts();
    cleanup_child(child);
}

fn cleanup_child(mut child: Box<dyn ChildProcess>) {
    let _ = child.terminate();
    let _ = child.wait();
}

fn close_and_join(
    assignments: VecDeque<mpsc::SyncSender<ObserverAssignment>>,
    workers: Vec<thread::JoinHandle<()>>,
) {
    drop(assignments);
    for worker in workers {
        let _ = worker.join();
    }
}

fn observe_child(
    assignment: ObserverAssignment,
    clock: &dyn Clock,
    events: &mpsc::Sender<ChildObservation>,
) {
    let ObserverAssignment {
        job,
        process,
        mut child,
        started_at: _started_at,
    } = assignment;

    loop {
        match child.wait_for_transition() {
            Ok(ProcessTransition::Stopped { signal }) => {
                if events
                    .send(ChildObservation::Stopped {
                        job,
                        process,
                        signal,
                    })
                    .is_err()
                {
                    let _ = child.terminate();
                    let _ = child.wait();
                    return;
                }
            }
            Ok(ProcessTransition::Completed(status)) => {
                let _ = events.send(ChildObservation::Completed {
                    job,
                    process,
                    status,
                    observed_at: clock.now(),
                });
                return;
            }
            Err(error) => {
                let _ = child.terminate();
                match child.wait() {
                    Ok(status) => {
                        let _ = events.send(ChildObservation::Completed {
                            job,
                            process,
                            status,
                            observed_at: clock.now(),
                        });
                    }
                    Err(cleanup) => {
                        let _ = events.send(ChildObservation::Failed {
                            job,
                            process,
                            error,
                            cleanup: Some(cleanup),
                        });
                    }
                }
                return;
            }
        }
    }
}
