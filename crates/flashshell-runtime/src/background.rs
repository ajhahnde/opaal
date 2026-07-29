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
    ChildProcess, JobSignal, Platform, ProcessGroupId, ProcessStatus, ProcessTransition, WaitError,
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
    /// Why observation was abandoned, retained for the lifetime report.
    ///
    /// The notice queue carries the same text, but a notice is consumed by
    /// rendering while a join must still be able to say what it skipped.
    observation_message: Option<String>,
    /// Whether a resume has been requested for the current stop.
    ///
    /// A stop is only cleared by a later observation, so without this the
    /// repeated resume pass inside a wait would signal the same stopped group
    /// once per loop turn.
    resume_requested: bool,
    observers: Option<ObserverSlots>,
}

/// Whether a live background job is running or stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveJobState {
    /// The job is running.
    Running,
    /// The job is stopped and will not proceed until it is resumed.
    Stopped,
}

/// One background job that has not reached a terminal aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveJob {
    job: JobId,
    state: LiveJobState,
    command: String,
}

impl LiveJob {
    /// The job identity.
    #[must_use]
    pub const fn job(&self) -> JobId {
        self.job
    }

    /// Whether the job is running or stopped.
    #[must_use]
    pub const fn state(&self) -> LiveJobState {
        self.state
    }

    /// The stored display-safe command label.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }
}

/// Why one background job is reported as failing at a session boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackgroundFailureReason {
    /// The job completed with a nonzero aggregate status.
    Exited(Status),
    /// Observation failed unrecoverably; the record is quarantined.
    Observation(String),
    /// The group could not be signalled; the record is quarantined.
    Signal(String),
}

/// One background job whose outcome a session boundary must report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackgroundFailure {
    job: JobId,
    command: String,
    reason: BackgroundFailureReason,
}

impl BackgroundFailure {
    /// The job identity.
    #[must_use]
    pub const fn job(&self) -> JobId {
        self.job
    }

    /// The stored display-safe command label.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Why the job is reported.
    #[must_use]
    pub const fn reason(&self) -> &BackgroundFailureReason {
        &self.reason
    }

    /// One complete diagnostic line, without a trailing newline.
    #[must_use]
    pub fn render(&self) -> String {
        match &self.reason {
            BackgroundFailureReason::Exited(status) => format!(
                "[{}] {} exited with status {}",
                self.job.get(),
                self.command,
                status
                    .code()
                    .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
            ),
            BackgroundFailureReason::Observation(message) => format!(
                "[{}] {} was not observed: {message}",
                self.job.get(),
                self.command
            ),
            BackgroundFailureReason::Signal(message) => format!(
                "[{}] {} could not be signalled: {message}",
                self.job.get(),
                self.command
            ),
        }
    }
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
                observation_message: None,
                resume_requested: false,
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

    /// Every job that has not reached a terminal aggregate.
    ///
    /// A completed-but-unacknowledged record is not live: it is drained by the
    /// notice path rather than waited on. A quarantined record is not live
    /// either, because no further observation of it will ever arrive.
    pub(crate) fn live_jobs(&self) -> Vec<LiveJob> {
        self.records
            .iter()
            .filter(|(_, record)| record.completion.is_none() && !record.observation_failed)
            .map(|(id, record)| LiveJob {
                job: *id,
                state: if matches!(record.job.state(), JobState::Stopped { .. }) {
                    LiveJobState::Stopped
                } else {
                    LiveJobState::Running
                },
                command: record.command.clone(),
            })
            .collect()
    }

    /// Resume every stopped job, then wait every live job to a terminal status.
    ///
    /// Resuming first is what keeps the wait from deadlocking: a session that
    /// is ending has no terminal from which a stopped job could be resumed by
    /// hand, so an unresumed stop would never reach a terminal status.
    pub(crate) fn wait_for_quiescence(
        &mut self,
        platform: &dyn Platform,
    ) -> Vec<BackgroundFailure> {
        let mut failures = Vec::new();
        self.resume_stopped(platform, &mut failures);
        // The table owns `event_sender`, so `recv` can only block, never
        // disconnect. Every live record therefore still has an observer that
        // will report, and the loop ends when the last one does.
        while self.has_live_jobs() {
            let Ok(observation) = self.event_receiver.recv() else {
                break;
            };
            self.apply_observation(observation);
            self.resume_stopped(platform, &mut failures);
        }
        self.collect_failures(&mut failures);
        failures
    }

