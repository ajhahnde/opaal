#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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
