#![forbid(unsafe_code)]

use std::ffi::OsString;

use flashshell_runtime::{
    ByteSize, Duration, FiniteFloat, NativePath, Record, Signal, Status, Value,
};

#[test]
fn finite_floats_are_normalized_and_compare_exactly_with_integers() {
    assert!(FiniteFloat::new(f64::NAN).is_err());
    assert!(FiniteFloat::new(f64::INFINITY).is_err());
    assert!(FiniteFloat::new(f64::NEG_INFINITY).is_err());

    let positive_zero = FiniteFloat::new(0.0).unwrap();
    let negative_zero = FiniteFloat::new(-0.0).unwrap();
    assert_eq!(positive_zero, negative_zero);
    assert_eq!(positive_zero.get().to_bits(), 0.0_f64.to_bits());
    assert_eq!(format!("{:?}", Value::from(negative_zero)), "0.0");

    assert_eq!(Value::Int(42), Value::from(FiniteFloat::new(42.0).unwrap()));
    assert_ne!(Value::Int(42), Value::from(FiniteFloat::new(42.5).unwrap()));
    assert_eq!(
        Value::Int(9_007_199_254_740_992),
        Value::from(FiniteFloat::new(9_007_199_254_740_992.0).unwrap())
    );
    assert_ne!(
        Value::Int(9_007_199_254_740_993),
        Value::from(FiniteFloat::new(9_007_199_254_740_992.0).unwrap())
    );
    assert_eq!(
        Value::Int(i64::MIN),
        Value::from(FiniteFloat::new(i64::MIN as f64).unwrap())
    );
    assert_ne!(
        Value::Int(i64::MAX),
        Value::from(FiniteFloat::new(i64::MAX as f64).unwrap())
    );
}

#[test]
fn lists_and_records_are_immutable_ordered_values() {
    let source = vec![Value::Int(1), Value::string("two")];
    let list = Value::list(source.clone());
    drop(source);
    assert_eq!(
        list.as_list().unwrap(),
        &[Value::Int(1), Value::string("two")]
    );

    let record = Record::new(vec![
        ("first".to_owned(), Value::Int(1)),
        ("second".to_owned(), Value::Bool(true)),
    ])
    .unwrap();
    assert_eq!(record.get("first"), Some(&Value::Int(1)));
    assert_eq!(
        record
            .entries()
            .iter()
            .map(|(key, _)| key.as_ref())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        format!("{:?}", Value::from(record.clone())),
        r#"{"first": 1, "second": true}"#
    );

    let duplicate = Record::new(vec![
        ("name".to_owned(), Value::Int(1)),
        ("name".to_owned(), Value::Int(2)),
    ])
    .unwrap_err();
    assert_eq!(duplicate.key(), "name");
    assert_eq!(duplicate.index(), 1);

    let reversed = Record::new(vec![
        ("second".to_owned(), Value::Bool(true)),
        ("first".to_owned(), Value::Int(1)),
    ])
    .unwrap();
    assert_ne!(record, reversed);
}

#[test]
fn statuses_enforce_leaf_and_aggregate_invariants() {
    let success = Status::exit(0, Duration::from_nanos(12)).unwrap();
    let failure = Status::exit(7, Duration::from_nanos(20)).unwrap();
    let signal = Signal::new(Some(9), Some("SIGKILL".to_owned())).unwrap();
    let signaled = Status::signaled(signal, Duration::from_nanos(3)).unwrap();

    assert!(success.is_ok());
    assert!(!failure.is_ok());
    assert!(!signaled.is_ok());
    assert_eq!(format!("{success}"), "success");
    assert_eq!(format!("{failure}"), "exit 7");
    assert_eq!(format!("{signaled}"), "signal SIGKILL (9)");
    assert_eq!(
        format!("{success:?}"),
        "status(code: 0, signal: null, stages: [], duration: 12ns)"
    );
    assert!(Signal::new(None, None).is_err());
    assert!(Status::exit(0, Duration::from_nanos(-1)).is_err());

    let aggregate = Status::aggregate(
        vec![success.clone(), failure.clone(), signaled.clone()],
        1,
        Duration::from_nanos(50),
    )
    .unwrap();
    assert_eq!(aggregate.code(), Some(7));
    assert_eq!(aggregate.signal(), None);
    assert_eq!(aggregate.stages(), &[success, failure, signaled]);
    assert_eq!(aggregate.duration(), Duration::from_nanos(50));
    assert!(!aggregate.is_ok());
    assert!(Status::aggregate(Vec::new(), 0, Duration::ZERO).is_err());
    assert!(
        Status::aggregate(
            vec![Status::exit(0, Duration::ZERO).unwrap()],
            1,
            Duration::ZERO
        )
        .is_err()
    );
    assert!(Status::aggregate(vec![aggregate], 0, Duration::from_nanos(1)).is_err());
}

#[test]
fn debug_and_display_forms_are_deterministic_and_type_revealing() {
    assert_forms(Value::Null, "null", "null");
    assert_forms(Value::Bool(true), "true", "true");
    assert_forms(Value::Int(-12), "-12", "-12");
    assert_forms(Value::from(FiniteFloat::new(3.0).unwrap()), "3.0", "3.0");
    assert_forms(Value::string("line\n\""), r#""line\n\"""#, "line\n\"");
    assert_forms(
        Value::bytes([b'A', b'Z', 0, 0xff, b'\\', b'"']),
        r#"bytes"AZ\x00\xFF\\\"""#,
        r#"AZ\x00\xFF\\\""#,
    );
    assert_forms(
        Value::from(Duration::from_nanos(-5)),
        "duration(-5ns)",
        "-5ns",
    );
    assert_forms(Value::from(ByteSize::new(42)), "size(42b)", "42b");
    assert_forms(
        Value::list(vec![Value::Int(1), Value::string("x")]),
        r#"[1, "x"]"#,
        r#"[1, "x"]"#,
    );
}

#[cfg(unix)]
#[test]
fn paths_preserve_and_escape_native_unix_bytes() {
    use std::os::unix::ffi::OsStringExt;

    let path = NativePath::new(OsString::from_vec(b"/tmp/\xff".to_vec()));
    assert_eq!(path.as_os_str().as_encoded_bytes(), b"/tmp/\xff");
    assert_forms(Value::from(path), r#"path"/tmp/\xFF""#, r#"/tmp/\xFF"#);
}

fn assert_forms(value: Value, debug: &str, display: &str) {
    assert_eq!(format!("{value:?}"), debug);
    assert_eq!(value.to_string(), display);
}
