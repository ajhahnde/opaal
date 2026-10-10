#![forbid(unsafe_code)]
use std::process::Command;

#[test]
fn hostile_stream_progress_preserves_confirmed_counts_and_recoverable_roles() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("stream-protocol")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        b"standard stream progress, acknowledgement and reap passed\n"
    );
    assert!(result.stderr.is_empty());
}

#[test]
fn native_streams_preserve_exact_bytes_flags_and_owned_teardown() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("streams")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        b"standard streams bytes, failures and reap passed\n"
    );
    assert!(result.stderr.is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn a_debugged_image_refuses_before_launch() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("policy")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        b"standard host signing policy refusal passed\n"
    );
    assert!(result.stderr.is_empty());
}

#[test]
fn racing_path_replacement_cannot_resume_a_different_image() {
    let directory = std::env::temp_dir().join(format!("opaal-worker-race-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let executable = directory.join("worker");
    std::fs::copy(
        env!("CARGO_BIN_EXE_opaal-standard-host-fixture"),
        &executable,
    )
    .unwrap();
    let marker = directory.join("resumed");
    let source = directory.join("replacement.c");
    std::fs::write(&source, format!("#include <stdio.h>\nint main(void) {{ FILE *file = fopen({:?}, \"w\"); if (file) fclose(file); return 0; }}\n", marker.to_str().unwrap())).unwrap();
    let replacement = directory.join("replacement");
    let compiled = Command::new("cc")
        .arg(&source)
        .arg("-o")
        .arg(&replacement)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let result = Command::new(&executable)
        .arg("race")
        .arg(&replacement)
        .output()
        .unwrap();
    let resumed = marker.exists();
    std::fs::remove_dir_all(directory).unwrap();
    assert!(
        !resumed,
        "different image executed before identity admission"
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        b"standard host racing image admission and reap passed\n"
    );
    assert!(result.stderr.is_empty());
}

#[test]
fn a_live_non_system_library_refuses_host_capture() {
    let directory =
        std::env::temp_dir().join(format!("opaal-worker-loader-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let source = directory.join("sentinel.c");
    std::fs::write(&source, "int opaal_loader_sentinel(void) { return 71; }\n").unwrap();
    let library = directory.join(if cfg!(target_os = "macos") {
        "sentinel.dylib"
    } else {
        "sentinel.so"
    });
    let compiled = Command::new("cc")
        .args(if cfg!(target_os = "macos") {
            vec!["-dynamiclib"]
        } else {
            vec!["-shared", "-fPIC"]
        })
        .arg(&source)
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("loader")
        .arg(&library)
        .output()
        .unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"standard host live loader refusal passed\n");
    assert!(result.stderr.is_empty());
}

#[test]
fn sustained_signal_interruptions_do_not_extend_blocked_fill_cancellation() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("interrupt")
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
fn production_worker_refuses_hostile_binding_fill_ack_and_ancillary_records() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("worker-input")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        b"standard host worker input refusal and reap passed\n"
    );
    assert!(result.stderr.is_empty());
}

#[test]
fn failed_maintained_backend_discards_partial_bytes_and_reaps() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("failure")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        b"standard host failed fill and reap passed\n"
    );
    assert!(result.stderr.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn maintained_backend_internal_retries_remain_under_the_original_deadline() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("retry")
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
fn hostile_protocol_missing_ready_and_received_descriptors_reap_without_retry() {
    let result = Command::new(env!("CARGO_BIN_EXE_opaal-standard-host-fixture"))
        .arg("protocol")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        b"standard host protocol refusal and reap passed\n"
    );
    assert!(result.stderr.is_empty());
}

#[test]
fn replaced_or_deleted_executable_cannot_launch_a_different_worker_image() {
    for mode in ["replace", "delete"] {
        let directory =
            std::env::temp_dir().join(format!("opaal-worker-image-{}-{mode}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let executable = directory.join("worker");
        std::fs::copy(
            env!("CARGO_BIN_EXE_opaal-standard-host-fixture"),
            &executable,
        )
        .unwrap();
        let result = Command::new(&executable).arg(mode).output().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            result.stdout,
            b"standard host image identity and reap passed\n"
        );
        assert!(result.stderr.is_empty());
    }
}

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
