#![forbid(unsafe_code)]
#[path = "support/stdio.rs"]
mod support;
use opaal_runtime::Value;
use opaal_runtime::eval::ResourceBudget;
use opaal_runtime::operational::ModuleError;
use opaal_runtime::operational::standard::StandardLimits;
use support::Harness;

#[test]
fn read_reserves_probe_releases_unused_bytes_and_charges_retention_once() {
    let mut h = Harness::new(b"abc");
    h.limits(StandardLimits {
        max_host_bytes: 3,
        ..StandardLimits::default()
    });
    assert_eq!(
        h.invoke("read_stdin", &[Value::Int(3)]).unwrap_err().code(),
        "RESOURCE_LIMIT"
    );
    assert!(h.script.lock().unwrap().calls.is_empty());
    h.limits(StandardLimits {
        max_host_bytes: 4,
        ..StandardLimits::default()
    });
    assert_eq!(
        h.invoke("read_stdin", &[Value::Int(3)]).unwrap(),
        Value::bytes(b"abc".to_vec())
    );
    assert_eq!(h.budget.collection_bytes(), 3);
    assert_eq!(h.state.consumed_bytes(), 3);
    assert_eq!(
        h.invoke("read_stdin", &[Value::Int(0)]).unwrap(),
        Value::bytes(vec![])
    );
    assert_eq!(h.state.consumed_bytes(), 3);
    assert_eq!(
        h.invoke("print", &[Value::string("a")]).unwrap(),
        Value::Null
    );
    assert_eq!(
        h.invoke("read_stdin", &[Value::Int(0)]).unwrap_err().code(),
        "RESOURCE_LIMIT"
    );
}

#[test]
fn entropy_input_and_both_outputs_share_one_nonrestorable_byte_counter() {
    let mut h = Harness::new(b"a");
    h.limits(StandardLimits {
        max_host_bytes: 5,
        ..StandardLimits::default()
    });
    h.state
        .invoke(
            &h.context,
            &h.effects,
            &h.platform,
            &mut h.budget,
            "bytes",
            &[Value::Int(2)],
        )
        .unwrap();
    h.invoke("read_stdin", &[Value::Int(1)]).unwrap();
    h.invoke("print", &[Value::string("b")]).unwrap();
    h.invoke("eprint", &[Value::string("c")]).unwrap();
    assert_eq!(h.state.consumed_bytes(), 5);
    assert_eq!(
        h.invoke("println", &[Value::string("")])
            .unwrap_err()
            .code(),
        "RESOURCE_LIMIT"
    );
    h.invoke("print", &[Value::string("")]).unwrap();
    assert_eq!(h.state.consumed_bytes(), 5);
}

#[test]
fn retention_work_and_repeated_empty_probes_fail_before_the_next_transfer() {
    let mut h = Harness::new(b"abc");
    h.budget = ResourceBudget::opaal().with_collection_bytes(2);
    assert_eq!(
        h.invoke("read_stdin", &[Value::Int(3)]).unwrap_err().code(),
        "RESOURCE_LIMIT"
    );
    assert!(h.script.lock().unwrap().calls.is_empty());
    let mut h = Harness::new(&[]);
    h.budget = ResourceBudget::steps(8);
    for _ in 0..2 {
        h.invoke("read_stdin", &[Value::Int(0)]).unwrap();
    }
    assert_eq!(
        h.invoke("read_stdin", &[Value::Int(0)]).unwrap_err().code(),
        "RESOURCE_LIMIT"
    );
    assert_eq!(h.state.consumed_bytes(), 0);
    assert_eq!(h.script.lock().unwrap().calls.len(), 2);
}

#[test]
fn deadline_after_confirmed_transfer_preserves_primary_progress_and_cleanup() {
    let mut h = Harness::new(&[]);
    {
        let mut script = h.script.lock().unwrap();
        script.advance = Some(h.clock.clone());
        script.close_fail = true;
    }
    assert!(matches!(
        h.invoke("print", &[Value::string("abc")]),
        Err(ModuleError::Cancelled(_))
    ));
    assert_eq!(h.state.progress().confirmed_bytes, 3);
    assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 0);
    assert_eq!(h.state.consumed_bytes(), 3);
    assert!(h.state.take_cleanup_error().is_some());
    assert_eq!(h.script.lock().unwrap().closed, 1);
}
