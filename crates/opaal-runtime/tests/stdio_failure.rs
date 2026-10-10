#![forbid(unsafe_code)]
#[path = "support/stdio.rs"]
mod support;
use opaal_platform::operational::{OperationalError, OperationalErrorKind};
use opaal_platform::standard_host::TransferError;
use opaal_runtime::Value;
use support::Harness;

#[test]
fn role_totals_survive_multiple_calls_eof_and_a_later_failed_transfer() {
    let mut h = Harness::new(b"abc");
    h.invoke("read_stdin", &[Value::Int(3)]).unwrap();
    h.invoke("read_stdin", &[Value::Int(0)]).unwrap();
    h.invoke("print", &[Value::string("one")]).unwrap();
    h.invoke("println", &[Value::string("two")]).unwrap();
    h.invoke("eprint", &[Value::string("diagnostic")]).unwrap();
    h.state
        .invoke(
            &h.context,
            &h.effects,
            &h.platform,
            &mut h.budget,
            "bytes",
            &[Value::Int(8)],
        )
        .unwrap();
    h.script.lock().unwrap().errors.push_back(TransferError {
        error: OperationalError::new(OperationalErrorKind::Protocol, "lost acknowledgement"),
        confirmed_bytes: 1,
        uncertain_bytes_upper_bound: 2,
        eof: None,
        cleanup_error: None,
    });
    assert!(h.invoke("print", &[Value::string("fail")]).is_err());
    let progress = h.state.role_progress();
    assert_eq!(progress.entropy.confirmed_bytes, 8);
    assert_eq!(progress.stdin.confirmed_bytes, 3);
    assert_eq!(progress.stdout.confirmed_bytes, 8);
    assert_eq!(progress.stdout.uncertain_bytes_upper_bound, 2);
    assert_eq!(progress.stderr.confirmed_bytes, 10);
    assert_eq!(h.state.consumed_bytes(), 31);
    assert!(h.invoke("read_stdin", &[Value::Int(-1)]).is_err());
    h.state.close().unwrap();
    assert_eq!(h.state.role_progress(), progress);
}

#[test]
fn broken_pipe_zero_write_and_failed_read_have_no_successful_partial_value() {
    let mut h = Harness::new(&[]);
    h.script
        .lock()
        .unwrap()
        .errors
        .push_back(TransferError::not_started(OperationalError::new(
            OperationalErrorKind::Io(std::io::ErrorKind::BrokenPipe),
            "payload must not enter diagnostics",
        )));
    let error = h
        .invoke("println", &[Value::string("payload")])
        .unwrap_err();
    assert_eq!(error.code(), "IO003");
    assert!(!error.to_string().contains("payload"));
    assert_eq!(h.state.consumed_bytes(), 0);
    h.script.lock().unwrap().max_progress = Some(0);
    assert_eq!(
        h.invoke("print", &[Value::string("a")]).unwrap_err().code(),
        "IO004"
    );
    assert_eq!(h.script.lock().unwrap().calls.len(), 2);
    let mut h = Harness::new(b"abc");
    h.script.lock().unwrap().errors.push_back(TransferError {
        error: OperationalError::new(OperationalErrorKind::Protocol, "injected read failure"),
        confirmed_bytes: 1,
        uncertain_bytes_upper_bound: 2,
        eof: None,
        cleanup_error: None,
    });
    assert_eq!(
        h.invoke("read_stdin", &[Value::Int(3)]).unwrap_err().code(),
        "OPERATION004"
    );
    assert_eq!(h.state.progress().confirmed_bytes, 1);
    assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 2);
    assert_eq!(h.state.consumed_bytes(), 3);
    assert_eq!(h.budget.collection_bytes(), 0);
}

#[test]
fn interrupted_and_nonblocking_attempts_retry_only_pending_bytes_with_fresh_work() {
    for kind in [
        std::io::ErrorKind::Interrupted,
        std::io::ErrorKind::WouldBlock,
    ] {
        let mut h = Harness::new(b"abc");
        h.script
            .lock()
            .unwrap()
            .errors
            .push_back(TransferError::not_started(OperationalError::new(
                OperationalErrorKind::Io(kind),
                "no progress",
            )));
        assert_eq!(
            h.invoke("read_stdin", &[Value::Int(3)]).unwrap(),
            Value::bytes(b"abc".to_vec())
        );
        assert_eq!(h.state.consumed_bytes(), 3);
        assert_eq!(h.script.lock().unwrap().calls.len(), 3);
        assert!(h.budget.used() >= 10);
    }
}

#[test]
fn impossible_progress_refuses_and_charges_one_uncertain_chunk_then_closes() {
    let mut h = Harness::new(&[]);
    h.script.lock().unwrap().errors.push_back(TransferError {
        error: OperationalError::new(OperationalErrorKind::Protocol, "forged count"),
        confirmed_bytes: usize::MAX,
        uncertain_bytes_upper_bound: 1,
        eof: None,
        cleanup_error: None,
    });
    assert_eq!(
        h.invoke("print", &[Value::string("abc")])
            .unwrap_err()
            .code(),
        "OPERATION004"
    );
    assert_eq!(h.state.progress().confirmed_bytes, 0);
    assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 3);
    assert_eq!(h.state.consumed_bytes(), 3);
    assert_eq!(h.script.lock().unwrap().closed, 1);
    assert!(h.invoke("print", &[Value::string("")]).is_err());
}

#[test]
fn acknowledged_eof_survives_a_later_transport_failure_without_a_partial_value() {
    let mut h = Harness::new(&[]);
    let mut error = TransferError::not_started(OperationalError::new(
        OperationalErrorKind::Protocol,
        "acknowledgement failed",
    ));
    error.eof = Some(true);
    h.script.lock().unwrap().errors.push_back(error);
    assert!(h.invoke("read_stdin", &[Value::Int(1)]).is_err());
    assert_eq!(h.state.progress().eof, Some(true));
    assert_eq!(h.state.consumed_bytes(), 0);
    assert_eq!(h.budget.collection_bytes(), 0);
}
