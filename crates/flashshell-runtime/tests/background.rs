#![forbid(unsafe_code)]

//! Host-free acceptance tests for background child observers.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use flashshell_platform::{
    ChildProcess, ProcessStatus, ProcessTransition, TerminateError, WaitError,
};
use flashshell_runtime::background::{
    ChildObservation, ObserverAssignment, ObserverSlots, escape_job_label,
};
use flashshell_runtime::eval::{FakeClock, Instant};
use flashshell_runtime::job::{JobId, ProcessId};

type ScriptedTransition = Result<ProcessTransition, WaitError>;

struct ControlledChild {
    id: u64,
    transitions: mpsc::Receiver<ScriptedTransition>,
    wait_entries: mpsc::Sender<usize>,
    wait_calls: Arc<AtomicUsize>,
    terminate_calls: Arc<AtomicUsize>,
}

impl fmt::Debug for ControlledChild {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControlledChild")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl ControlledChild {
    fn next_transition(&self) -> ScriptedTransition {
        let call = self.wait_calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.wait_entries
            .send(call)
            .expect("the test retains the wait-entry receiver");
        self.transitions
            .recv()
            .expect("the test supplies every scripted transition")
    }
}

impl ChildProcess for ControlledChild {
    fn id(&self) -> u64 {
        self.id
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        loop {
            match self.next_transition()? {
                ProcessTransition::Stopped { .. } => {}
                ProcessTransition::Continued => {}
                ProcessTransition::Completed(status) => return Ok(status),
            }
        }
    }

    fn wait_for_transition(&mut self) -> Result<ProcessTransition, WaitError> {
        self.next_transition()
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        self.terminate_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct ControlledChildHandle {
    transitions: mpsc::Sender<ScriptedTransition>,
    wait_entries: mpsc::Receiver<usize>,
    wait_calls: Arc<AtomicUsize>,
    terminate_calls: Arc<AtomicUsize>,
}

fn controlled_child(id: u64) -> (Box<dyn ChildProcess>, ControlledChildHandle) {
    let (transition_sender, transition_receiver) = mpsc::channel();
    let (wait_entry_sender, wait_entry_receiver) = mpsc::channel();
    let wait_calls = Arc::new(AtomicUsize::new(0));
    let terminate_calls = Arc::new(AtomicUsize::new(0));
    (
        Box::new(ControlledChild {
            id,
            transitions: transition_receiver,
            wait_entries: wait_entry_sender,
            wait_calls: Arc::clone(&wait_calls),
            terminate_calls: Arc::clone(&terminate_calls),
        }),
        ControlledChildHandle {
            transitions: transition_sender,
            wait_entries: wait_entry_receiver,
            wait_calls,
            terminate_calls,
        },
    )
}

fn job(value: u64) -> JobId {
    JobId::new(value).expect("test job identities are nonzero")
}

fn process(value: u64) -> ProcessId {
    ProcessId::new(value).expect("test process identities are nonzero")
}

#[test]
fn every_observer_is_ready_before_a_child_can_be_assigned() {
    let clock = Arc::new(FakeClock::at(10));
    let (event_sender, event_receiver) = mpsc::channel();
    let mut slots =
        ObserverSlots::prepare(2, clock, event_sender).expect("both observers should become ready");
    let (first_child, first) = controlled_child(41);
    let (second_child, second) = controlled_child(42);

    assert_eq!(slots.remaining(), 2);
    assert_eq!(first.wait_calls.load(Ordering::SeqCst), 0);
    assert_eq!(second.wait_calls.load(Ordering::SeqCst), 0);

    slots
        .assign(ObserverAssignment::new(
            job(1),
            process(41),
            first_child,
            Instant::from_nanos(1),
        ))
        .expect("the first ready observer should accept its child");
    assert_eq!(first.wait_entries.recv().expect("first wait entry"), 1);
    slots
        .assign(ObserverAssignment::new(
            job(1),
            process(42),
            second_child,
            Instant::from_nanos(2),
        ))
        .expect("the second ready observer should accept its child");
    assert_eq!(second.wait_entries.recv().expect("second wait entry"), 1);
    assert_eq!(slots.remaining(), 0);

    first
        .transitions
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("release the first child");
    assert_eq!(
        event_receiver.recv().expect("first completion"),
        ChildObservation::Completed {
            job: job(1),
            process: process(41),
            status: ProcessStatus::Exited(0),
            observed_at: Instant::from_nanos(10),
        }
    );
    second
        .transitions
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))))
        .expect("release the second child");
    assert_eq!(
        event_receiver.recv().expect("second completion"),
        ChildObservation::Completed {
            job: job(1),
            process: process(42),
            status: ProcessStatus::Exited(0),
            observed_at: Instant::from_nanos(10),
        }
    );
    slots.shutdown().expect("both workers should join");
}

