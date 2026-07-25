#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "flashshell-cli-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("temporary directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn script(&self, name: impl AsRef<Path>, source: &str) -> PathBuf {
        let path = self.path().join(name);
        fs::write(&path, source).expect("script should be written");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fsh(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fsh"))
        .args(args)
        .output()
        .expect("fsh should start")
}

fn run_script(path: &Path, cwd: &Path, fixture_path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(path)
        .current_dir(cwd)
        .env("PATH", fixture_path)
        .output()
        .expect("fsh should start")
}

fn fixture_directory() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_flashshell-e2e-status-fixture"))
        .parent()
        .expect("fixture should have a parent directory")
}

fn status_fixture() -> &'static str {
    "flashshell-e2e-status-fixture"
}

fn stream_fixture() -> &'static str {
    "flashshell-e2e-stream-fixture"
}

#[test]
fn version_reports_binary_name_and_package_version() {
    let output = fsh(&["--version"]);

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "fsh 0.1.0\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn help_describes_the_script_cli() {
    let output = fsh(&["--help"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("FlashShell command shell\n"));
    assert!(stdout.contains("Usage: fsh [OPTIONS] [SCRIPT]\n"));
    assert!(stdout.contains("--version"));
    assert!(output.stderr.is_empty());
}

#[test]
fn script_preserves_empty_space_and_unicode_arguments() {
    let temp = TempDir::new("argv");
    let report = temp.path().join("report.bin");
    let script = temp.script(
        "argv.fsh",
        "flashshell-e2e-process-observer-fixture '' 'two words' 'Grüße 🌍'\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(&script)
        .current_dir(temp.path())
        .env("PATH", fixture_directory())
        .env("FLASH_PROBE_REPORT", &report)
        .output()
        .expect("fsh should start");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let observed = ProcessReport::read(&report);
    assert_eq!(observed.cwd, temp.path().as_os_str().as_bytes());
    assert_eq!(
        observed.argv,
        [
            b"flashshell-e2e-process-observer-fixture".as_slice(),
            b"".as_slice(),
            b"two words".as_slice(),
            "Grüße 🌍".as_bytes(),
        ]
    );
}

#[test]
fn script_with_a_native_non_utf8_path_executes() {
    let temp = TempDir::new("native-path");
    let name = OsString::from_vec(b"script-\xff.fsh".to_vec());
    let script = temp.path().join(Path::new(&name));
    if let Err(error) = fs::write(&script, format!("^{} exit 0\n", status_fixture())) {
        // Some filesystems reject a non-UTF-8 file name (macOS APFS returns
        // EILSEQ, which maps to an uncategorized kind); skip where it cannot be
        // created rather than assert a platform capability the test does not own.
        if matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::InvalidInput
        ) || error.raw_os_error() == Some(92)
        {
            return;
        }
        panic!("native-path script should be written: {error}");
    }

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn generated_64_mib_pipeline_completes_without_capture_or_deadlock() {
    let temp = TempDir::new("large-stream");
    let script = temp.script(
        "large.fsh",
        &format!(
            "^{0} source 67108864 0 | ^{0} sink 67108864 0\n",
            stream_fixture()
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn a_closed_pipeline_reader_preserves_the_last_stage_status() {
    let temp = TempDir::new("broken-pipe");
    let script = temp.script(
        "broken-pipe.fsh",
        &format!(
            "^{} source 67108864 0 | ^{} exit 0\n",
            stream_fixture(),
            status_fixture()
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn a_missing_command_is_a_script_error() {
    let temp = TempDir::new("missing-command");
    let script = temp.script("missing.fsh", "^definitely-not-a-flashshell-command\n");

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("command not found: definitely-not-a-flashshell-command"),
        "{output:?}"
    );
}

#[test]
fn a_failed_redirection_open_is_a_script_error() {
    let temp = TempDir::new("failed-open");
    let script = temp.script(
        "failed-open.fsh",
        &format!("^{} exit 0 > missing/output.bin\n", status_fixture()),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing/output.bin"), "{output:?}");
    assert!(stderr.contains("No such file or directory"), "{output:?}");
}

#[test]
fn the_last_completed_status_becomes_the_fsh_exit_status() {
    let temp = TempDir::new("exit-status");
    let script = temp.script("status.fsh", &format!("^{} exit 23\n", status_fixture()));

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[derive(Debug)]
struct ProcessReport {
    cwd: Vec<u8>,
    argv: Vec<Vec<u8>>,
}

impl ProcessReport {
    fn read(path: &Path) -> Self {
        let bytes = fs::read(path).expect("process report should exist");
        let mut reader = ReportReader::new(&bytes);
        let cwd = reader.field();
        let _value = reader.field();
        let _path = reader.field();
        reader.byte();
        let count = reader.u32() as usize;
        let argv = (0..count).map(|_| reader.field()).collect();
        assert!(reader.remaining().is_empty());
        Self { cwd, argv }
    }
}

struct ReportReader<'a> {
    remaining: &'a [u8],
}

impl<'a> ReportReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn byte(&mut self) -> u8 {
        self.take(1)[0]
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().expect("four bytes should remain"))
    }

    fn field(&mut self) -> Vec<u8> {
        let length = self.u32() as usize;
        self.take(length).to_vec()
    }

    fn take(&mut self, length: usize) -> &'a [u8] {
        let (taken, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        taken
    }

    const fn remaining(&self) -> &'a [u8] {
        self.remaining
    }
}
