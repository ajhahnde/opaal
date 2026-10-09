#![forbid(unsafe_code)]
#[path = "support/random.rs"]
mod support;
use opaal_runtime::Value;
use opaal_runtime::eval::ResourceBudget;
use opaal_runtime::operational::random::{MAX_CALL_BYTES, RandomLimits};
use support::Harness;

#[test]
fn rejection_stops_at_candidate_and_narrower_byte_or_work_limits() {
    for (limits, budget, expected) in [
        (RandomLimits::default(), ResourceBudget::opaal(), 128),
        (
            RandomLimits {
                max_integer_candidates: 3,
                ..RandomLimits::default()
            },
            ResourceBudget::opaal(),
            3,
        ),
        (
            RandomLimits {
                max_host_bytes: 16,
                ..RandomLimits::default()
            },
            ResourceBudget::opaal(),
            2,
        ),
        (RandomLimits::default(), ResourceBudget::steps(9), 2),
    ] {
        let mut h = Harness::new(&[], &[]);
        h.script.lock().unwrap().repeat = Some(0);
        h.limits(limits);
        h.budget = budget;
        assert_eq!(
            h.invoke("int", &[Value::Int(0), Value::Int(3)])
                .unwrap_err()
                .code(),
            "RESOURCE_LIMIT"
        );
        assert_eq!(h.fills(), vec![8; expected]);
        assert_eq!(h.state.consumed_bytes(), expected * 8);
        assert_eq!(h.state.progress().confirmed_bytes, expected * 8);
        assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 0);
    }
}

#[test]
fn fills_are_bounded_and_retention_and_host_admission_precede_work() {
    let mut h = Harness::new(&[], &[]);
    h.script.lock().unwrap().repeat = Some(42);
    let Value::Bytes(bytes) = h
        .invoke("bytes", &[Value::Int(MAX_CALL_BYTES as i64)])
        .unwrap()
    else {
        panic!("Bytes expected")
    };
    assert_eq!(bytes.len(), MAX_CALL_BYTES);
    assert_eq!(h.fills(), vec![256; 4096]);
    assert_eq!(h.budget.collection_bytes(), MAX_CALL_BYTES as u64);
    let mut h = Harness::new(&[], &[]);
    h.budget = ResourceBudget::opaal().with_collection_bytes(3);
    assert_eq!(
        h.invoke("bytes", &[Value::Int(4)]).unwrap_err().code(),
        "RESOURCE_LIMIT"
    );
    assert!(h.fills().is_empty());
    let mut h = Harness::new(&[], &[]);
    h.limits(RandomLimits {
        max_host_bytes: 3,
        ..RandomLimits::default()
    });
    assert_eq!(h.invoke("float", &[]).unwrap_err().code(), "RESOURCE_LIMIT");
    assert!(h.fills().is_empty());
}

#[test]
fn failure_is_uncertain_in_full_and_never_retried_or_returned_as_bytes() {
    let mut h = Harness::new(&[], &[]);
    h.script.lock().unwrap().fail = true;
    assert!(h.invoke("bytes", &[Value::Int(257)]).is_err());
    assert_eq!(h.fills(), [256]);
    let progress = h.state.progress();
    assert_eq!(progress.confirmed_bytes, 0);
    assert_eq!(progress.uncertain_bytes_upper_bound, 256);
    assert_eq!(h.state.consumed_bytes(), 256);
    h.state.close().unwrap();
    h.state.close().unwrap();
    assert_eq!(h.script.lock().unwrap().closed, 1);
}

#[test]
fn cumulative_host_bytes_stop_at_exact_first_excess_and_never_restore_after_catch() {
    let mut h = Harness::new(&[], &[]);
    h.script.lock().unwrap().repeat = Some(1);
    for _ in 0..8 {
        h.invoke("bytes", &[Value::Int(MAX_CALL_BYTES as i64)])
            .unwrap();
    }
    assert_eq!(h.state.consumed_bytes(), 8 * MAX_CALL_BYTES);
    let draws = h.fills().len();
    assert_eq!(
        h.invoke("bytes", &[Value::Int(1)]).unwrap_err().code(),
        "RESOURCE_LIMIT"
    );
    assert_eq!(h.invoke("float", &[]).unwrap_err().code(), "RESOURCE_LIMIT");
    assert_eq!(h.fills().len(), draws);
    assert_eq!(
        h.invoke("bytes", &[Value::Int(0)]).unwrap(),
        Value::bytes(Vec::new())
    );
}

#[test]
fn original_operation_timeout_is_sticky_and_returns_no_partial_value() {
    let mut h = Harness::new(&[], &[]);
    {
        let mut script = h.script.lock().unwrap();
        script.repeat = Some(1);
        script.advance = Some(h.clock.clone());
    }
    assert!(matches!(
        h.invoke("bytes", &[Value::Int(257)]),
        Err(opaal_runtime::operational::ModuleError::Cancelled(
            opaal_runtime::eval::CancelReason::Timeout
        ))
    ));
    assert_eq!(h.fills(), [256]);
    assert_eq!(h.state.progress().confirmed_bytes, 256);
    assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 0);
    assert_eq!(h.script.lock().unwrap().closed, 1);
    assert!(matches!(
        h.invoke("bytes", &[Value::Int(0)]),
        Err(opaal_runtime::operational::ModuleError::Cancelled(
            opaal_runtime::eval::CancelReason::Timeout
        ))
    ));
    assert_eq!(h.fills(), [256]);
}

