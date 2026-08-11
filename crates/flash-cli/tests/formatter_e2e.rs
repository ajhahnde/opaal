#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use flash_cli::format::{FormatFilesystem, HostFormatFilesystem};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "flash-formatter-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("temporary directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn source(&self, name: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path().join(name);
        fs::write(&path, contents).expect("source should be written");
        path
    }

    fn entries(&self) -> Vec<OsString> {
        let mut entries = fs::read_dir(self.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
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

#[test]
fn formatter_help_and_usage_statuses_are_distinct() {
    let top_level = fsh(["--help"]);
    assert!(top_level.status.success(), "{top_level:?}");
    assert!(
        String::from_utf8(top_level.stdout)
            .unwrap()
            .contains("fsh format --check [--] PATH...")
    );

    let help = fsh(["format", "--help"]);
    assert!(help.status.success(), "{help:?}");
    assert!(help.stderr.is_empty());
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.starts_with("Check or rewrite Flash source formatting\n"));
    assert!(stdout.contains("fsh format --check [--] PATH..."));
    assert!(stdout.contains("fsh format --write [--] PATH..."));
    assert!(stdout.contains("permission bits"));
    assert!(!stdout.contains("async-chain"));

    let misuse = fsh(["format", "--check"]);
    assert_eq!(misuse.status.code(), Some(2), "{misuse:?}");
    assert!(misuse.stdout.is_empty());
    assert_eq!(
        String::from_utf8(misuse.stderr).unwrap(),
        "fsh: format requires at least one path\n"
    );
}

#[test]
fn check_is_silent_for_canonical_and_empty_sources_and_never_writes() {
    let temp = TempDir::new("check-silent");
    let canonical = temp.source("canonical.fsh", b"echo ready\n");
    let empty = temp.source("empty.fsh", b"");
    let before = fs::read(&canonical).unwrap();
    let metadata = fs::metadata(&canonical).unwrap();

    let output = fsh([
        OsString::from("format"),
        OsString::from("--check"),
        canonical.as_os_str().to_owned(),
        empty.as_os_str().to_owned(),
    ]);

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(&canonical).unwrap(), before);
    assert_eq!(fs::metadata(&canonical).unwrap().ino(), metadata.ino());
}

#[test]
fn check_reports_every_noncanonical_and_invalid_operand_in_order() {
    let temp = TempDir::new("check-order");
    let first = temp.source("first.fsh", b"echo   first");
    let invalid = temp.source("invalid.fsh", b"| broken\n");
    let last = temp.source("last.fsh", b"echo   last");

    let output = fsh([
        OsString::from("format"),
        OsString::from("--check"),
        first.as_os_str().to_owned(),
        invalid.as_os_str().to_owned(),
        last.as_os_str().to_owned(),
    ]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let first_offset = stderr.find(first.to_str().unwrap()).unwrap();
    let invalid_offset = stderr.find(invalid.to_str().unwrap()).unwrap();
    let last_offset = stderr.find(last.to_str().unwrap()).unwrap();
    assert!(first_offset < invalid_offset && invalid_offset < last_offset);
    assert_eq!(stderr.matches("error[FMT001]").count(), 2);
    assert!(stderr.contains("pipeline operator cannot begin a stage"));
    assert_eq!(fs::read(first).unwrap(), b"echo   first");
    assert_eq!(fs::read(last).unwrap(), b"echo   last");
}

#[test]
fn write_preflight_failure_leaves_the_complete_batch_untouched() {
    let temp = TempDir::new("write-preflight");
    let first = temp.source("first.fsh", b"echo   first");
    let incomplete = temp.source("incomplete.fsh", b"echo \"");
    let last = temp.source("last.fsh", b"echo   last");

    let output = fsh([
        OsString::from("format"),
        OsString::from("--write"),
        first.as_os_str().to_owned(),
        incomplete.as_os_str().to_owned(),
        last.as_os_str().to_owned(),
    ]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("error[SYN002]")
    );
    assert_eq!(fs::read(first).unwrap(), b"echo   first");
    assert_eq!(fs::read(last).unwrap(), b"echo   last");
}

#[test]
fn write_replaces_exact_bytes_preserves_permissions_and_skips_unchanged_files() {
    let temp = TempDir::new("write-success");
    let unchanged = temp.source("unchanged.fsh", b"echo ready\n");
    let changed = temp.source("changed.fsh", b"echo   'Gr\xc3\xbc\xc3\x9fe'");
    fs::set_permissions(&changed, fs::Permissions::from_mode(0o751)).unwrap();
    let unchanged_before = fs::metadata(&unchanged).unwrap();

    let output = fsh([
        OsString::from("format"),
        OsString::from("--write"),
        unchanged.as_os_str().to_owned(),
        changed.as_os_str().to_owned(),
    ]);

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(&changed).unwrap(), "echo 'Grüße'\n".as_bytes());
    assert_eq!(
        fs::metadata(&changed).unwrap().permissions().mode() & 0o777,
        0o751
    );
    let unchanged_after = fs::metadata(&unchanged).unwrap();
    assert_eq!(unchanged_after.ino(), unchanged_before.ino());
    assert_eq!(unchanged_after.mtime(), unchanged_before.mtime());
    assert_eq!(
        temp.entries(),
        vec![
            OsString::from("changed.fsh"),
            OsString::from("unchanged.fsh")
        ]
    );
}

