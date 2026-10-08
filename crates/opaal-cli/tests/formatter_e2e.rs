#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "opaal-formatter-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn source(&self, name: &str, contents: &[u8]) -> PathBuf {
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
fn formatter_help_and_usage_are_opaal_only() {
    let help = opaal(["format", "--help"]);
    assert!(help.status.success(), "{help:?}");
    assert!(help.stderr.is_empty());
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.starts_with("Check or rewrite OPAAL source formatting\n"));
    assert!(stdout.contains("opaal format --check [--] PATH..."));
    assert!(stdout.contains("regular .opaal files"));

    let misuse = opaal(["format", "--check"]);
    assert_eq!(misuse.status.code(), Some(2), "{misuse:?}");
    assert!(misuse.stdout.is_empty());
}

#[test]
fn check_is_silent_for_canonical_opaal_and_never_writes() {
    let temp = TempDir::new("check");
    let source = temp.source("canonical.opaal", b"echo ready\n");
    let before = fs::read(&source).unwrap();
    let metadata = fs::metadata(&source).unwrap();

    let output = opaal([
        OsString::from("format"),
        OsString::from("--check"),
        source.as_os_str().to_owned(),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_eq!(fs::metadata(source).unwrap().ino(), metadata.ino());
}

#[test]
fn write_formats_opaal_and_preserves_permissions() {
    let temp = TempDir::new("write");
    let source = temp.source("changed.opaal", b"echo   'ready'");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();

    let output = opaal([
        OsString::from("format"),
        OsString::from("--write"),
        source.as_os_str().to_owned(),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(&source).unwrap(), b"echo 'ready'\n");
    assert_eq!(
        fs::metadata(source).unwrap().permissions().mode() & 0o777,
        0o751
    );
}

#[test]
fn invalid_source_and_non_opaal_paths_are_rejected_without_writes() {
    let temp = TempDir::new("identity");
    let invalid = temp.source("invalid.opaal", b"| broken\n");
    let legacy = temp.source("legacy.fsh", b"echo   ready");

    for source in [&invalid, &legacy] {
        let before = fs::read(source).unwrap();
        let output = opaal([
            OsString::from("format"),
            OsString::from("--write"),
            source.as_os_str().to_owned(),
        ]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(fs::read(source).unwrap(), before);
    }
}

const LEGACY: &str = include_str!("../../../tests/golden/source-formatting/legacy.opaal");
const CANONICAL: &str = include_str!("../../../tests/golden/source-formatting/canonical.opaal");

#[test]
fn canonical_declaration_migration_preserves_execution_and_unchanged_inode() {
    let temp = TempDir::new("declarations");
    let source = temp.source("legacy.opaal", LEGACY.as_bytes());
    let check = |operation| {
        opaal([
            OsString::from("format"),
            OsString::from(operation),
            OsString::from("--"),
            source.as_os_str().to_owned(),
        ])
    };
    let before = check("--check");
    assert_eq!(before.status.code(), Some(1));
    assert!(before.stdout.is_empty());
    assert!(String::from_utf8(before.stderr).unwrap().contains("FMT001"));
    assert_eq!(fs::read(&source).unwrap(), LEGACY.as_bytes());
    let legacy_result = opaal([source.as_os_str()]);
    assert!(legacy_result.status.success(), "{legacy_result:?}");
    assert!(legacy_result.stdout.is_empty());
    assert!(legacy_result.stderr.is_empty());
    for _ in 0..2 {
        let written = check("--write");
        assert!(written.status.success(), "{written:?}");
        assert!(written.stdout.is_empty());
        assert!(written.stderr.is_empty());
        assert_eq!(fs::read(&source).unwrap(), CANONICAL.as_bytes());
        let metadata = fs::metadata(&source).unwrap();
        let unchanged = check("--write");
        assert!(unchanged.status.success(), "{unchanged:?}");
        assert_eq!(fs::metadata(&source).unwrap().ino(), metadata.ino());
    }
    let canonical = check("--check");
    assert!(canonical.status.success());
    assert!(canonical.stdout.is_empty());
    assert!(canonical.stderr.is_empty());
    let checked = opaal([OsStr::new("check"), source.as_os_str()]);
    assert!(checked.status.success(), "{checked:?}");
    let canonical_result = opaal([source.as_os_str()]);
    assert_eq!(canonical_result.status.code(), legacy_result.status.code());
    assert_eq!(canonical_result.stdout, legacy_result.stdout);
    assert_eq!(canonical_result.stderr, legacy_result.stderr);
}

fn raw_session(text: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn raw_stdin_continues_declarations_and_retains_values_after_diagnostics() {
    let output = raw_session(&format!(
        "{CANONICAL}identity('ready')\nfive()\n| broken\nfive()\n"
    ));
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.matches("...> ").count(), 10, "{stdout:?}");
    assert_eq!(stdout.matches("ready\n").count(), 2, "{stdout:?}");
    assert_eq!(stdout.matches("5\n").count(), 3, "{stdout:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("pipeline operator cannot begin a stage"),
        "{stderr}"
    );
    assert!(!stderr.contains("expected a block"), "{stderr}");

    let pending = raw_session("def waiting()\n");
    let stdout = String::from_utf8(pending.stdout).unwrap();
    assert_eq!(stdout, ">> ...> ");
    let stderr = String::from_utf8(pending.stderr).unwrap();
    assert_eq!(stderr.matches("SYN002").count(), 1, "{stderr}");
    assert!(raw_session("").stderr.is_empty());
}

#[test]
fn formatting_never_loads_imports_or_runs_external_source() {
    let temp = TempDir::new("no-evaluation");
    let marker = temp.0.join("must-not-exist");
    let text = format!(
        "import './missing.opaal' as missing\ndef sentinel() {{ ^touch '{}' }}\nsentinel()\n",
        marker.display()
    );
    let source = temp.source("sentinel.opaal", text.as_bytes());
    let result = opaal([
        OsStr::new("format"),
        OsStr::new("--write"),
        source.as_os_str(),
    ]);
    assert!(result.status.success(), "{result:?}");
    assert!(result.stdout.is_empty());
    assert!(result.stderr.is_empty());
    assert!(!marker.exists());
}
