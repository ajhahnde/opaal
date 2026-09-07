#![forbid(unsafe_code)]

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn interactive_entry_preselects_opaal_one_without_legacy_startup_state() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"1\n").unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains('1'), "{stdout:?}");
    assert!(!stdout.contains("Flash"), "{stdout:?}");
}