#[test]
fn completion_is_waited_and_queued_without_a_coordinator_drain() {
    let clock = Arc::new(FakeClock::at(20));
    let (event_sender, event_receiver) = mpsc::channel();
    let mut slots =
        ObserverSlots::prepare(1, clock.clone(), event_sender).expect("observer should be ready");
    let (child, control) = controlled_child(51);
    slots
        .assign(ObserverAssignment::new(
            job(2),
            process(51),
            child,
            Instant::from_nanos(5),
        ))
        .expect("the observer should accept its child");
    assert_eq!(control.wait_entries.recv().expect("wait entry"), 1);

    clock.advance(7);
    control
        .transitions
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Exited(3))))
        .expect("release the child");
    slots
        .shutdown()
        .expect("the worker should finish before the event is drained");

    assert_eq!(
        event_receiver.recv().expect("queued completion"),
        ChildObservation::Completed {
            job: job(2),
            process: process(51),
            status: ProcessStatus::Exited(3),
            observed_at: Instant::from_nanos(27),
        }
    );
}

#[test]
fn stopped_continued_and_completed_transitions_are_queued_in_child_order() {
    let clock = Arc::new(FakeClock::at(30));
    let (event_sender, event_receiver) = mpsc::channel();
    let mut slots =
        ObserverSlots::prepare(1, clock.clone(), event_sender).expect("observer should be ready");
    let (child, control) = controlled_child(61);
    slots
        .assign(ObserverAssignment::new(
            job(3),
            process(61),
            child,
            Instant::from_nanos(6),
        ))
        .expect("the observer should accept its child");
    assert_eq!(control.wait_entries.recv().expect("first wait"), 1);

    control
        .transitions
        .send(Ok(ProcessTransition::Stopped { signal: 19 }))
        .expect("release the first stop");
    assert_eq!(control.wait_entries.recv().expect("second wait"), 2);
    control
        .transitions
        .send(Ok(ProcessTransition::Continued))
        .expect("release the continuation");
    assert_eq!(control.wait_entries.recv().expect("third wait"), 3);
    control
        .transitions
        .send(Ok(ProcessTransition::Stopped { signal: 20 }))
        .expect("release the second stop");
    assert_eq!(control.wait_entries.recv().expect("fourth wait"), 4);
    clock.advance(9);
    control
        .transitions
        .send(Ok(ProcessTransition::Completed(ProcessStatus::Signaled(
            15,
        ))))
        .expect("release completion");
    slots.shutdown().expect("the worker should join");

    assert_eq!(
        event_receiver.into_iter().collect::<Vec<_>>(),
        vec![
            ChildObservation::Stopped {
                job: job(3),
                process: process(61),
                signal: 19,
            },
            ChildObservation::Continued {
                job: job(3),
                process: process(61),
            },
            ChildObservation::Stopped {
                job: job(3),
                process: process(61),
                signal: 20,
            },
            ChildObservation::Completed {
                job: job(3),
                process: process(61),
                status: ProcessStatus::Signaled(15),
                observed_at: Instant::from_nanos(39),
            },
        ]
    );
}

#[test]
fn closing_an_unused_assignment_channel_exits_the_idle_worker() {
    let (event_sender, _event_receiver) = mpsc::channel();
    let slots = ObserverSlots::prepare(1, Arc::new(FakeClock::new()), event_sender)
        .expect("observer should be ready");

    slots
        .shutdown()
        .expect("an idle worker should exit when its assignment channel closes");
}

#[test]
fn a_wait_failure_terminates_then_performs_one_final_wait() {
    let (event_sender, event_receiver) = mpsc::channel();
    let mut slots = ObserverSlots::prepare(1, Arc::new(FakeClock::new()), event_sender)
        .expect("observer should be ready");
    let (child, control) = controlled_child(71);
    let initial = WaitError::new(io::ErrorKind::Interrupted, "initial observation failure");
    let cleanup = WaitError::new(io::ErrorKind::Other, "cleanup observation failure");
    slots
        .assign(ObserverAssignment::new(
            job(4),
            process(71),
            child,
            Instant::from_nanos(7),
        ))
        .expect("the observer should accept its child");
    assert_eq!(control.wait_entries.recv().expect("first wait"), 1);

    control
        .transitions
        .send(Err(initial.clone()))
        .expect("release the failed observation");
    assert_eq!(control.wait_entries.recv().expect("cleanup wait"), 2);
    assert_eq!(control.terminate_calls.load(Ordering::SeqCst), 1);
    control
        .transitions
        .send(Err(cleanup.clone()))
        .expect("release the failed cleanup");
    slots.shutdown().expect("the worker should join");

    assert_eq!(control.wait_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        event_receiver.recv().expect("failed observation event"),
        ChildObservation::Failed {
            job: job(4),
            process: process(71),
            error: initial,
            cleanup: Some(cleanup),
        }
    );
}

#[test]
fn command_labels_escape_controls_and_preserve_printable_unicode() {
    assert_eq!(
        escape_job_label("one\n\rtwo\t\u{1b}\u{7} café"),
        "one\\n\\rtwo\\t\\x1b\\u{7} café"
    );
}
