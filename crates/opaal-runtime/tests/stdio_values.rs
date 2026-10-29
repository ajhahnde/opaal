#![forbid(unsafe_code)]
#[path = "support/stdio.rs"]
mod support;
use opaal_runtime::Value;
use opaal_runtime::eval::{ResourceBudget, RuntimeError, RuntimeErrorKind};
use opaal_runtime::module::{NominalTypeId, ValueType};
use opaal_runtime::operational::standard::StandardLimits;
use opaal_runtime::{
    ByteSize, Duration, FiniteFloat, NativePath, Range, Record, Signal, Status, Table,
};
use opaal_runtime::{NominalRecordValue, VariantValue};
use std::sync::Arc;
use support::Harness;

fn nominal(name: &str) -> NominalTypeId {
    let program = support::load(&format!("type {name} = {{}}"));
    program
        .resolve_nominal_type(program.graph().root(), &[name])
        .unwrap()
        .id()
        .clone()
}

fn assert_display(value: Value) {
    for value in [value.clone(), Value::list(vec![value])] {
        let expected = value.to_string();
        for name in ["print", "println", "eprint", "eprintln"] {
            let mut h = Harness::new(&[]);
            h.invoke(name, std::slice::from_ref(&value)).unwrap();
            let mut bytes = expected.as_bytes().to_vec();
            if name.ends_with("println") {
                bytes.push(b'\n');
            }
            let script = h.script.lock().unwrap();
            assert_eq!(
                if name.starts_with('e') {
                    &script.stderr
                } else {
                    &script.stdout
                },
                &bytes
            );
            assert_eq!(h.state.progress().confirmed_bytes, bytes.len());
        }
    }
}

#[test]
fn all_data_families_match_shared_display_at_root_and_nested() {
    for value in [
        Value::Null,
        Value::Bool(true),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::string("\0\r\n\t\u{7f}\"\\ä"),
        Value::bytes((0..=255).collect::<Vec<u8>>()),
        NativePath::new("/tmp/ä\n\"\\").into(),
        Duration::from_nanos(i128::MIN).into(),
        ByteSize::new(u64::MAX).into(),
        Range::new(-2, 5, true).into(),
        Value::list(vec![]),
        Record::new(vec![]).unwrap().into(),
        Record::new(vec![
            ("z\n".into(), Value::Int(1)),
            ("a".into(), Value::string("x")),
        ])
        .unwrap()
        .into(),
        NominalRecordValue::new(
            nominal("Report"),
            vec![ValueType::List(Box::new(ValueType::String))],
            vec![(Arc::from("name"), Value::string("x"))],
        )
        .into(),
        NominalRecordValue::new(nominal("Empty"), vec![], vec![]).into(),
        VariantValue::new(
            nominal("Result"),
            vec![ValueType::Int],
            "Ok",
            vec![Value::Int(42)],
        )
        .into(),
        VariantValue::new(nominal("Result"), vec![], "Empty", vec![]).into(),
        Table::new(vec!["name".into()], vec![vec![Value::string("x")]])
            .unwrap()
            .into(),
        Table::new(vec![], vec![vec![], vec![]]).unwrap().into(),
        Status::exit(7, Duration::ZERO).unwrap().into(),
        Status::aggregate(
            vec![
                Status::exit(0, Duration::ZERO).unwrap(),
                Status::signaled(
                    Signal::new(Some(9), Some("SIG\nKILL".into())).unwrap(),
                    Duration::ZERO,
                )
                .unwrap(),
            ],
            1,
            Duration::from_nanos(42),
        )
        .unwrap()
        .into(),
        Status::signaled(
            Signal::new(None, Some("TERM".into())).unwrap(),
            Duration::ZERO,
        )
        .unwrap()
        .into(),
        Status::signaled(Signal::new(Some(9), None).unwrap(), Duration::ZERO)
            .unwrap()
            .into(),
    ] {
        assert_display(value);
    }
    // A fixed corpus also checks the allocation-free float spelling against its old oracle.
    let mut seed = 0x1357_9bdf_2468_ace0_u64;
    for _ in 0..256 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        if let Ok(float) = FiniteFloat::new(f64::from_bits(seed)) {
            let mut expected = float.get().to_string();
            if !expected.contains(['.', 'e', 'E']) {
                expected.push_str(".0");
            }
            assert_eq!(float.to_string(), expected);
            assert_display(float.into());
        }
    }
    for number in [0.0, -0.0, f64::MAX, f64::MIN_POSITIVE, f64::from_bits(1)] {
        assert_display(FiniteFloat::new(number).unwrap().into());
    }
}

