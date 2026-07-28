#![forbid(unsafe_code)]

use flashshell_runtime::job::{
    Job, JobId, JobMemberState, JobPlacement, JobState, JobTransitionError, ProcessId,
};
use flashshell_runtime::{Duration, Status};

fn job_id(value: u64) -> JobId {
    JobId::new(value).expect("test job IDs are nonzero")
}

fn process_id(value: u64) -> ProcessId {
    ProcessId::new(value).expect("test process IDs are nonzero")
}

fn status(code: i64) -> Status {
    Status::exit(code, Duration::ZERO).expect("test status is valid")
}

fn started(placement: JobPlacement, processes: &[u64]) -> Job {
    let mut job = Job::new(job_id(7));
    job.begin_starting().unwrap();
    for process in processes {
        job.add_process(process_id(*process)).unwrap();
    }
    job.finish_starting(placement).unwrap();
    job
}

#[test]
fn a_foreground_job_completes_notifies_and_leaves_the_table() {
    let mut job = started(JobPlacement::Foreground, &[11, 12]);

    job.observe_completed(process_id(11), status(0)).unwrap();
    assert_eq!(job.state(), JobState::Foreground);
    assert!(!job.notice_pending());

    job.observe_completed(process_id(12), status(3)).unwrap();
    assert_eq!(job.state(), JobState::Completed);
    assert!(job.notice_pending());

    job.mark_notified().unwrap();
    assert_eq!(job.state(), JobState::Notified);
    assert!(!job.notice_pending());
    job.mark_reaped().unwrap();
    assert_eq!(job.state(), JobState::Reaped);
}

#[test]
fn a_job_is_stopped_only_when_no_live_member_is_running() {
    let mut job = started(JobPlacement::Foreground, &[21, 22, 23]);

    job.observe_stopped(process_id(21), 20).unwrap();
    assert_eq!(job.state(), JobState::Foreground);
    job.observe_completed(process_id(22), status(0)).unwrap();
    assert_eq!(job.state(), JobState::Foreground);
    job.observe_stopped(process_id(23), 20).unwrap();

    assert_eq!(
        job.state(),
        JobState::Stopped {
            resume: JobPlacement::Foreground
        }
    );
    assert!(job.notice_pending());
}

#[test]
fn a_stopped_job_can_resume_in_the_background_then_foreground() {
    let mut job = started(JobPlacement::Foreground, &[31]);
    job.observe_stopped(process_id(31), 20).unwrap();
    job.acknowledge_stopped_notice().unwrap();
    assert!(!job.notice_pending());

    job.continue_in(JobPlacement::Background).unwrap();
    assert_eq!(job.state(), JobState::Background);
    assert_eq!(
        job.members().collect::<Vec<_>>(),
        vec![(process_id(31), &JobMemberState::Running)]
    );

    job.move_to(JobPlacement::Foreground).unwrap();
    assert_eq!(job.state(), JobState::Foreground);
}

#[test]
fn observations_during_startup_are_folded_only_after_the_spawn_barrier() {
    let mut job = Job::new(job_id(8));
    job.begin_starting().unwrap();
    job.add_process(process_id(41)).unwrap();
    job.observe_completed(process_id(41), status(0)).unwrap();

    assert_eq!(job.state(), JobState::Starting);
    job.finish_starting(JobPlacement::Background).unwrap();
    assert_eq!(job.state(), JobState::Completed);
    assert!(job.notice_pending());
}

#[test]
fn platform_continuation_observation_is_idempotent_for_running_state() {
    let mut job = started(JobPlacement::Background, &[51, 52]);
    job.observe_stopped(process_id(51), 21).unwrap();
    job.observe_stopped(process_id(52), 21).unwrap();

    job.observe_continued(process_id(51)).unwrap();
    assert_eq!(job.state(), JobState::Background);
    job.observe_continued(process_id(52)).unwrap();
    assert_eq!(job.state(), JobState::Background);
}

#[test]
fn invalid_membership_and_terminal_reobservations_are_explicit() {
    let mut job = Job::new(job_id(9));
    job.begin_starting().unwrap();
    job.add_process(process_id(61)).unwrap();
    assert_eq!(
        job.add_process(process_id(61)),
        Err(JobTransitionError::DuplicateProcess {
            process: process_id(61)
        })
    );
    job.finish_starting(JobPlacement::Foreground).unwrap();
    assert_eq!(
        job.observe_stopped(process_id(99), 20),
        Err(JobTransitionError::UnknownProcess {
            process: process_id(99)
        })
    );
    job.observe_completed(process_id(61), status(0)).unwrap();
    assert_eq!(
        job.observe_stopped(process_id(61), 20),
        Err(JobTransitionError::InvalidState {
            operation: "observe a stopped process",
            state: JobState::Completed
        })
    );
}

#[test]
fn failed_startup_is_removed_only_after_cleanup() {
    let mut job = Job::new(job_id(10));
    job.begin_starting().unwrap();
    job.add_process(process_id(71)).unwrap();

    job.abort_starting_after_cleanup().unwrap();
    assert_eq!(job.state(), JobState::Reaped);
    assert_eq!(job.members().count(), 0);
}

#[test]
fn job_and_process_identities_reject_zero_and_preserve_values() {
    assert_eq!(JobId::new(0), None);
    assert_eq!(ProcessId::new(0), None);
    assert_eq!(job_id(17).get(), 17);
    assert_eq!(process_id(23).get(), 23);
}
