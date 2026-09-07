#![forbid(unsafe_code)]

use std::fs;
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
    assert!(!stdout.contains("Flash"));
    assert!(!stdout.contains("fsh"));
}

#[test]
fn pure_language_one_script_is_silent_success() {
    let temp = TempDir::new("pure-success");
    let script = temp.script("main.opaal", "language 1\nlet value = 1\n");
    let output = run(&script, &[]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn every_non_opaal_language_entry_is_rejected() {
    let temp = TempDir::new("language-rejections");
    let cases = [
        ("missing.opaal", "let value = 1\n", "error[OP2001]"),
        ("late.opaal", "let value = 1\nlanguage 1\n", "error[OP2001]"),
        ("future.opaal", "language 2\n", "error[OP2003]"),
        (
            "duplicate.opaal",
            "language 1\nlanguage 1\n",
            "error[OP2004]",
        ),
    ];
    for (name, source, diagnostic) in cases {
        let script = temp.script(name, source);
        let output = run(&script, &[]);
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        assert!(output.stdout.is_empty(), "{name}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(diagnostic), "{name}: {stderr}");
    }

    let flash = temp.script("legacy.fsh", "let value = 1\n");
    let output = run(&flash, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("must use the .opaal extension")
    );
}

#[test]
fn imports_are_opaal_only_and_report_the_imported_source() {
    let temp = TempDir::new("imports");
    let dependency = temp.script("dependency.opaal", "let value = 1\n");
    let root = temp.script(
        "main.opaal",
        "language 1\nimport './dependency.opaal' as dependency\n",
    );
    let output = run(&root, &[]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[OP2001]"), "{stderr}");
    assert!(
        stderr.contains(dependency.file_name().unwrap().to_str().unwrap()),
        "{stderr}"
    );
}

#[test]
fn effectful_source_is_refused_before_process_access() {
    let temp = TempDir::new("effect-refusal");
    let marker = temp.0.join("must-not-exist");
    let source = format!("language 1\n^touch '{}'\n", marker.display());
    let script = temp.script("main.opaal", &source);
    let output = run(&script, &[]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(!marker.exists());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("refused"), "{stderr}");
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
