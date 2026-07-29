//! Background child observation without job-table or terminal ownership.
//!
//! Observer workers receive exactly one owned child, block on the platform's
//! transition seam, and enqueue immutable observations. They never mutate
//! session state or write output.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread;

use flashshell_platform::{ChildProcess, ProcessStatus, ProcessTransition, WaitError};

use crate::eval::{Clock, Instant};
use crate::job::{JobId, ProcessId};

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