#[cfg(unix)]
#[test]
fn native_paths_and_unicode_keep_their_spelling_across_poll_blocks() {
    use std::os::unix::ffi::OsStringExt;
    for prefix in 4090..4100 {
        let mut bytes = vec![b'a'; prefix];
        bytes.extend_from_slice("💎ä".as_bytes());
        bytes.extend_from_slice(&[0xff, 0xc0, 0x80, 0xe0, 0x80, 0, b'"']);
        assert_display(NativePath::new(std::ffi::OsString::from_vec(bytes)).into());
        assert_display(Value::list(vec![Value::string(format!(
            "{}💎",
            "a".repeat(prefix)
        ))]));
    }
}

#[test]
fn opaque_values_are_refused_without_calling_any_formatter() {
    struct Opaque;
    impl std::fmt::Debug for Opaque {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("opaque debug")
        }
    }
    impl opaal_runtime::Callable for Opaque {
        fn family(&self) -> &'static str {
            "function"
        }
        fn display(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("opaque display")
        }
        fn as_any(&self) -> &dyn std::any::Any {
            panic!("opaque captures")
        }
    }
    let source = opaal_syntax::SourceFile::new(opaal_syntax::SourceId::new(1), "test.opaal", "x");
    let values = vec![
        Value::Callable(Arc::new(Opaque)),
        Value::Error(Arc::new(RuntimeError::new(
            RuntimeErrorKind::UserThrown {
                message: "secret".into(),
            },
            source.span(0..1).unwrap(),
        ))),
    ];
    for value in values {
        for value in [
            value.clone(),
            Value::list(vec![value.clone()]),
            Record::new(vec![("x".into(), value.clone())])
                .unwrap()
                .into(),
            NominalRecordValue::new(
                nominal("Ordinary"),
                vec![],
                vec![(Arc::from("x"), value.clone())],
            )
            .into(),
            VariantValue::new(nominal("Result"), vec![], "Ok", vec![value.clone()]).into(),
            Table::new(vec!["x".into()], vec![vec![value]])
                .unwrap()
                .into(),
        ] {
            let mut h = Harness::new(&[]);
            for _ in 0..2 {
                let error = h
                    .invoke("println", std::slice::from_ref(&value))
                    .unwrap_err();
                assert_eq!(error.code(), "IO005");
                assert_eq!(
                    error.to_string(),
                    "IO005: value is not supported for standard printing"
                );
                assert!(h.script.lock().unwrap().calls.is_empty());
                assert_eq!(h.state.consumed_bytes(), 0);
            }
        }
    }
    // Names alone are not identities.
    assert_display(NominalRecordValue::new(nominal("SecretHeader"), vec![], vec![]).into());
}

#[test]
fn depth_type_metadata_and_hidden_status_metadata_are_bounded() {
    for depth in [128, 129] {
        let mut value = Value::Int(1);
        for _ in 1..depth {
            value = Value::list(vec![value]);
        }
        let mut h = Harness::new(&[]);
        let result = h.invoke("print", &[value]);
        if depth == 128 {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err().code(), "RESOURCE_LIMIT");
            assert!(h.script.lock().unwrap().calls.is_empty());
        }
        let mut argument = ValueType::Int;
        for _ in 2..depth {
            argument = ValueType::List(Box::new(argument));
        }
        let value = NominalRecordValue::new(nominal("Report"), vec![argument], vec![]).into();
        let mut h = Harness::new(&[]);
        let result = h.invoke("print", &[value]);
        if depth == 128 {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err().code(), "RESOURCE_LIMIT");
            assert!(h.script.lock().unwrap().calls.is_empty());
        }
    }
    let status = Status::aggregate(
        (0..64)
            .map(|_| Status::exit(0, Duration::ZERO).unwrap())
            .collect(),
        0,
        Duration::ZERO,
    )
    .unwrap();
    let mut h = Harness::new(&[]);
    h.budget = ResourceBudget::steps(10);
    assert_eq!(
        h.invoke("print", &[status.into()]).unwrap_err().code(),
        "RESOURCE_LIMIT"
    );
    assert!(h.script.lock().unwrap().calls.is_empty());
}

