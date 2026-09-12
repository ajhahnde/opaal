#![forbid(unsafe_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!(
                "opaal-cli-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn script(&self, name: &str, source: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, source).unwrap();
        path
    }
}

fn opaal(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args(args)
        .output()
        .expect("opaal should start")
}

fn run(path: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opaal"))
        .arg(path)
        .args(arguments)
        .output()
        .expect("opaal should start")
}

#[test]
fn help_and_version_expose_only_opaal_identity() {
    let version = opaal(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        "opaal 1.0.0-alpha.1\n"
    );
    assert!(version.stderr.is_empty());

    let help = opaal(&["--help"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.starts_with("OPAAL language client\n"));
    assert!(stdout.contains("opaal SCRIPT"));
    assert!(stdout.contains(".opaal source"));
    assert!(!stdout.contains("fsh"));
}

#[test]
fn pure_directive_free_script_is_silent_success() {
    let temp = TempDir::new("pure-success");
    let script = temp.script("main.opaal", "let value = 1\n");
    let output = run(&script, &[]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn empty_and_declaration_first_sources_are_silent_successes() {
    let temp = TempDir::new("directive-free-roots");
    for (name, source) in [
        ("empty.opaal", ""),
        ("declaration.opaal", "let value = 1\n"),
    ] {
        let script = temp.script(name, source);
        let output = run(&script, &[]);
        assert!(output.status.success(), "{name}: {output:?}");
        assert!(output.stdout.is_empty(), "{name}: {output:?}");
        assert!(output.stderr.is_empty(), "{name}: {output:?}");
    }
}

#[test]
fn a_former_header_receives_only_an_ordinary_unknown_name_error() {
    let temp = TempDir::new("former-header");
    let script = temp.script("former-header.opaal", "language 1\nlet value = 1\n");
    let output = run(&script, &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[CMD007]"), "{stderr}");
    assert!(
        stderr.contains("unknown bare command or callable `language`"),
        "{stderr}"
    );
    assert!(stderr.contains("cannot fall back to external process lookup"));
    assert!(!stderr.contains("OP200"), "{stderr}");
    assert!(!stderr.contains("directive"), "{stderr}");

    let temp = TempDir::new("unsupported-extension");
    let unsupported = temp.script("legacy.fsh", "let value = 1\n");
    let output = run(&unsupported, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("must use the .opaal extension")
    );
}

#[test]
fn directive_free_imports_run_silently() {
    let temp = TempDir::new("imports");
    temp.script("dependency.opaal", "let value = 1\n");
    let root = temp.script("main.opaal", "import './dependency.opaal' as dependency\n");
    let output = run(&root, &[]);

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn effectful_source_is_refused_before_process_access() {
    let temp = TempDir::new("effect-refusal");
    let marker = temp.0.join("must-not-exist");
    let source = format!("^touch '{}'\n", marker.display());
    let script = temp.script("main.opaal", &source);
    let output = run(&script, &[]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(!marker.exists());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("refused"), "{stderr}");
}

#[test]
fn an_unknown_bare_head_never_falls_back_to_a_path_executable() {
    let temp = TempDir::new("unknown-bare-head");
    let marker = temp.0.join("must-not-exist");
    let executable = temp.script(
        "misspelled",
        &format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
    );
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&executable, permissions).unwrap();
    let script = temp.script("main.opaal", "misspelled\n");

    let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .arg(&script)
        .env("PATH", &temp.0)
        .output()
        .expect("opaal should start");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(!marker.exists(), "the PATH executable must not run");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("unknown bare command or callable"),
        "{stderr}"
    );
}

#[test]
fn historical_launcher_options_are_not_compatibility_switches() {
    for option in [
        "--no-config",
        "--no-history",
        "--async-capsule",
        "--opaal-repl-fixture",
    ] {
        let output = opaal(&[option]);
        assert_eq!(output.status.code(), Some(2), "{option}: {output:?}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("unknown option"), "{option}: {stderr}");
    }
}
