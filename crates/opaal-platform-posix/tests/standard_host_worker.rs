#![forbid(unsafe_code)]
use std::process::Command;

#[test]
fn same_image_worker_fills_acknowledges_and_reaps() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .env("OPAAL_HOST_SENTINEL", "worker-must-not-inherit")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"standard host fill and reap passed\n");
    assert!(result.stderr.is_empty());
}

#[test]
fn cancellation_terminates_and_reaps_with_inherited_signal_mask() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("cancel")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"standard host fill and reap passed\n");
    assert!(result.stderr.is_empty());
}

#[test]
fn live_reaper_change_refuses_before_launch() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("availability")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        b"standard host availability refusal passed\n"
    );
    assert!(result.stderr.is_empty());
}

#[test]
fn maximum_bytes_acknowledgements_fit_one_operation_interval() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("maximum")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"standard host fill and reap passed\n");
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .starts_with("maximum_bytes=1048576 fills=4096 elapsed_ms=")
    );
}
