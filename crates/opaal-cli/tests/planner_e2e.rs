#![forbid(unsafe_code)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str, source: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tmp")
        .join(format!("opaal-plan-{}-{name}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.opaal");
    fs::write(&path, source).unwrap();
    path
}

#[test]
fn opaal_planning_is_always_a_host_free_refusal() {
    let source = fixture("refusal", "language 1\nlet value = 1\n");
    let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args(["plan", source.to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[PLAN004]"), "{stderr}");
    assert!(stderr.contains("OPAAL 1 execution planning"), "{stderr}");
}

#[test]
fn missing_directive_is_not_a_flash_planner_fallback() {
    let source = fixture("missing-directive", "let value = 1\n");
    let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args(["plan", source.to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[OP2001]"), "{stderr}");
    assert!(!stderr.contains("error[FS"), "{stderr}");
}

#[test]
fn planner_help_names_only_the_opaal_boundary() {
    let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args(["plan", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("Inspect the OPAAL planning boundary"));
    assert!(!stdout.contains("Flash"));
}
