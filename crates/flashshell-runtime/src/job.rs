//! Pure lifecycle model for one shell job.
//!
//! This module performs no process, signal, or terminal operation. It gives the
//! later job-control coordinator one deterministic place to register child
//! identities, fold per-process observations into a job state, and retain a
//! completed job until its notice or explicit wait has been acknowledged.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::Status;

/// A stable shell-assigned job identity, distinct from reusable host PIDs.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct JobId(u64);

impl JobId {
    /// Construct a nonzero job identity.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// The numeric identity displayed after `%`.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A stable process identity supplied by the platform child handle.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProcessId(u64);

impl ProcessId {
    /// Construct a nonzero process identity.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// The platform process identity.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Whether a running or resumed job owns the foreground terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobPlacement {
    /// The job is the interactive terminal's foreground work.
    Foreground,
    /// The shell retains terminal ownership while the job runs.
    Background,
}

/// The externally observable lifecycle state of one job record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    /// The job identity exists, but startup has not begun.
    Planned,
    /// Children are being registered and no terminal state is published yet.
    Starting,
    /// The job is running with foreground placement.
    Foreground,
    /// The job is running with background placement.
    Background,
    /// Every nonterminal member is stopped.
    Stopped {
        /// Where a successful `SIGCONT` should resume the job.
        resume: JobPlacement,
    },
    /// Every member has a terminal status and has been waited.
    Completed,
    /// Completion was acknowledged by a notice or an explicit waiter.
    Notified,
    /// The job-table record may no longer be addressed.
    Reaped,
}

/// The latest observation retained for one member process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobMemberState {
    /// No stop or terminal observation is current.
    Running,
    /// The member stopped under this platform signal number.
    Stopped { signal: i32 },
    /// The member produced its final language status.
    Completed(Status),
}

/// An invalid lifecycle transition or process observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobTransitionError {
    /// The operation is not valid in the current state.
    InvalidState {
        operation: &'static str,
        state: JobState,
    },
    /// Startup cannot publish a process-backed job with no members.
    NoMembers,
    /// A process identity was registered twice.
    DuplicateProcess { process: ProcessId },
    /// An observation named a process outside this job.
    UnknownProcess { process: ProcessId },
    /// A terminal process received a later stop or continue observation.
    ProcessAlreadyCompleted { process: ProcessId },
    /// No pending stop or completion notice exists to acknowledge.
    NoPendingNotice,
}

impl fmt::Display for JobTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidState { operation, state } => {
                write!(formatter, "cannot {operation} while job is {state:?}")
            }
            Self::NoMembers => {
                formatter.write_str("cannot start a process-backed job with no members")
            }
            Self::DuplicateProcess { process } => {
                write!(formatter, "process {} is already in the job", process.get())
            }
            Self::UnknownProcess { process } => {
                write!(formatter, "process {} is not in the job", process.get())
            }
            Self::ProcessAlreadyCompleted { process } => {
                write!(formatter, "process {} has already completed", process.get())
            }
            Self::NoPendingNotice => formatter.write_str("the job has no pending notice"),
        }
    }
}

impl Error for JobTransitionError {}

/// One source job and the state observations of its external members.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    id: JobId,
    state: JobState,
    placement: Option<JobPlacement>,
    members: BTreeMap<ProcessId, JobMemberState>,
    notice_pending: bool,
}

impl Job {
    /// Reserve a planned job identity.
    #[must_use]
    pub fn new(id: JobId) -> Self {
        Self {
            id,
            state: JobState::Planned,
            placement: None,
            members: BTreeMap::new(),
            notice_pending: false,
        }
    }

    /// The stable shell-assigned identity.
    #[must_use]
    pub const fn id(&self) -> JobId {
        self.id
    }

    /// The current aggregate lifecycle state.
    #[must_use]
    pub const fn state(&self) -> JobState {
        self.state
    }

    /// Whether a stop or completion notice still needs acknowledgement.
    #[must_use]
    pub const fn notice_pending(&self) -> bool {
        self.notice_pending
    }

    /// The registered members in stable process-ID order.
    pub fn members(&self) -> impl Iterator<Item = (ProcessId, &JobMemberState)> {
        self.members
            .iter()
            .map(|(process, state)| (*process, state))
    }

    /// Enter the startup barrier.
    pub fn begin_starting(&mut self) -> Result<(), JobTransitionError> {
        self.require_state("begin startup", &[JobState::Planned])?;
        self.state = JobState::Starting;
        Ok(())
    }

    /// Register one successfully spawned child before startup is published.
    pub fn add_process(&mut self, process: ProcessId) -> Result<(), JobTransitionError> {
        self.require_state("register a process", &[JobState::Starting])?;
        if self.members.contains_key(&process) {
            return Err(JobTransitionError::DuplicateProcess { process });
        }
        self.members.insert(process, JobMemberState::Running);
        Ok(())
    }

    /// Publish the job after every child has been registered.
    pub fn finish_starting(&mut self, placement: JobPlacement) -> Result<(), JobTransitionError> {
        self.require_state("finish startup", &[JobState::Starting])?;
        if self.members.is_empty() {
            return Err(JobTransitionError::NoMembers);
        }
        self.placement = Some(placement);
        self.state = placement.into();
        self.reconcile();
        Ok(())
    }

    /// Record a child stop observation.
    pub fn observe_stopped(
        &mut self,
        process: ProcessId,
        signal: i32,
    ) -> Result<(), JobTransitionError> {
        self.require_observable("observe a stopped process")?;
        let member = self.member_mut(process)?;
        if matches!(member, JobMemberState::Completed(_)) {
            return Err(JobTransitionError::ProcessAlreadyCompleted { process });
        }
        *member = JobMemberState::Stopped { signal };
        self.reconcile();
        Ok(())
    }