#[test]
fn directories_symlinks_special_files_and_duplicate_targets_are_refused() {
    let temp = TempDir::new("unsupported-targets");
    let regular = temp.source("regular.fsh", b"echo ready\n");
    let directory = temp.path().join("directory.fsh");
    fs::create_dir(&directory).unwrap();
    let link = temp.path().join("link.fsh");
    symlink(&regular, &link).unwrap();
    let special = temp.path().join("fifo.fsh");
    assert!(
        Command::new("mkfifo")
            .arg(&special)
            .status()
            .expect("mkfifo should start")
            .success()
    );
    let alias = temp.path().join(".").join("regular.fsh");

    let output = fsh([
        OsString::from("format"),
        OsString::from("--check"),
        directory.as_os_str().to_owned(),
        link.as_os_str().to_owned(),
        special.as_os_str().to_owned(),
        regular.as_os_str().to_owned(),
        alias.as_os_str().to_owned(),
    ]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not a regular file"));
    assert!(stderr.contains("final path component is a symlink"));
    assert!(stderr.contains("duplicate"));
    assert!(stderr.contains(regular.to_str().unwrap()));
}

#[test]
fn native_non_utf8_paths_are_formatted_without_loss() {
    let temp = TempDir::new("native-path");
    let name = OsString::from_vec(b"source-\xff.fsh".to_vec());
    let path = temp.path().join(PathBuf::from(name));
    if let Err(error) = fs::write(&path, b"echo   native") {
        if matches!(
            error.kind(),
            std::io::ErrorKind::InvalidInput
                | std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::Unsupported
        ) {
            return;
        }
        panic!("native-path fixture should be written: {error}");
    }

    let output = fsh([
        OsString::from("format"),
        OsString::from("--write"),
        OsString::from("--"),
        path.as_os_str().to_owned(),
    ]);

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(path).unwrap(), b"echo native\n");
}

#[test]
fn atomic_replace_refuses_a_source_changed_since_preflight() {
    let temp = TempDir::new("stale-source");
    let path = temp.source("stale.fsh", b"echo   original");
    let mut filesystem = HostFormatFilesystem;
    let inspection = filesystem.inspect(&path).unwrap();
    let expected = filesystem.read(&path).unwrap();
    fs::write(&path, b"echo external\n").unwrap();

    let error = filesystem
        .replace_atomically(
            &path,
            &expected,
            b"echo original\n",
            inspection.permissions(),
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("contents changed since formatter preflight")
    );
    assert_eq!(fs::read(&path).unwrap(), b"echo external\n");
    assert_eq!(temp.entries(), vec![OsString::from("stale.fsh")]);
}

#[test]
fn atomic_replace_refuses_permissions_changed_since_preflight() {
    let temp = TempDir::new("stale-permissions");
    let path = temp.source("stale.fsh", b"echo   original");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let mut filesystem = HostFormatFilesystem;
    let inspection = filesystem.inspect(&path).unwrap();
    let expected = filesystem.read(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let error = filesystem
        .replace_atomically(
            &path,
            &expected,
            b"echo original\n",
            inspection.permissions(),
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("permissions changed since formatter preflight")
    );
    assert_eq!(fs::read(&path).unwrap(), b"echo   original");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(temp.entries(), vec![OsString::from("stale.fsh")]);
}
