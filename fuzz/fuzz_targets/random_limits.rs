#![no_main]

use libfuzzer_sys::fuzz_target;
use opaal_runtime::Value;
use opaal_runtime::eval::ResourceBudget;
use opaal_runtime::operational::random::{RandomLimits, RandomState};
use opaal_runtime::workflow::{CheckArtifact, PlanArtifact, audit_journal};

#[path = "../../crates/opaal-runtime/tests/support/random.rs"]
mod support;

fuzz_target!(|data: &[u8]| {
    let mut bytes = [0; 32];
    let count = bytes.len().min(data.len());
    bytes[..count].copy_from_slice(&data[..count]);
    if bytes[7] & 1 == 1 {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/debug/opaal-standard-host-fixture");
        let mut child = Command::new(fixture)
            .arg("protocol-fuzz")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("build the native fixture before fuzzing Random");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&data[..data.len().min(4096)])
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let min = i64::from_be_bytes(bytes[8..16].try_into().unwrap());
    let max = i64::from_be_bytes(bytes[16..24].try_into().unwrap());
    let (name, arguments) = match bytes[0] % 3 {
        0 => ("int", vec![Value::Int(min), Value::Int(max)]),
        1 => ("float", vec![]),
        _ => (
            "bytes",
            vec![Value::Int(
                i64::from(u16::from_be_bytes([bytes[8], bytes[9]]) % 1026) - 1,
            )],
        ),
    };
    let prepare = || {
        let h = support::Harness::new(&[], &[]);
        h.script.lock().unwrap().repeat = Some(bytes[1]);
        h
    };
    let mut generous = prepare();
    let expected = generous.invoke(name, &arguments);
    let mut limited = prepare();
    limited.limits(RandomLimits {
        max_call_bytes: usize::from(bytes[2]) * 4,
        max_host_bytes: usize::from(bytes[3]) * 8,
        max_integer_candidates: usize::from(bytes[4] % 129),
        ..RandomLimits::default()
    });
    limited.budget =
        ResourceBudget::steps(u64::from(bytes[5])).with_collection_bytes(u64::from(bytes[6]) * 4);
    let result = limited.invoke(name, &arguments);
    if let Ok(value) = &result {
        assert_eq!(expected.as_ref().unwrap(), value);
        match value {
            Value::Int(value) => assert!(min <= *value && *value < max),
            Value::Float(value) => {
                assert!((0.0..1.0).contains(&value.get()));
                assert_eq!((value.get() * (1_u64 << 53) as f64).fract(), 0.0);
            }
            Value::Bytes(value) => assert_eq!(
                value.len(),
                match arguments[0] {
                    Value::Int(count) => count as usize,
                    _ => unreachable!(),
                }
            ),
            _ => unreachable!(),
        }
    }
    let fills = limited.fills();
    assert!(fills.iter().all(|count| (1..=256).contains(count)));
    assert!(
        fills.len()
            <= if name == "int" {
                usize::from(bytes[4] % 129)
            } else {
                4
            }
    );
    assert!(limited.state.consumed_bytes() <= usize::from(bytes[3]) * 8);
    assert_eq!(
        limited.state.progress().confirmed_bytes,
        fills.iter().sum::<usize>()
    );
    assert_eq!(limited.state.progress().uncertain_bytes_upper_bound, 0);

    let mut failed = prepare();
    failed.script.lock().unwrap().fail = true;
    let failed_result = failed.invoke(name, &arguments);
    if !failed.fills().is_empty() {
        assert!(failed_result.is_err());
        assert_eq!(failed.fills().len(), 1);
        assert_eq!(failed.state.progress().confirmed_bytes, 0);
        assert_eq!(
            failed.state.progress().uncertain_bytes_upper_bound,
            failed.fills()[0]
        );
    }
    let mut cancelled = prepare();
    cancelled
        .context
        .narrow_deadline(opaal_runtime::lifetime::Deadline::at(
            opaal_runtime::eval::Instant::from_nanos(1),
        ));
    cancelled.clock.advance(1);
    assert!(cancelled.invoke(name, &arguments).is_err());
    assert!(cancelled.fills().is_empty());
    assert_eq!(cancelled.script.lock().unwrap().closed, 1);
    assert!(
        RandomState::new(
            None,
            RandomLimits {
                max_integer_candidates: 129,
                ..RandomLimits::default()
            }
        )
        .is_err()
    );

    // Closed readers must reject malformed or incompatible transported artifacts.
    let _ = PlanArtifact::parse(data);
    let _ = CheckArtifact::parse(data);
    let _ = audit_journal(data);
});