#[test]
fn cancellation_after_an_acknowledged_fill_keeps_progress_and_cleanup_secondary() {
    let mut h = Harness::new(&[], &[]);
    {
        let mut script = h.script.lock().unwrap();
        script.repeat = Some(1);
        script.advance = Some(h.clock.clone());
        script.cleanup_fail = true;
    }
    assert!(matches!(
        h.invoke("float", &[]),
        Err(opaal_runtime::operational::ModuleError::Cancelled(
            opaal_runtime::eval::CancelReason::Timeout
        ))
    ));
    assert_eq!(h.fills(), [8]);
    assert_eq!(h.state.progress().confirmed_bytes, 8);
    assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 0);
    assert_eq!(h.script.lock().unwrap().closed, 1);
    assert!(h.state.take_cleanup_error().is_some());
    h.state.close().unwrap();
    assert_eq!(h.script.lock().unwrap().closed, 1);
}

#[test]
fn cancellation_between_calls_closes_the_existing_worker_without_more_entropy() {
    let mut h = Harness::new(&[1], &[]);
    h.invoke("float", &[]).unwrap();
    h.context
        .narrow_deadline(opaal_runtime::lifetime::Deadline::at(
            opaal_runtime::eval::Instant::from_nanos(1),
        ));
    h.clock.advance(1);
    assert!(matches!(
        h.invoke("bytes", &[Value::Int(0)]),
        Err(opaal_runtime::operational::ModuleError::Cancelled(_))
    ));
    assert_eq!(h.fills(), [8]);
    assert_eq!(h.script.lock().unwrap().closed, 1);
    assert_eq!(h.state.progress().admitted_bytes, 0);
}

#[test]
fn confirmed_fill_survives_acknowledgement_failure_without_a_returned_value() {
    let mut h = Harness::new(&[1], &[]);
    h.script.lock().unwrap().fail_after_confirmation = true;
    assert_eq!(h.invoke("float", &[]).unwrap_err().code(), "OPERATION004");
    assert_eq!(h.fills(), [8]);
    assert_eq!(h.state.consumed_bytes(), 8);
    assert_eq!(h.state.progress().confirmed_bytes, 8);
    assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 0);
}

#[test]
fn rejection_and_chunks_share_the_original_narrowed_deadline() {
    for narrowed in [false, true] {
        let mut h = Harness::new(&[], &[]);
        if narrowed {
            h.context
                .narrow_deadline(opaal_runtime::lifetime::Deadline::at(
                    opaal_runtime::eval::Instant::from_nanos(10),
                ));
        }
        {
            let mut script = h.script.lock().unwrap();
            script.repeat = Some(0);
            script.advance = Some(h.clock.clone());
            script.advance_nanos = Some(if narrowed { 5 } else { 15_000_000_000 });
        }
        let result = if narrowed {
            h.invoke("bytes", &[Value::Int(513)])
        } else {
            h.invoke("int", &[Value::Int(0), Value::Int(3)])
        };
        assert!(matches!(
            result,
            Err(opaal_runtime::operational::ModuleError::Cancelled(
                opaal_runtime::eval::CancelReason::Timeout
            ))
        ));
        assert_eq!(h.fills(), if narrowed { vec![256; 2] } else { vec![8; 2] });
        assert_eq!(
            h.state.progress().confirmed_bytes,
            if narrowed { 512 } else { 16 }
        );
        assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 0);
        assert_eq!(h.script.lock().unwrap().closed, 1);
        assert!(matches!(
            h.invoke("bytes", &[Value::Int(-1)]),
            Err(opaal_runtime::operational::ModuleError::Cancelled(_))
        ));
    }
}

#[test]
fn adapter_primary_and_cleanup_diagnostics_use_existing_secret_redaction() {
    let mut h = Harness::new(&[], &[]);
    h.context
        .insert_secret(
            opaal_runtime::security::Secret::new(
                opaal_runtime::security::SecretId::new("token").unwrap(),
                b"entropy-diagnostic-canary".to_vec(),
            )
            .unwrap(),
        )
        .unwrap();
    {
        let mut script = h.script.lock().unwrap();
        script.fail = true;
        script.cleanup_fail = true;
        script.error_message = Some("failed: entropy-diagnostic-canary");
    }
    let primary = h.invoke("float", &[]).unwrap_err().to_string();
    let secondary = h.state.take_cleanup_error().unwrap();
    assert!(!primary.contains("entropy-diagnostic-canary"));
    assert!(!secondary.message().contains("entropy-diagnostic-canary"));
    assert!(primary.contains(opaal_runtime::security::REDACTED));
    assert!(
        secondary
            .message()
            .contains(opaal_runtime::security::REDACTED)
    );
}

#[test]
fn proven_unstarted_fill_releases_host_capacity_and_cleanup_stays_secondary() {
    let mut h = Harness::new(&[], &[]);
    h.script.lock().unwrap().not_started = true;
    assert!(h.invoke("float", &[]).is_err());
    assert_eq!(h.state.consumed_bytes(), 0);
    assert_eq!(h.state.progress().admitted_bytes, 0);
    assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 0);
    assert!(h.fills().is_empty());
    {
        let mut script = h.script.lock().unwrap();
        script.not_started = false;
        script.fail = true;
        script.cleanup_fail = true;
    }
    let primary = h.invoke("float", &[]).unwrap_err();
    assert!(primary.to_string().contains("injected fill failure"));
    assert!(!primary.to_string().contains("cleanup"));
    assert!(
        h.state
            .take_cleanup_error()
            .unwrap()
            .message()
            .contains("cleanup")
    );
    assert!(h.state.take_cleanup_error().is_none());
    assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 8);
}
