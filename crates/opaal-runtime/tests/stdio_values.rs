#![forbid(unsafe_code)]
#[path = "support/stdio.rs"]
mod support;
use opaal_runtime::Value;
use opaal_runtime::operational::standard::StandardLimits;
use support::Harness;

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
        ("print", vec![Value::bytes(vec![])], "OPERATION001"),
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