#[test]
fn failed_preparation_keeps_charges_and_resets_only_per_call_progress() {
    let mut h = Harness::new(&[]);
    h.limits(StandardLimits {
        max_call_bytes: 8,
        ..StandardLimits::default()
    });
    h.invoke("print", &[Value::Int(42)]).unwrap();
    let charged = h.budget.collection_bytes();
    assert_eq!(charged, 8);
    assert_eq!(
        h.invoke("println", &[Value::bytes(vec![0xff; 2])])
            .unwrap_err()
            .code(),
        "IO001"
    );
    assert_eq!(h.state.progress().confirmed_bytes, 0);
    assert_eq!(h.state.role_progress().stdout.confirmed_bytes, 2);
    assert_eq!(h.state.consumed_bytes(), 2);
    assert!(h.budget.collection_bytes() > charged);
    h.invoke("println", &[Value::Int(7)]).unwrap();
    assert_eq!(h.script.lock().unwrap().stdout, b"427\n");
    assert_eq!(h.state.role_progress().stdout.confirmed_bytes, 4);
    let mut h = Harness::new(&[]);
    h.budget = ResourceBudget::opaal().with_collection_bytes(0);
    h.invoke("println", &[Value::string("unchanged\0")])
        .unwrap();
    assert_eq!(h.budget.collection_bytes(), 0);
    assert_eq!(
        h.invoke("print", &[Value::Int(1)]).unwrap_err().code(),
        "RESOURCE_LIMIT"
    );
    assert_eq!(h.script.lock().unwrap().stdout, b"unchanged\0\n");
}

#[test]
fn one_preparation_uses_one_scratch_allocation_and_shared_work() {
    let mut h = Harness::new(&[]);
    h.budget = ResourceBudget::steps(6).with_collection_bytes(4096);
    h.invoke("print", &[Value::Int(42)]).unwrap();
    assert_eq!(h.budget.used(), 6);
    assert_eq!(h.budget.collection_bytes(), 4096);
    assert_eq!(h.script.lock().unwrap().stdout, b"42");
    for (value, cap, expected) in [
        (Value::bytes(vec![0xff]), 5, None),
        (Value::bytes(vec![0xff]), 4, Some("IO001")),
    ] {
        let mut h = Harness::new(&[]);
        h.limits(StandardLimits {
            max_call_bytes: cap,
            ..StandardLimits::default()
        });
        match expected {
            None => {
                h.invoke("println", &[value]).unwrap();
                assert_eq!(h.script.lock().unwrap().stdout, b"\\xFF\n");
            }
            Some(code) => {
                assert_eq!(h.invoke("println", &[value]).unwrap_err().code(), code);
                assert!(h.script.lock().unwrap().calls.is_empty());
            }
        }
    }
}

#[test]
fn cancellation_and_timeout_during_preparation_emit_nothing_and_close_the_host() {
    use opaal_runtime::context::OperationalContext;
    use opaal_runtime::eval::{CancelReason, CancellationToken};
    use opaal_runtime::operational::ModuleError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    for timeout in [false, true] {
        let mut h = Harness::new(&[]);
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let clock = h.clock.clone();
        let token = CancellationToken::from_fn(move || {
            if observed.fetch_add(1, Ordering::SeqCst) == 8 {
                if timeout {
                    clock.advance(30_000_000_000);
                } else {
                    return true;
                }
            }
            false
        });
        h.context = OperationalContext::new(support::authority(), token, h.clock.clone(), None);
        let value = Value::list(vec![Value::string("ä\n".repeat(10_000))]);
        let result = h.invoke("print", &[value]);
        assert!(
            matches!(result, Err(ModuleError::Cancelled(reason)) if reason == if timeout { CancelReason::Timeout } else { CancelReason::Requested })
        );
        assert!(h.budget.collection_bytes() > 0);
        assert_eq!(h.state.consumed_bytes(), 0);
        let script = h.script.lock().unwrap();
        assert!(script.calls.is_empty());
        assert_eq!(script.closed, 1);
    }
}

#[test]
fn metadata_and_mixed_failures_follow_bounded_display_order() {
    let long = "x".repeat(20_000);
    for value in [
        NominalRecordValue::new(
            nominal("Report"),
            vec![ValueType::TypeParameter(long.clone())],
            vec![],
        )
        .into(),
        Record::new(vec![(long.clone(), Value::Null)])
            .unwrap()
            .into(),
        Status::signaled(Signal::new(None, Some(long)).unwrap(), Duration::ZERO)
            .unwrap()
            .into(),
    ] {
        let mut h = Harness::new(&[]);
        h.budget = ResourceBudget::steps(5);
        assert_eq!(
            h.invoke("print", &[value]).unwrap_err().code(),
            "RESOURCE_LIMIT"
        );
        assert!(h.script.lock().unwrap().calls.is_empty());
    }
    let source = opaal_syntax::SourceFile::new(opaal_syntax::SourceId::new(1), "test.opaal", "x");
    let error = Value::Error(Arc::new(RuntimeError::new(
        RuntimeErrorKind::ExecutionUnsupported,
        source.span(0..1).unwrap(),
    )));
    for (values, code) in [
        (
            vec![
                Value::string("long enough to exceed the cap"),
                error.clone(),
            ],
            "IO001",
        ),
        (
            vec![error, Value::string("long enough to exceed the cap")],
            "IO005",
        ),
    ] {
        let mut h = Harness::new(&[]);
        h.limits(StandardLimits {
            max_call_bytes: 10,
            ..StandardLimits::default()
        });
        assert_eq!(
            h.invoke("print", &[Value::list(values)])
                .unwrap_err()
                .code(),
            code
        );
        assert!(h.script.lock().unwrap().calls.is_empty());
    }
}

