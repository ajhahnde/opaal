#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "flash-checker-{label}-{}-{sequence}",
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
fn checker_help_and_usage_have_exact_output_channels_and_statuses() {
    let top_level = fsh(["--help"]);
    assert!(top_level.status.success(), "{top_level:?}");
    assert!(top_level.stderr.is_empty());
    assert!(
        String::from_utf8(top_level.stdout)
            .unwrap()
            .contains("fsh check [--] SOURCE")
    );

    let help = fsh(["check", "--help"]);
    assert!(help.status.success(), "{help:?}");
    assert!(help.stderr.is_empty());
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.starts_with("Analyze Flash source without executing it\n"));
    assert!(stdout.contains("fsh check [--] SOURCE"));
    assert!(stdout.contains("canonical import closure"));
    assert!(stdout.contains("Successful checking is silent"));
    assert!(!stdout.contains("async-chain"));

    let misuse = fsh(["check"]);
    assert_eq!(misuse.status.code(), Some(2), "{misuse:?}");
    assert!(misuse.stdout.is_empty());
    assert_eq!(
        String::from_utf8(misuse.stderr).unwrap(),
        "fsh: check requires exactly one source path\n"
    );
}

#[test]
fn regular_sources_and_canonical_symlink_aliases_check_silently() {
    let temp = TempDir::new("regular-aliases");
    let dependency = temp.source("dependency.fsh", "let answer = 42\n");
    let dependency_alias = temp.path().join("dependency-alias.fsh");
    symlink(&dependency, &dependency_alias).unwrap();
    let root = temp.source(
        "main.fsh",
        concat!(
            "import './dependency.fsh'\n",
            "import './dependency-alias.fsh'\n",
            "echo ready\n",
        ),
    );
    let root_alias = temp.path().join("main-alias.fsh");
    symlink(&root, &root_alias).unwrap();

    for source in [&root, &root_alias] {
        let output = fsh([OsString::from("check"), source.as_os_str().to_owned()]);

        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn diagnostics_are_stderr_only_and_keep_analysis_and_source_order() {
    let temp = TempDir::new("diagnostic-order");
    temp.source("lib.fsh", "let private = 1\nls | ^cat\n");
    let root = temp.source("main.fsh", "import { private } from './lib.fsh'\neach\n");

    let output = fsh([OsString::from("check"), root.as_os_str().to_owned()]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let name = stderr.find("error[MOD007]").unwrap();
    let command = stderr.find("error[CMD003]").unwrap();
    let root_pipeline = stderr.find("error[PIP001]").unwrap();
    let imported_pipeline = stderr.find("error[PIP003]").unwrap();
    assert!(name < command && command < root_pipeline && root_pipeline < imported_pipeline);
    assert!(stderr.contains(" ::: "));
    assert_eq!(stderr.matches("error[").count(), 4);
}

#[test]
fn checker_accepts_structured_error_contracts_and_rejects_invalid_throw_types() {
    let temp = TempDir::new("structured-errors");
    let valid = temp.source(
        "valid.fsh",
        concat!(
            "try { throw \"boom\" } catch error {\n",
            "    let typed: Error = $error\n",
            "}\n",
        ),
    );
    let valid_output = fsh([OsString::from("check"), valid.as_os_str().to_owned()]);
    assert!(valid_output.status.success(), "{valid_output:?}");
    assert!(valid_output.stdout.is_empty());
    assert!(valid_output.stderr.is_empty());

    let invalid = temp.source("invalid.fsh", "throw 42\n");
    let invalid_output = fsh([OsString::from("check"), invalid.as_os_str().to_owned()]);
    assert_eq!(invalid_output.status.code(), Some(1), "{invalid_output:?}");
    assert!(invalid_output.stdout.is_empty());
    let stderr = String::from_utf8(invalid_output.stderr).unwrap();
    assert!(stderr.contains("error[SIG007]"), "{stderr}");
    assert!(
        stderr.contains("throw requires `String` or `Error`"),
        "{stderr}"
    );
}

#[test]
fn directories_and_special_files_are_rejected_as_roots_and_imports() {
    let temp = TempDir::new("unsupported-targets");
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
        let output = fsh([OsString::from("check"), source.as_os_str().to_owned()]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("not a regular file")
        );
    }

    let root = temp.source(
        "main.fsh",
        "import './directory.fsh'\nimport './fifo.fsh'\n",
    );
    let output = fsh([OsString::from("check"), root.as_os_str().to_owned()]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.matches("error[MOD001]").count(), 2);
    assert!(stderr.find("directory.fsh").unwrap() < stderr.find("fifo.fsh").unwrap());
}

#[test]
fn checking_never_runs_initializers_redirections_or_external_commands() {
    let temp = TempDir::new("non-execution");
    let nested = temp.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let redirect = temp.path().join("redirected.txt");
    let marker = temp.path().join("external-marker.txt");
    let process_report = temp.path().join("process-report.bin");
    let root = temp.source(
        "main.fsh",
        format!(
            concat!(
                "export FLASH_PROBE_VALUE = 'checker-mutated'\n",
                "cd './nested'\n",
                "echo changed > '{}'\n",
                "^flash-e2e-status-fixture late 0 '{}' 0\n",
                "let selected = 'flash-e2e-process-observer-fixture'\n",
                "command $selected\n",
            ),
            redirect.display(),
            marker.display(),
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_fsh"))
        .arg("check")
        .arg(&root)
        .current_dir(temp.path())
        .env("PATH", fixture_directory())
        .env("FLASH_PROBE_REPORT", &process_report)
        .env("FLASH_PROBE_VALUE", "original")
        .output()
        .expect("fsh should start");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!redirect.exists(), "redirection must not be opened");
    assert!(!marker.exists(), "the forced external must not execute");
    assert!(
        !process_report.exists(),
        "the checker must not probe or execute an external command"
    );
}
