#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "opaal-checker-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn source(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn opaal(arguments: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn checker_help_and_usage_are_opaal_only() {
    let help = opaal(["check", "--help"]);
    assert!(help.status.success(), "{help:?}");
    assert!(help.stderr.is_empty());
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.starts_with("Analyze OPAAL source without executing it\n"));
    assert!(stdout.contains("opaal check [--] SOURCE"));
    assert!(stdout.contains("regular .opaal files"));
    assert!(!stdout.contains("Flash"));

    let misuse = opaal(["check"]);
    assert_eq!(misuse.status.code(), Some(2), "{misuse:?}");
    assert!(misuse.stdout.is_empty());
    assert_eq!(
        String::from_utf8(misuse.stderr).unwrap(),
        "opaal: check requires exactly one source path\n"
    );
}

#[test]
fn declared_language_one_module_closure_checks_silently() {
    let temp = TempDir::new("closure");
    temp.source("dependency.opaal", "language 1\nlet answer = 42\n");
    let root = temp.source(
        "main.opaal",
        "language 1\nimport './dependency.opaal' as dependency\necho ready\n",
    );

    let output = opaal([OsString::from("check"), root.into_os_string()]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn every_module_requires_the_opaal_directive() {
    let temp = TempDir::new("directive");
    temp.source("dependency.opaal", "let answer = 42\n");
    let root = temp.source(
        "main.opaal",
        "language 1\nimport './dependency.opaal' as dependency\n",
    );

    let output = opaal([OsString::from("check"), root.into_os_string()]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[OP2001]"), "{stderr}");
    assert!(stderr.contains("dependency.opaal"), "{stderr}");
}

#[test]
fn checker_analyzes_effects_without_executing_them() {
    let temp = TempDir::new("non-execution");
    let marker = temp.0.join("marker.txt");
    let root = temp.source(
        "main.opaal",
        &format!(
            "language 1\necho changed > '{}'\n^touch '{}'\n",
            marker.display(),
            marker.display()
        ),
    );

    let output = opaal([OsString::from("check"), root.into_os_string()]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!marker.exists(), "checking must not execute source effects");
}