#[test]
fn raw_reads_observe_empty_exact_cap_first_excess_and_sequential_eof() {
    for (input, cap, expected) in [
        (&b""[..], 0, Some(b"".to_vec())),
        (&b"\0\xff"[..], 2, Some(b"\0\xff".to_vec())),
        (&b"abc"[..], 2, None),
        (&b"a"[..], 0, None),
    ] {
        let mut h = Harness::new(input);
        let result = h.invoke("read_stdin", &[Value::Int(cap)]);
        if let Some(expected) = expected {
            assert_eq!(result.unwrap(), Value::bytes(expected.clone()));
            assert_eq!(h.budget.collection_bytes(), expected.len() as u64);
            assert_eq!(h.state.progress().eof, Some(true));
        } else {
            assert_eq!(result.unwrap_err().code(), "IO002");
            assert_eq!(h.budget.collection_bytes(), 0);
            assert_eq!(h.state.progress().eof, None);
        }
        assert_eq!(h.state.consumed_bytes(), input.len());
        assert_eq!(h.state.progress().confirmed_bytes, input.len());
        assert_eq!(h.state.progress().uncertain_bytes_upper_bound, 0);
        assert_eq!(
            h.invoke("read_stdin", &[Value::Int(0)]).unwrap(),
            Value::bytes(vec![])
        );
        assert_eq!(h.state.consumed_bytes(), input.len());
    }
}

#[test]
fn text_raw_and_lf_output_keep_exact_bytes_and_distinct_sinks() {
    for (name, value, expected, stderr) in [
        ("print", Value::string("ä\0"), &b"\xc3\xa4\0"[..], false),
        ("println", Value::string(""), &b"\n"[..], false),
        (
            "println",
            Value::string("already\n"),
            &b"already\n\n"[..],
            false,
        ),
        ("eprint", Value::string(""), &b""[..], true),
        ("eprintln", Value::string("ä"), &b"\xc3\xa4\n"[..], true),
        (
            "write_stdout",
            Value::bytes(vec![0, 255]),
            &b"\0\xff"[..],
            false,
        ),
        (
            "write_stderr",
            Value::bytes(vec![0, 255]),
            &b"\0\xff"[..],
            true,
        ),
    ] {
        let mut h = Harness::new(&[]);
        h.script.lock().unwrap().max_progress = Some(1);
        assert_eq!(h.invoke(name, &[value]).unwrap(), Value::Null);
        assert_eq!(h.state.progress().confirmed_bytes, expected.len());
        assert_eq!(h.state.consumed_bytes(), expected.len());
        let script = h.script.lock().unwrap();
        assert_eq!(
            if stderr {
                &script.stderr
            } else {
                &script.stdout
            },
            expected
        );
        assert!(if stderr {
            script.stdout.is_empty()
        } else {
            script.stderr.is_empty()
        });
    }
}

#[test]
fn wrong_signatures_and_encoded_cap_including_lf_fail_before_transfer() {
    for (name, args, code) in [
        ("read_stdin", vec![Value::Int(-1)], "IO001"),
        ("read_stdin", vec![Value::Int(4)], "IO001"),
        ("print", vec![Value::string("ää")], "IO001"),
        ("println", vec![Value::string("abc")], "IO001"),
        ("eprintln", vec![Value::string("abc")], "IO001"),
        ("write_stdout", vec![Value::string("abc")], "OPERATION001"),
        ("println", vec![], "OPERATION001"),
    ] {
        let mut h = Harness::new(b"input");
        h.limits(StandardLimits {
            max_call_bytes: 3,
            ..StandardLimits::default()
        });
        assert_eq!(h.invoke(name, &args).unwrap_err().code(), code, "{name}");
        assert!(h.script.lock().unwrap().calls.is_empty());
        assert_eq!(h.state.consumed_bytes(), 0);
    }
}
