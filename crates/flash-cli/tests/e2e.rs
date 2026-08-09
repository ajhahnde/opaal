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
            "flash-cli-{label}-{}-{sequence}",
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
    Path::new(env!("CARGO_BIN_EXE_flash-e2e-status-fixture"))
        .parent()
        .expect("fixture should have a parent directory")
}

fn status_fixture() -> &'static str {
    "flash-e2e-status-fixture"
}

fn stream_fixture() -> &'static str {
    "flash-e2e-stream-fixture"
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
    assert!(stdout.starts_with("Flash command shell\n"));
    assert!(stdout.contains("Usage: fsh [OPTIONS] [SCRIPT]\n"));
    assert!(stdout.contains("--version"));
    assert!(output.stderr.is_empty());
}

#[test]
fn the_help_text_does_not_advertise_the_reserved_chain_mode() {
    let output = fsh(&["--help"]);

    assert!(output.status.success());
    let rendered = String::from_utf8(output.stdout).expect("help output should be UTF-8");
    assert!(!rendered.contains("async-chain"));
}

#[test]
fn the_reserved_chain_mode_uses_the_environment_and_short_circuits() {
    let temp = TempDir::new("reserved-chain");
    let marker = temp.path().join("unreached.txt");
    let source = format!(
        "^{} exit $FLASH_OK || ^{} late 0 {} 9",
        status_fixture(),
        status_fixture(),
        marker.display()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .args([
            "--async-chain",
            source.as_str(),
            "--async-pipefail",
            "--async-capture-limit",
            "4096",
        ])
        .current_dir(temp.path())
        .env("PATH", fixture_directory())
        .env("FLASH_OK", "0")
        .output()
        .expect("fsh should start");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(
        !marker.exists(),
        "the successful left side must skip the right"
    );
}

#[test]
fn a_script_runs_and_joins_a_background_conditional_chain() {
    let temp = TempDir::new("background-chain");
    let marker = temp.path().join("reached.txt");
    let script = temp.script(
        "background-chain.fsh",
        &format!(
            "^{} exit 7 || ^{} late 0 {} 0 &\n^{} exit 0\n",
            status_fixture(),
            status_fixture(),
            marker.display(),
            status_fixture(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(
        marker.exists(),
        "the child shell should reach and complete the right side"
    );
}

#[test]
fn static_imports_are_loaded_from_the_filesystem_without_execution() {
    let temp = TempDir::new("static-import");
    let marker = temp.path().join("import-ran.txt");
    temp.script(
        "dependency.fsh",
        &format!("^{} late 0 {} 0\n", status_fixture(), marker.display()),
    );
    let script = temp.script(
        "main.fsh",
        &format!("import './dependency.fsh'\n^{} exit 0\n", status_fixture()),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(
        !marker.exists(),
        "a load-only imported module must not execute initialization"
    );
}

#[test]
fn named_imports_initialize_through_the_filesystem_while_load_only_siblings_stay_dormant() {
    let temp = TempDir::new("named-import-runtime");
    temp.script("dependency.fsh", "let answer = 0\nexport { answer }\n");
    temp.script("dormant.fsh", "export BROKEN = $missing\n");
    let script = temp.script(
        "main.fsh",
        &format!(
            concat!(
                "import {{ answer }} from './dependency.fsh'\n",
                "import './dormant.fsh'\n",
                "^{} exit $answer\n",
            ),
            status_fixture(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn named_initializers_share_environment_and_working_directory_with_the_root() {
    let temp = TempDir::new("named-import-shared-state");
    let nested = temp.path().join("nested");
    fs::create_dir(&nested).expect("the dependency target directory should be created");
    let report = temp.path().join("report.bin");
    temp.script(
        "dependency.fsh",
        concat!(
            "let ready = true\n",
            "export { ready }\n",
            "export FLASH_PROBE_VALUE = 'dependency'\n",
            "cd './nested'\n",
        ),
    );
    let script = temp.script(
        "main.fsh",
        concat!(
            "import { ready } from './dependency.fsh'\n",
            "flash-e2e-process-observer-fixture\n",
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(&script)
        .current_dir(temp.path())
        .env("PATH", fixture_directory())
        .env("FLASH_PROBE_REPORT", &report)
        .output()
        .expect("fsh should start");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    let observed = ProcessReport::read(&report);
    assert_eq!(observed.cwd, nested.as_os_str().as_bytes());
    assert_eq!(observed.value, b"dependency");
}

#[test]
fn the_root_inherits_the_last_initializer_status() {
    let temp = TempDir::new("named-import-shared-status");
    temp.script(
        "dependency.fsh",
        &format!(
            "let ready = true\nexport {{ ready }}\n^{} exit 7\n",
            status_fixture(),
        ),
    );
    let script = temp.script(
        "main.fsh",
        "import { ready } from './dependency.fsh'\nexit\n",
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn imported_syntax_diagnostics_name_the_imported_source() {
    let temp = TempDir::new("import-syntax");
    let marker = temp.path().join("root-ran.txt");
    let dependency = temp.script("broken.fsh", "let broken = ;\n");
    let script = temp.script(
        "main.fsh",
        &format!(
            "^{} late 0 {} 0\nimport './broken.fsh'\n",
            status_fixture(),
            marker.display()
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(
        stderr.contains("error[FS1000]: expected an expression"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(" --> {}:", dependency.display())),
        "{stderr}"
    );
    assert!(
        !marker.exists(),
        "module analysis must finish before the first root statement executes"
    );
}

#[test]
fn import_cycles_render_each_source_group() {
    let temp = TempDir::new("import-cycle");
    let root = temp.script("a.fsh", "import './b.fsh'\n");
    let middle = temp.script("b.fsh", "import './c.fsh'\n");
    let closing = temp.script("c.fsh", "import './a.fsh'\n");

    let output = run_script(&root, temp.path(), fixture_directory());

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(
        stderr.contains("error[MOD002]: module import cycle"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(" --> {}:", closing.display())),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(" ::: {}:", root.display())),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(" ::: {}:", middle.display())),
        "{stderr}"
    );
}

#[test]
fn script_preserves_empty_space_and_unicode_arguments() {
    let temp = TempDir::new("argv");
    let report = temp.path().join("report.bin");
    let script = temp.script(
        "argv.fsh",
        "flash-e2e-process-observer-fixture '' 'two words' 'Grüße 🌍'\n",
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
            b"flash-e2e-process-observer-fixture".as_slice(),
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
fn generated_64_mib_mixed_pipeline_streams_without_capture_or_deadlock() {
    let temp = TempDir::new("large-mixed-stream");
    let script = temp.script(
        "large-mixed.fsh",
        &format!(
            "^{0} source 67108864 0 | decode bytes | encode bytes | \
             ^{0} sink 67108864 0\n",
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
fn a_closed_mixed_pipeline_reader_stops_the_internal_bridge() {
    let temp = TempDir::new("mixed-broken-pipe");
    let script = temp.script(
        "mixed-broken-pipe.fsh",
        &format!(
            "^{} source 67108864 0 | decode bytes | encode bytes | ^{} exit 0\n",
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
    let script = temp.script("missing.fsh", "^definitely-not-a-flash-command\n");

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("command not found: definitely-not-a-flash-command"),
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

#[test]
fn a_script_joins_a_background_job_before_it_ends() {
    let temp = TempDir::new("script-join");
    let marker = temp.path().join("late.txt");
    let script = temp.script(
        "join.fsh",
        &format!(
            "^{} late 300 {} 0 &\n^{} exit 0\n",
            status_fixture(),
            marker.display(),
            status_fixture(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        marker.exists(),
        "the script must not exit before the job it started has finished"
    );
}

#[test]
fn a_failing_background_job_makes_the_script_exit_nonzero() {
    let temp = TempDir::new("script-join-failure");
    let script = temp.script(
        "fail.fsh",
        &format!(
            "^{} exit 7 &\n^{} exit 0\n",
            status_fixture(),
            status_fixture(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(
        output.status.code(),
        Some(7),
        "the background failure must reach the exit code: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exited with status 7"),
        "the failure must be reported: {stderr}"
    );
}

#[test]
fn an_explicit_mid_script_exit_still_joins() {
    let temp = TempDir::new("script-join-exit");
    let marker = temp.path().join("late.txt");
    let script = temp.script(
        "exit.fsh",
        &format!(
            "^{} late 300 {} 0 &\nexit 0\n",
            status_fixture(),
            marker.display(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(marker.exists(), "an explicit exit must not skip the join");
}

#[test]
fn a_successful_background_job_is_silent_in_a_script() {
    let temp = TempDir::new("script-join-silent");
    let script = temp.script(
        "silent.fsh",
        &format!(
            "^{} exit 0 &\n^{} exit 0\n",
            status_fixture(),
            status_fixture(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "",
        "a script has no prompt boundary and must not narrate successful jobs"
    );
}

#[derive(Debug)]
struct ProcessReport {
    cwd: Vec<u8>,
    value: Vec<u8>,
    argv: Vec<Vec<u8>>,
}

impl ProcessReport {
    fn read(path: &Path) -> Self {
        let bytes = fs::read(path).expect("process report should exist");
        let mut reader = ReportReader::new(&bytes);
        let cwd = reader.field();
        let value = reader.field();
        let _path = reader.field();
        reader.byte();
        let count = reader.u32() as usize;
        let argv = (0..count).map(|_| reader.field()).collect();
        assert!(reader.remaining().is_empty());
        Self { cwd, value, argv }
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
