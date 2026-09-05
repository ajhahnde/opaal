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
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "fsh 1.0.0\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn help_describes_the_script_cli() {
    let output = fsh(&["--help"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("Flash command shell\n"));
    assert!(stdout.contains("  fsh [OPTIONS] SCRIPT [ARGUMENT]...\n"));
    assert!(stdout.contains("Ordered UTF-8 strings exposed to the root module as $args"));
    assert!(stdout.contains("Every operand after SCRIPT belongs to the script"));
    assert!(stdout.contains("--version"));
    assert!(output.stderr.is_empty());
}

#[test]
fn the_help_text_does_not_advertise_the_reserved_capsule_mode() {
    let output = fsh(&["--help"]);

    assert!(output.status.success());
    let rendered = String::from_utf8(output.stdout).expect("help output should be UTF-8");
    assert!(!rendered.contains("async-capsule"));
    assert!(!rendered.contains("async-completion"));
    assert!(!rendered.contains("flash-v2-repl-fixture"));
}

#[test]
fn scripts_use_the_environment_and_short_circuit_at_the_host_boundary() {
    let temp = TempDir::new("environment-short-circuit");
    let marker = temp.path().join("unreached.txt");
    let source = format!(
        "^{} exit ${{env('FLASH_OK')}} || ^{} late 0 {} 9",
        status_fixture(),
        status_fixture(),
        marker.display()
    );
    let script = temp.script("short-circuit.fsh", &source);
    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(script)
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
fn an_empty_script_is_silent_success() {
    let temp = TempDir::new("empty-script");
    let script = temp.script("empty.fsh", "");

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn v2_final_values_and_domain_results_are_silent_successes() {
    let outcome_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language/outcomes/complete");

    for fixture in ["reference.fsh", "domain-error.fsh", "option-none.fsh"] {
        let output = run_script(
            &outcome_root.join(fixture),
            &outcome_root,
            fixture_directory(),
        );
        assert!(output.status.success(), "{fixture}: {output:?}");
        assert!(output.stdout.is_empty(), "{fixture} printed a final value");
        assert!(
            output.stderr.is_empty(),
            "{fixture} reported a domain value"
        );
    }
}

#[test]
fn golden_v2_workflow_formats_checks_and_runs_with_exact_cli_channels() {
    let workflow = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/workflow");
    let foundation = workflow.parent().unwrap();
    let preserved = foundation.join("v2/preserved.fsh");
    let source = foundation.join("v2/source.fsh");
    let workspace = workflow.join("workspace/root.fsh");
    let facade = workflow.join("workspace/support/facade.fsh");

    let formatted = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .args(["format", "--check"])
        .arg(&preserved)
        .arg(&source)
        .arg(&workspace)
        .arg(&facade)
        .output()
        .unwrap();
    assert!(formatted.status.success(), "{formatted:?}");
    assert!(formatted.stdout.is_empty());
    assert!(formatted.stderr.is_empty());

    let checked = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg("check")
        .arg(&source)
        .output()
        .unwrap();
    assert!(checked.status.success(), "{checked:?}");
    assert!(checked.stdout.is_empty());
    assert!(checked.stderr.is_empty());

    let checked_workspace = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg("check")
        .arg(&workspace)
        .output()
        .unwrap();
    assert!(checked_workspace.status.success(), "{checked_workspace:?}");
    assert!(checked_workspace.stdout.is_empty());
    assert!(checked_workspace.stderr.is_empty());

    let executed = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(&source)
        .args(["alpha", "beta", "gamma"])
        .current_dir(source.parent().unwrap())
        .output()
        .unwrap();
    assert!(executed.status.success(), "{executed:?}");
    assert!(executed.stdout.is_empty());
    assert!(executed.stderr.is_empty());

    let executed_workspace = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(&workspace)
        .current_dir(workspace.parent().unwrap())
        .output()
        .unwrap();
    assert!(
        executed_workspace.status.success(),
        "{executed_workspace:?}"
    );
    assert!(executed_workspace.stdout.is_empty());
    assert!(executed_workspace.stderr.is_empty());
}

#[test]
fn script_roots_reject_directories_and_special_files_before_source_reads() {
    let temp = TempDir::new("script-special-files");
    let directory = temp.path().join("directory.fsh");
    fs::create_dir(&directory).unwrap();
    let special = temp.path().join("fifo.fsh");
    assert!(
        Command::new("mkfifo")
            .arg(&special)
            .status()
            .expect("mkfifo should start")
            .success()
    );

    for source in [&directory, &special] {
        let output = run_script(source, temp.path(), fixture_directory());
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("not a regular file")
        );
    }
}

#[test]
fn v2_standard_outcome_diagnostics_render_from_the_compiled_descriptor() {
    let outcome_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language/outcomes");
    let output = run_script(
        &outcome_root.join("invalid/result-generic-arity.fsh"),
        &outcome_root,
        fixture_directory(),
    );

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[SIG009]"), "{stderr}");
    assert!(stderr.contains("std::outcome"), "{stderr}");
}

#[test]
fn v1_and_v2_completed_program_codes_propagate_exactly_without_diagnostics() {
    let temp = TempDir::new("completed-codes");
    let script = temp.path().join("status.fsh");

    for (language, directive) in [("v1", ""), ("v2", "language 2\n\n")] {
        for code in [0_u8, 1, 2, 125, 126, 127, 128, 255] {
            fs::write(&script, format!("{directive}exit {code}\n"))
                .expect("status script should be written");
            let output = run_script(&script, temp.path(), fixture_directory());

            assert_eq!(
                output.status.code(),
                Some(i32::from(code)),
                "{language}: {output:?}"
            );
            assert!(
                output.stdout.is_empty(),
                "{language} code {code}: {output:?}"
            );
            assert!(
                output.stderr.is_empty(),
                "{language} code {code}: {output:?}"
            );
        }
    }
}

#[test]
fn v2_effectful_process_use_refuses_before_the_process_can_write() {
    let temp = TempDir::new("v2-process-refusal");
    let marker = temp.path().join("must-not-exist.txt");
    let script = temp.script(
        "refused.fsh",
        &format!(
            "language 2\n\n^{} late 0 {} 9\n",
            status_fixture(),
            marker.display()
        ),
    );
    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "fsh: refused[unsupported]: operation `process execution` did not begin\n"
    );
    assert!(!marker.exists(), "a refused v2 process must never start");

    for source in [
        format!("language 2\n\nexit 0 > {}\n", marker.display()),
        format!(
            "language 2\n\nexit $(^{} late 0 {} 9)\n",
            status_fixture(),
            marker.display()
        ),
    ] {
        fs::write(&script, source).expect("refusal script should be replaced");
        let output = run_script(&script, temp.path(), fixture_directory());
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "fsh: refused[unsupported]: operation `process execution` did not begin\n"
        );
        assert!(
            !marker.exists(),
            "effectful host-control syntax must refuse before its effect"
        );
    }
}

