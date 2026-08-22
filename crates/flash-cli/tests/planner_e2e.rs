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
            "flash-planner-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("temporary directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn source(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path().join(name);
        fs::write(&path, contents).expect("source should be written");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fsh(arguments: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fsh"))
        .args(arguments)
        .output()
        .expect("fsh should start")
}

fn fixture_directory() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_flash-e2e-status-fixture"))
        .parent()
        .expect("fixture should have a parent directory")
}

#[test]
fn planner_help_and_misuse_have_exact_channels_and_statuses() {
    let top_level = fsh(["--help"]);
    assert!(top_level.status.success(), "{top_level:?}");
    assert!(top_level.stderr.is_empty());
    assert!(
        String::from_utf8(top_level.stdout)
            .unwrap()
            .contains("fsh plan [--] SOURCE")
    );

    let help = fsh(["plan", "--help"]);
    assert!(help.status.success(), "{help:?}");
    assert!(help.stderr.is_empty());
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.starts_with("Inspect one Flash execution plan without executing it\n"));
    assert!(stdout.contains("read-only PATH metadata checks"));
    assert!(stdout.contains("does not load\nconfiguration or history"));
    assert!(!stdout.contains("async-capsule"));

    let misuse = fsh(["plan"]);
    assert_eq!(misuse.status.code(), Some(2), "{misuse:?}");
    assert!(misuse.stdout.is_empty());
    assert_eq!(
        String::from_utf8(misuse.stderr).unwrap(),
        "fsh: plan requires exactly one source path\n"
    );
}

#[test]
fn one_pipeline_prints_the_resolved_plan_without_running_or_opening_it() {
    let temp = TempDir::new("resolved-no-effects");
    let marker = temp.path().join("external-marker.txt");
    let redirected = temp.path().join("redirected.txt");
    let source = temp.source(
        "command.fsh",
        format!(
            "command flash-e2e-status-fixture late 0 '{}' 0 > '{}'\n",
            marker.display(),
            redirected.display()
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg("plan")
        .arg(&source)
        .current_dir(temp.path())
        .env("PATH", fixture_directory())
        .env("FLASH_PLAN_EXACT", "inherited")
        .output()
        .expect("fsh should start");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("plan span "));
    assert!(stdout.contains(&format!("cwd [{}]", temp.path().display())));
    assert!(stdout.contains("[FLASH_PLAN_EXACT]=[inherited]"));
    assert!(stdout.contains("flash-e2e-status-fixture"));
    assert!(stdout.contains("external"));
    assert!(!stdout.contains("internal command"));
    assert!(stdout.contains(&format!("[{}]", redirected.display())));
    assert!(!marker.exists(), "the external command must not run");
    assert!(!redirected.exists(), "the redirection target must not open");
}

#[test]
fn substitution_and_broader_script_shapes_fail_without_side_effects() {
    let temp = TempDir::new("rejected-effects");
    let marker = temp.path().join("substitution-marker.txt");
    let substitution = temp.source(
        "substitution.fsh",
        format!(
            "^echo $(^flash-e2e-status-fixture late 0 '{}' 0)\n",
            marker.display()
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg("plan")
        .arg(&substitution)
        .env("PATH", fixture_directory())
        .output()
        .expect("fsh should start");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("error[PLAN002]")
    );
    assert!(!marker.exists(), "command substitution must not run");

    let script = temp.source("script.fsh", "let value = 1\n^echo ready\n");
    let output = fsh([OsString::from("plan"), script.into_os_string()]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("error[PLAN001]")
    );
}