    /// Record a platform-reported continuation of one child.
    pub fn observe_continued(&mut self, process: ProcessId) -> Result<(), JobTransitionError> {
        self.require_observable("observe a continued process")?;
        let member = self.member_mut(process)?;
        if matches!(member, JobMemberState::Completed(_)) {
            return Err(JobTransitionError::ProcessAlreadyCompleted { process });
        }
        *member = JobMemberState::Running;
        if let JobState::Stopped { resume } = self.state {
            self.placement = Some(resume);
        }
        self.notice_pending = false;
        self.reconcile();
        Ok(())
    }

    /// Record one child's terminal status after the platform wait consumed it.
    pub fn observe_completed(
        &mut self,
        process: ProcessId,
        status: Status,
    ) -> Result<(), JobTransitionError> {
        self.require_observable("observe a completed process")?;
        let member = self.member_mut(process)?;
        if matches!(member, JobMemberState::Completed(_)) {
            return Err(JobTransitionError::ProcessAlreadyCompleted { process });
        }
        *member = JobMemberState::Completed(status);
        self.reconcile();
        Ok(())
    }

    /// Move a running job without changing member observations.
    pub fn move_to(&mut self, placement: JobPlacement) -> Result<(), JobTransitionError> {
        self.require_state(
            "change placement",
            &[JobState::Foreground, JobState::Background],
        )?;
        self.placement = Some(placement);
        self.state = placement.into();
        Ok(())
    }

    /// Apply a successfully delivered group continuation in the selected placement.
    pub fn continue_in(&mut self, placement: JobPlacement) -> Result<(), JobTransitionError> {
        if !matches!(self.state, JobState::Stopped { .. }) {
            return Err(JobTransitionError::InvalidState {
                operation: "continue",
                state: self.state,
            });
        }
        for member in self.members.values_mut() {
            if matches!(member, JobMemberState::Stopped { .. }) {
                *member = JobMemberState::Running;
            }
        }
        self.placement = Some(placement);
        self.state = placement.into();
        self.notice_pending = false;
        Ok(())
    }

    /// Acknowledge a prompt-safe stopped-job notice without changing its state.
    pub fn acknowledge_stopped_notice(&mut self) -> Result<(), JobTransitionError> {
        if !matches!(self.state, JobState::Stopped { .. }) {
            return Err(JobTransitionError::InvalidState {
                operation: "acknowledge a stopped notice",
                state: self.state,
            });
        }
        if !self.notice_pending {
            return Err(JobTransitionError::NoPendingNotice);
        }
        self.notice_pending = false;
        Ok(())
    }

    /// Acknowledge completion through a notice or explicit waiter.
    pub fn mark_notified(&mut self) -> Result<(), JobTransitionError> {
        self.require_state("mark notified", &[JobState::Completed])?;
        self.state = JobState::Notified;
        self.notice_pending = false;
        Ok(())
    }

    /// Remove an acknowledged completed job from the addressable job table.
    pub fn mark_reaped(&mut self) -> Result<(), JobTransitionError> {
        self.require_state("mark reaped", &[JobState::Notified])?;
        self.state = JobState::Reaped;
        Ok(())
    }

    /// Record cleanup of a startup that never became an addressable job.
    pub fn abort_starting_after_cleanup(&mut self) -> Result<(), JobTransitionError> {
        self.require_state("abort startup", &[JobState::Starting])?;
        self.state = JobState::Reaped;
        self.members.clear();
        Ok(())
    }

    fn require_observable(&self, operation: &'static str) -> Result<(), JobTransitionError> {
        if matches!(
            self.state,
            JobState::Starting
                | JobState::Foreground
                | JobState::Background
                | JobState::Stopped { .. }
        ) {
            Ok(())
        } else {
            Err(JobTransitionError::InvalidState {
                operation,
                state: self.state,
            })
        }
    }

    fn require_state(
        &self,
        operation: &'static str,
        accepted: &[JobState],
    ) -> Result<(), JobTransitionError> {
        if accepted.contains(&self.state) {
            Ok(())
        } else {
            Err(JobTransitionError::InvalidState {
                operation,
                state: self.state,
            })
        }
    }

    fn member_mut(
        &mut self,
        process: ProcessId,
    ) -> Result<&mut JobMemberState, JobTransitionError> {
        self.members
            .get_mut(&process)
            .ok_or(JobTransitionError::UnknownProcess { process })
    }

    fn reconcile(&mut self) {
        if self.state == JobState::Starting {
            return;
        }
        let all_completed = self
            .members
            .values()
            .all(|member| matches!(member, JobMemberState::Completed(_)));
        if all_completed {
            if self.state != JobState::Completed {
                self.notice_pending = true;
            }
            self.state = JobState::Completed;
            return;
        }
        let all_live_stopped = self.members.values().all(|member| {
            matches!(
                member,
                JobMemberState::Stopped { .. } | JobMemberState::Completed(_)
            )
        });
        if all_live_stopped {
            let resume = self
                .placement
                .expect("a published job always retains its placement");
            if !matches!(self.state, JobState::Stopped { .. }) {
                self.notice_pending = true;
            }
            self.state = JobState::Stopped { resume };
            return;
        }
        self.state = self
            .placement
            .expect("a published job always retains its placement")
            .into();
    }
}

impl From<JobPlacement> for JobState {
    fn from(placement: JobPlacement) -> Self {
        match placement {
            JobPlacement::Foreground => Self::Foreground,
            JobPlacement::Background => Self::Background,
        }
    }
}