#[test]
fn v2_catch_handles_only_language_error_and_uncaught_error_maps_to_one() {
    let temp = TempDir::new("v2-error-catch");
    let caught = temp.script(
        "caught.fsh",
        "language 2\n\ntry {\n    throw \"caught\"\n} catch error {\n    null\n}\n2\n",
    );
    let caught_output = run_script(&caught, temp.path(), fixture_directory());
    assert_eq!(caught_output.status.code(), Some(0), "{caught_output:?}");
    assert!(caught_output.stdout.is_empty());
    assert!(caught_output.stderr.is_empty());

    let uncaught = temp.script("uncaught.fsh", "language 2\n\nthrow \"uncaught\"\n");
    let uncaught_output = run_script(&uncaught, temp.path(), fixture_directory());
    assert_eq!(
        uncaught_output.status.code(),
        Some(1),
        "{uncaught_output:?}"
    );
    assert!(uncaught_output.stdout.is_empty());
    assert!(
        String::from_utf8(uncaught_output.stderr)
            .unwrap()
            .contains("uncaught")
    );
}

#[test]
fn explicit_environment_and_live_status_reads_reach_the_fsh_host_boundary() {
    let temp = TempDir::new("dynamic-session-reads");
    let script = temp.script(
        "dynamic-session-reads.fsh",
        &format!(
            concat!(
                "if env('FLASH_DYNAMIC') == 'present' && $status == null {{\n",
                "    ^{} exit 0\n",
                "}} else {{\n",
                "    exit 91\n",
                "}}\n",
                "if $status.ok && $status.code == 0 && $status.signal == null ",
                "&& $status.stages == [] {{\n",
                "    exit 0\n",
                "}} else {{\n",
                "    exit 92\n",
                "}}\n",
            ),
            status_fixture(),
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(script)
        .current_dir(temp.path())
        .env("PATH", fixture_directory())
        .env("FLASH_DYNAMIC", "present")
        .output()
        .expect("fsh should start");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn recursive_command_composition_reaches_the_fsh_host_boundary() {
    let temp = TempDir::new("recursive-command-composition");
    let report = temp.path().join("report.bin");
    let script = temp.script(
        "recursive-command-composition.fsh",
        &format!(
            concat!(
                "def capture_probe() {{\n",
                "    if ^{0} exit 0 {{\n",
                "        return $(^{1} source 3 0)\n",
                "    }} else {{\n",
                "        return 'unreached'\n",
                "    }}\n",
                "}}\n",
                "let captured = capture_probe()\n",
                "flash-e2e-process-observer-fixture $captured\n",
            ),
            status_fixture(),
            stream_fixture(),
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
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let observed = ProcessReport::read(&report);
    assert_eq!(
        observed.argv,
        [
            b"flash-e2e-process-observer-fixture".as_slice(),
            b"xxx".as_slice(),
        ]
    );
}

#[test]
fn dynamically_selected_external_preserves_argv_through_real_fsh() {
    let temp = TempDir::new("dynamic-external-argv");
    let report = temp.path().join("report.bin");
    let script = temp.script(
        "dynamic-external-argv.fsh",
        concat!(
            "let program = 'flash-e2e-process-observer-fixture'\n",
            "let arguments = ['', 'two words', 'Grüße 🌍']\n",
            "command $program ...$arguments\n",
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
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
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
fn dynamic_external_composes_with_callables_conditions_pipelines_and_capture() {
    let temp = TempDir::new("dynamic-external-composition");
    let report = temp.path().join("report.bin");
    let script = temp.script(
        "dynamic-external-composition.fsh",
        concat!(
            "def succeeds(program) { command $program exit 0 }\n",
            "let status_program = 'flash-e2e-status-fixture'\n",
            "let stream_program = 'flash-e2e-stream-fixture'\n",
            "if succeeds($status_program) {\n",
            "    let captured = $(command $stream_program source 3 0 | decode utf8)\n",
            "    command flash-e2e-process-observer-fixture $captured\n",
            "} else {\n",
            "    exit 99\n",
            "}\n",
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
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(
        ProcessReport::read(&report).argv,
        [
            b"flash-e2e-process-observer-fixture".as_slice(),
            b"xxx".as_slice(),
        ]
    );
}

#[test]
fn typed_byte_capture_preserves_real_process_output_and_nested_status() {
    let temp = TempDir::new("typed-byte-capture");
    let script = temp.script(
        "capture.fsh",
        &format!(
            concat!(
                "let binary = $(bytes: ^{} binary 7)\n",
                "if $status.code != 7 {{ exit 91 }}\n",
                "let expected = $(bytes: ^{} binary 0)\n",
                "if $binary != $expected {{ exit 92 }}\n",
                "let redirected = $(bytes: ^{} binary 0 > payload.bin)\n",
                "let empty = $(bytes: ^{} source 0 0)\n",
                "if $redirected != $empty {{ exit 93 }}\n",
                "open payload.bin | ^{} binary-sink\n",
            ),
            stream_fixture(),
            stream_fixture(),
            stream_fixture(),
            stream_fixture(),
            stream_fixture(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn dynamic_external_preserves_redirection_and_background_execution() {
    let temp = TempDir::new("dynamic-external-effects");
    let redirected = temp.path().join("redirected.bin");
    let marker = temp.path().join("background.txt");
    let script = temp.script(
        "dynamic-external-effects.fsh",
        &format!(
            concat!(
                "command flash-e2e-stream-fixture source 4 0 | \
                 command flash-e2e-stream-fixture sink 4 0\n",
                "command flash-e2e-stream-fixture source 4 0 > '{}'\n",
                "command flash-e2e-status-fixture late 0 '{}' 0 &\n",
                "command flash-e2e-status-fixture exit 0\n",
            ),
            redirected.display(),
            marker.display(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(fs::read(redirected).unwrap(), b"xxxx");
    assert_eq!(fs::read(marker).unwrap(), b"late");
}

#[test]
fn dynamic_external_missing_target_is_a_runtime_resolution_error() {
    let temp = TempDir::new("dynamic-external-missing");
    let marker = temp.path().join("unreached.txt");
    let script = temp.script(
        "dynamic-external-missing.fsh",
        &format!(
            "command flash-e2e-command-that-does-not-exist \
             $(command flash-e2e-status-fixture late 0 '{}' 0)\n",
            marker.display(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(stderr.contains("command not found"), "{stderr}");
    assert!(
        stderr.contains("flash-e2e-command-that-does-not-exist"),
        "{stderr}"
    );
    assert!(
        !marker.exists(),
        "later arguments must not expand after target resolution fails"
    );
}

#[test]
fn explicit_glob_reaches_module_initialization_and_real_argv() {
    let temp = TempDir::new("glob-module-argv");
    fs::create_dir_all(temp.path().join("inputs/nested")).unwrap();
    fs::write(temp.path().join("inputs/a.fsh"), b"").unwrap();
    fs::write(temp.path().join("inputs/nested/b.fsh"), b"").unwrap();
    fs::write(temp.path().join("inputs/ignored.txt"), b"").unwrap();
    let report = temp.path().join("report.bin");
    temp.script(
        "dependency.fsh",
        "let files = glob('inputs/**/*.fsh')\nexport { files }\n",
    );
    let script = temp.script(
        "glob.fsh",
        "import { files } from './dependency.fsh'\n\
         flash-e2e-process-observer-fixture ...$files\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(&script)
        .current_dir(temp.path())
        .env("PATH", fixture_directory())
        .env("FLASH_PROBE_REPORT", &report)
        .output()
        .expect("fsh should start");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let observed = ProcessReport::read(&report);
    assert_eq!(
        observed.argv,
        [
            b"flash-e2e-process-observer-fixture".as_slice(),
            b"inputs/a.fsh".as_slice(),
            b"inputs/nested/b.fsh".as_slice(),
        ]
    );
}

#[test]
fn completed_signal_maps_to_128_plus_its_number_without_a_shell_report() {
    let temp = TempDir::new("completed-signal");
    let script = temp.script("signal.fsh", &format!("^{} signal\n", status_fixture()));

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(134), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn explicit_exit_codes_are_exact_and_silent() {
    let temp = TempDir::new("explicit-exit-codes");
    let script = temp.path().join("exit.fsh");

    for code in [0_u8, 1, 2, 255] {
        fs::write(&script, format!("exit {code}\n")).expect("exit script should be written");
        let output = run_script(&script, temp.path(), fixture_directory());

        assert_eq!(output.status.code(), Some(i32::from(code)), "{output:?}");
        assert!(output.stdout.is_empty(), "code {code}: {output:?}");
        assert!(output.stderr.is_empty(), "code {code}: {output:?}");
    }
}

#[test]
fn default_pipeline_selection_reaches_the_host_boundary_exactly() {
    let temp = TempDir::new("default-pipeline-selection");
    let source = format!("^{0} exit 7 | ^{0} exit 0", status_fixture(),);
    let script = temp.script("pipeline.fsh", &source);

    let default = run_script(&script, temp.path(), fixture_directory());
    assert_eq!(default.status.code(), Some(0), "{default:?}");
    assert!(default.stdout.is_empty());
    assert!(default.stderr.is_empty());
}

#[test]
fn program_bytes_use_stdout_without_shell_text() {
    let temp = TempDir::new("program-output");
    let script = temp.script("output.fsh", "which pwd | get kind | encode utf8\n");

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"internal");
    assert!(output.stderr.is_empty());
}

#[test]
fn parse_import_and_runtime_failures_are_status_one_on_stderr() {
    let temp = TempDir::new("shell-owned-failures");
    let cases = [
        ("parse.fsh", "let broken = ;\n", "error[FS1000]"),
        ("import.fsh", "import './missing.fsh'\n", "error[MOD001]"),
        ("runtime.fsh", "let broken = 1 + true\n", "error[RUN001]"),
    ];

    for (name, source, heading) in cases {
        let script = temp.script(name, source);
        let output = run_script(&script, temp.path(), fixture_directory());

        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        assert!(output.stdout.is_empty(), "{name}: {output:?}");
        let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
        assert!(stderr.starts_with(heading), "{name}: {stderr}");
        assert!(stderr.ends_with('\n'), "{name}: {stderr}");
    }
}

#[test]
fn structured_errors_catch_host_failures_and_preserve_cross_file_frames() {
    let temp = TempDir::new("structured-errors");
    temp.script(
        "dependency.fsh",
        concat!(
            "def fail() {\n",
            "    throw \"module failure\"\n",
            "}\n",
            "export { fail }\n",
        ),
    );
    let script = temp.script(
        "main.fsh",
        concat!(
            "import { fail } from './dependency.fsh'\n",
            "try {\n",
            "    fail()\n",
            "} catch error {\n",
            "    ^/bin/echo \"${$error.category}|${$error.message}|",
            "${$error.source.name}|${$error.frames[0].callee}\"\n",
            "}\n",
            "try {\n",
            "    ^definitely-not-a-flash-command\n",
            "} catch error {\n",
            "    ^/bin/echo \"${$error.category}|${$error.message}\"\n",
            "}\n",
            "try {\n",
            "    ^/bin/sh -c 'exit 7' | check\n",
            "} catch error {\n",
            "    ^/bin/echo \"${$error.category}|${$error.status.code}\"\n",
            "}\n",
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    let stdout = std::str::from_utf8(&output.stdout).expect("caught error output should be UTF-8");
    let mut lines = stdout.lines();
    let module = lines.next().expect("module error line is present");
    assert!(module.starts_with("user|module failure|"), "{module}");
    assert!(module.contains("dependency.fsh|fail"), "{module}");
    let host = lines.next().expect("host error line is present");
    assert!(host.starts_with("command|command not found:"), "{host}");
    assert_eq!(
        lines.next().expect("checked-status error line is present"),
        "control|7"
    );
    assert!(lines.next().is_none(), "{stdout}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn background_failures_report_in_job_order_and_the_first_owns_status() {
    let temp = TempDir::new("ordered-background-failures");
    let script = temp.script(
        "failures.fsh",
        &format!(
            "^{0} exit 7 &\n^{0} exit 9 &\n^{0} exit 0\n",
            status_fixture(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("reports should be UTF-8");
    let first = stderr
        .find("exited with status 7")
        .expect("the first failure should be reported");
    let second = stderr
        .find("exited with status 9")
        .expect("the second failure should be reported");
    assert!(first < second, "{stderr}");
    assert_eq!(stderr.matches("fsh: [").count(), 2, "{stderr}");
    assert!(stderr.ends_with('\n'), "{stderr}");
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
fn a_background_supervisor_executes_a_multi_island_pipeline() {
    let temp = TempDir::new("background-multi-island");
    let marker = temp.path().join("reached.txt");
    let script = temp.script(
        "background-multi-island.fsh",
        &format!(
            "^{0} exit 0 | decode bytes | encode bytes | \
             ^{0} exit 0 | decode bytes | encode bytes | ^{0} exit 0 && \
             ^{0} late 0 {1} 0 &\n^{0} exit 0\n",
            status_fixture(),
            marker.display(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(
        marker.exists(),
        "the background child shell should complete every mixed segment"
    );
}

#[test]
fn a_background_supervisor_executes_snapshotted_values_and_functions() {
    let temp = TempDir::new("background-capsule");
    let marker = temp.path().join("reached.txt");
    let script = temp.script(
        "background-capsule.fsh",
        &format!(
            concat!(
                "let marker = \"{}\"\n",
                "let write = {{|code: Int| ^{} late $code $marker 0}}\n",
                "def finish(code: Int) {{\n",
                "    $write($code)\n",
                "}}\n",
                "^{} exit 7 || finish(0) &\n",
                "^{} exit 0\n",
            ),
            marker.display(),
            status_fixture(),
            status_fixture(),
            status_fixture(),
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(
        marker.exists(),
        "the supervisor should execute the function and its captured marker"
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
    temp.script("dormant.fsh", "export BROKEN = 1\n");
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
fn script_arguments_are_passed_without_reparsing_or_splitting() {
    let temp = TempDir::new("script-arguments");
    let report = temp.path().join("report.bin");
    let script = temp.script(
        "arguments.fsh",
        "^flash-e2e-process-observer-fixture ...$args\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(&script)
        .args(["", "two words", "Grüße 🌍", "--flag"])
        .current_dir(temp.path())
        .env("PATH", fixture_directory())
        .env("FLASH_PROBE_REPORT", &report)
        .output()
        .expect("fsh should start");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let observed = ProcessReport::read(&report);
    assert_eq!(
        observed.argv,
        [
            b"flash-e2e-process-observer-fixture".as_slice(),
            b"".as_slice(),
            b"two words".as_slice(),
            "Grüße 🌍".as_bytes(),
            b"--flag".as_slice(),
        ]
    );
}

#[test]
fn a_non_utf8_script_argument_is_rejected_before_source_loading() {
    let temp = TempDir::new("non-utf8-script-argument");
    let marker = temp.path().join("source-ran.txt");
    let script = temp.script(
        "invalid-argument.fsh",
        &format!("^{} late 0 {} 0\n", status_fixture(), marker.display()),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg(&script)
        .arg(OsString::from_vec(vec![0xff]))
        .current_dir(temp.path())
        .env("PATH", fixture_directory())
        .output()
        .expect("fsh should start");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("CLI diagnostics are UTF-8"),
        "fsh: script arguments must be valid UTF-8\n"
    );
    assert!(
        !marker.exists(),
        "argument decoding must precede source loading"
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
fn external_internal_alternation_round_trips_text() {
    let temp = TempDir::new("alternating-text-stream");
    let input = b"one\ntwo\nthree\n";
    fs::write(temp.path().join("input.txt"), input).expect("text fixture should be written");
    let script = temp.script(
        "alternating-text.fsh",
        &format!(
            "^{0} relay 0 < input.txt | decode utf8 | encode utf8 | \
             ^{0} relay 0 | decode utf8 | encode utf8 | \
             ^{0} relay 0 > output.txt\n",
            stream_fixture()
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(temp.path().join("output.txt")).unwrap(), input);
}

#[test]
fn generated_64_mib_three_island_pipeline_streams_with_bounded_consumption() {
    let temp = TempDir::new("large-three-island-stream");
    let script = temp.script(
        "large-three-island.fsh",
        &format!(
            "^{0} source 67108864 0 | decode bytes | encode bytes | \
             ^{0} relay 0 | decode bytes | encode bytes | \
             ^{0} relay 0 | decode bytes | encode bytes | \
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
fn a_deferred_check_failure_survives_downstream_early_exit() {
    let temp = TempDir::new("deferred-check-early-exit");
    let marker = temp.path().join("unreached.txt");
    let script = temp.script(
        "deferred-check-early-exit.fsh",
        &format!(
            "^{0} source 1048576 7 | check | ^{1} exit 0 && \
             ^{1} late 0 {2} 0\n",
            stream_fixture(),
            status_fixture(),
            marker.display()
        ),
    );

    let output = run_script(&script, temp.path(), fixture_directory());

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(!marker.exists(), "the runtime error must abort the chain");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(
        stderr.contains("checked command was unsuccessful"),
        "{stderr}"
    );
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