    /// Resume, hang up, and then wait every live job to a terminal status.
    pub(crate) fn hang_up_all(&mut self, platform: &dyn Platform) -> Vec<BackgroundFailure> {
        let mut failures = Vec::new();
        // Resume first: a stopped process cannot act on a hang-up.
        self.resume_stopped(platform, &mut failures);
        for live in self.live_jobs() {
            self.signal_or_quarantine(live.job, JobSignal::Hangup, platform, &mut failures);
        }
        let mut waited = self.wait_for_quiescence(platform);
        failures.append(&mut waited);
        failures.sort_by_key(BackgroundFailure::job);
        failures.dedup_by_key(|failure| failure.job());
        failures
    }

    /// Acknowledge every queued notice without rendering it.
    ///
    /// A mode with no prompt still has to move each completion through
    /// `Notified` and `Reaped`, or the lifecycle would differ by caller and the
    /// queue would grow without bound.
    pub(crate) fn drain_notices(&mut self) {
        while let Some(notice) = self.next_notice() {
            if self.acknowledge(notice.id()).is_err() {
                break;
            }
        }
    }

    fn has_live_jobs(&self) -> bool {
        self.records
            .values()
            .any(|record| record.completion.is_none() && !record.observation_failed)
    }

    /// Resume every stopped live group whose resume has not been sent yet.
    fn resume_stopped(&mut self, platform: &dyn Platform, failures: &mut Vec<BackgroundFailure>) {
        let stopped: Vec<JobId> = self
            .records
            .iter()
            .filter(|(_, record)| {
                record.completion.is_none()
                    && !record.observation_failed
                    && !record.resume_requested
                    && matches!(record.job.state(), JobState::Stopped { .. })
            })
            .map(|(id, _)| *id)
            .collect();
        for job in stopped {
            if let Some(record) = self.records.get_mut(&job) {
                record.resume_requested = true;
            }
            self.signal_or_quarantine(job, JobSignal::Continue, platform, failures);
        }
    }

    /// Deliver one signal, or quarantine the record and report why.
    ///
    /// A shell must not claim it hung up what it could not signal, so a refused
    /// delivery ends the record here rather than leaving a wait that no
    /// observation will ever end.
    fn signal_or_quarantine(
        &mut self,
        job: JobId,
        signal: JobSignal,
        platform: &dyn Platform,
        failures: &mut Vec<BackgroundFailure>,
    ) {
        let Some(record) = self.records.get_mut(&job) else {
            return;
        };
        let group = record.group;
        if let Err(error) = platform.signal_process_group(group, signal) {
            let message = escape_job_label(&error.to_string());
            record.observation_failed = true;
            record.observation_message = Some(message.clone());
            failures.push(BackgroundFailure {
                job,
                command: record.command.clone(),
                reason: BackgroundFailureReason::Signal(message),
            });
        }
    }

    /// Report every record that failed and is not already reported by identity.
    fn collect_failures(&self, failures: &mut Vec<BackgroundFailure>) {
        for (id, record) in &self.records {
            if failures.iter().any(|failure| failure.job == *id) {
                continue;
            }
            let reason = if record.observation_failed {
                BackgroundFailureReason::Observation(
                    record
                        .observation_message
                        .clone()
                        .unwrap_or_else(|| "the observation was abandoned".to_owned()),
                )
            } else {
                match &record.completion {
                    // A status with no code did not exit normally, which is a
                    // failure the join must still report rather than skip.
                    Some(status) if status.code() != Some(0) => {
                        BackgroundFailureReason::Exited(status.clone())
                    }
                    _ => continue,
                }
            };
            failures.push(BackgroundFailure {
                job: *id,
                command: record.command.clone(),
                reason,
            });
        }
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
                    // A newly observed stop is a stop no resume has answered.
                    record.resume_requested = false;
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
                    let message = escape_job_label(&message);
                    record.observation_message = Some(message.clone());
                    queued = Some(JobNoticeKind::ObservationFailed { message });
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
