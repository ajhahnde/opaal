#![no_main]

use libfuzzer_sys::fuzz_target;
use opaal_platform::operational::{OperationalError, OperationalErrorKind};
use opaal_platform::standard_host::{StandardStream, TransferError};
use opaal_runtime::Value;
use opaal_runtime::eval::{Instant, ResourceBudget};
use opaal_runtime::lifetime::Deadline;
use opaal_runtime::operational::standard::StandardLimits;
use opaal_runtime::workflow::{CheckArtifact, PlanArtifact, audit_journal};

#[path = "../../crates/opaal-runtime/tests/support/stdio.rs"]
mod support;

fuzz_target!(|data: &[u8]| {
    let mut control = [0; 16];
    let count = control.len().min(data.len());
    control[..count].copy_from_slice(&data[..count]);
    let input = data.get(16..).unwrap_or_default();
    let payload = &input[..input.len().min(256)];
    let cap = i64::from(control[1]) - 1;
    let text = String::from_utf8_lossy(payload).into_owned();
    let (name, arguments) = match control[0] % 7 {
        0 => ("read_stdin", vec![Value::Int(cap)]),
        1 => ("write_stdout", vec![Value::bytes(payload.to_vec())]),
        2 => ("write_stderr", vec![Value::bytes(payload.to_vec())]),
        n => (
            ["print", "println", "eprint", "eprintln"][usize::from(n - 3)],
            vec![Value::string(text)],
        ),
    };
    let mut expected = support::Harness::new(payload);
    expected.script.lock().unwrap().max_progress = Some(usize::from(control[6]));
    let expected_value = expected.invoke(name, &arguments);
    let mut limited = support::Harness::new(payload);
    let limits = StandardLimits {
        max_call_bytes: usize::from(control[2]),
        max_host_bytes: usize::from(control[3]) * 4,
        operation_timeout: std::time::Duration::from_nanos(u64::from(control[8]) + 1),
        ..StandardLimits::default()
    };
    limited.limits(limits);
    limited.budget = ResourceBudget::steps(u64::from(control[4]))
        .with_collection_bytes(u64::from(control[5]) * 4);
    limited.script.lock().unwrap().max_progress = Some(usize::from(control[6]));
    if control[7] & 1 == 1 {
        let mut script = limited.script.lock().unwrap();
        script.errors.push_back(TransferError {
            error: OperationalError::new(
                match control[9] % 4 {
                    0 => OperationalErrorKind::Protocol,
                    1 => OperationalErrorKind::Io(std::io::ErrorKind::Interrupted),
                    2 => OperationalErrorKind::Io(std::io::ErrorKind::WouldBlock),
                    _ => OperationalErrorKind::Io(std::io::ErrorKind::BrokenPipe),
                },
                "hostile transfer",
            ),
            confirmed_bytes: if control[10] == 255 {
                usize::MAX
            } else {
                usize::from(control[10])
            },
            uncertain_bytes_upper_bound: usize::from(control[11]),
            eof: match control[12] % 3 {
                0 => None,
                1 => Some(false),
                _ => Some(true),
            },
            cleanup_error: None,
        });
    }
    if control[7] & 2 == 2 {
        limited.script.lock().unwrap().advance = Some(limited.clock.clone());
    }
    limited.script.lock().unwrap().close_fail = control[7] & 4 == 4;
    let result = limited.invoke(name, &arguments);
    if let Ok(value) = result {
        assert_eq!(expected_value.unwrap(), value);
        let script = limited.script.lock().unwrap();
        assert_eq!(script.stdout, expected.script.lock().unwrap().stdout);
        assert_eq!(script.stderr, expected.script.lock().unwrap().stderr);
    }
    let progress = limited.state.role_progress();
    let total = [
        progress.entropy,
        progress.stdin,
        progress.stdout,
        progress.stderr,
    ]
    .into_iter()
    .map(|role| role.confirmed_bytes + role.uncertain_bytes_upper_bound)
    .sum::<usize>();
    assert_eq!(limited.state.consumed_bytes(), total);
    assert!(total <= limits.max_host_bytes);
    assert!(
        limited
            .script
            .lock()
            .unwrap()
            .calls
            .iter()
            .all(|(stream, count)| {
                *count > 0
                    && *count
                        <= limits.max_call_bytes + usize::from(*stream == StandardStream::Stdin)
            })
    );
    let mut cancelled = support::Harness::new(payload);
    cancelled
        .context
        .narrow_deadline(Deadline::at(Instant::from_nanos(1)));
    cancelled.clock.advance(1);
    assert!(cancelled.invoke(name, &arguments).is_err());
    assert!(cancelled.script.lock().unwrap().calls.is_empty());

    if control[7] & 8 == 8 {
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
            .expect("build the native fixture before fuzzing I/O");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&input[..input.len().min(4096)])
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let _ = CheckArtifact::parse(input);
    let _ = PlanArtifact::parse(input);
    let _ = audit_journal(input);
});
