#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use opaal_cli::project::{CheckProjectRequest, InitProjectRequest, doctor_project, init_project};
use opaal_runtime::module::AnalysisControl;
use serde_json::Value;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "doctor-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        init_project(&InitProjectRequest {
            destination: path.clone(),
            environment: "offline".to_owned(),
            docker: None,
            socket: None,
            qualified_version: None,
        })
        .unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn request(&self) -> CheckProjectRequest {
        CheckProjectRequest::new(
            self.0.join("opaal.toml"),
            "saved".to_owned(),
            "offline".to_owned(),
            vec![
                (
                    "expected_id".to_owned(),
                    format!("sha256:{}", "a".repeat(64)),
                ),
                ("expected_platform".to_owned(), "linux/amd64".to_owned()),
                (
                    "expected_revision".to_owned(),
                    "PRIVATE_CANARY\u{001b}\u{202e}".to_owned(),
                ),
            ],
        )
        .with_json()
    }
    fn view(&self) -> (u8, Value) {
        let run = doctor_project(&self.request(), &AnalysisControl::never()).unwrap();
        assert!(run.output.ends_with(b"\n"));
        assert!(!String::from_utf8_lossy(&run.output).contains("PRIVATE_CANARY"));
        let view: Value = serde_json::from_slice(&run.output).unwrap();
        let keys = view
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            [
                "environment",
                "findings",
                "live_readiness",
                "local_readiness",
                "project_id",
                "schema",
                "schema_version",
                "task_id"
            ]
        );
        for finding in view["findings"].as_array().unwrap() {
            assert_eq!(
                finding
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                [
                    "category",
                    "code",
                    "input_name",
                    "message",
                    "severity",
                    "source",
                    "span"
                ]
            );
            assert!(finding["code"].as_str().unwrap().starts_with("DOCTOR"));
            assert!(finding["source"].is_null());
            assert!(finding["span"].is_null());
        }
        (run.exit_code, view)
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn ready_view_matches_static_check_and_preserves_sources() {
    let project = Project::new();
    let before = fs::read(project.0.join("policy.opaal")).unwrap();
    let (exit, view) = project.view();
    assert_eq!(exit, 0);
    assert_eq!(view["schema"], "opaal.project-doctor.v1");
    assert_eq!(view["schema_version"], 1);
    assert_eq!(view["project_id"], "image_check");
    assert_eq!(view["task_id"], "image_check::saved");
    assert_eq!(view["local_readiness"], "ready");
    assert_eq!(view["live_readiness"], "not-applicable");
    assert_eq!(view["findings"], serde_json::json!([]));
    assert_eq!(fs::read(project.0.join("policy.opaal")).unwrap(), before);
    assert!(!project.0.join("output").exists());
}

#[test]
fn multiple_controls_version_and_host_findings_are_safe() {
    let project = Project::new();
    let manifest = fs::read_to_string(project.0.join("opaal.toml")).unwrap();
    fs::write(
        project.0.join("opaal.toml"),
        manifest.replace(">=1.5.0,<2.0.0", ">=99.0.0"),
    )
    .unwrap();
    fs::write(
        project.0.join("offline-authority.toml"),
        "PRIVATE_CANARY = { malformed\n",
    )
    .unwrap();
    let lock = fs::read_to_string(project.0.join("offline-tools.toml")).unwrap();
    let wrong = if cfg!(target_os = "macos") {
        "x86_64-unknown-linux-gnu"
    } else {
        "aarch64-apple-darwin"
    };
    let current = if cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    fs::write(
        project.0.join("offline-tools.toml"),
        lock.replace(current, wrong),
    )
    .unwrap();
    let (exit, view) = project.view();
    assert_eq!(exit, 2);
    let codes = view["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| finding["code"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(codes.contains(&"DOCTOR001"));
    assert!(codes.contains(&"DOCTOR006"));
    assert!(codes.contains(&"DOCTOR010"));
    assert!(codes.contains(&"DOCTOR002"));
}

#[test]
fn missing_inputs_do_not_hide_denied_authority() {
    let project = Project::new();
    let path = project.0.join("offline-authority.toml");
    fs::write(
        &path,
        fs::read_to_string(&path)
            .unwrap()
            .replace("decision = \"grant\"", "decision = \"deny\"")
            .replace("required_enforcement = \"enforced\"\n", ""),
    )
    .unwrap();
    let run = doctor_project(
        &CheckProjectRequest::new(
            project.0.join("opaal.toml"),
            "saved".to_owned(),
            "offline".to_owned(),
            vec![],
        )
        .with_json(),
        &AnalysisControl::never(),
    )
    .unwrap();
    assert_eq!(run.exit_code, 1);
    let view: Value = serde_json::from_slice(&run.output).unwrap();
    let findings = view["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|finding| finding["category"] == "authority")
    );
    for name in ["expected_id", "expected_platform", "expected_revision"] {
        assert!(findings.iter().any(|finding| finding["input_name"] == name));
    }
}

#[test]
fn missing_task_environment_and_controls_have_distinct_exit_classes() {
    let project = Project::new();
    for (task, environment, code) in [
        ("missing", "offline", "DOCTOR011"),
        ("saved", "missing", "DOCTOR005"),
    ] {
        let run = doctor_project(
            &CheckProjectRequest::new(
                project.0.join("opaal.toml"),
                task.to_owned(),
                environment.to_owned(),
                vec![],
            )
            .with_json(),
            &AnalysisControl::never(),
        )
        .unwrap();
        assert_eq!(run.exit_code, 1);
        assert!(String::from_utf8(run.output).unwrap().contains(code));
    }
    fs::remove_file(project.0.join("offline-tools.toml")).unwrap();
    assert_eq!(project.view().0, 2);
}

#[test]
fn cancelled_analysis_is_not_a_ready_view() {
    let project = Project::new();
    let run = doctor_project(&project.request(), &AnalysisControl::cooperative(|| true)).unwrap();
    assert_eq!(run.exit_code, 130);
    assert!(run.output.is_empty());
}

#[test]
fn cli_library_errors_and_broken_output_return_two_but_findings_return_one() {
    let project = Project::new();
    let command = |task: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_opaal"));
        command.current_dir(&project.0).args([
            "project",
            "doctor",
            "--project",
            "opaal.toml",
            "--task",
            task,
            "--environment",
            "offline",
            "--format",
            "json",
        ]);
        command
    };
    let invalid = command("invalid-name").output().unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(
        String::from_utf8(invalid.stderr)
            .unwrap()
            .contains("DOCTOR004")
    );
    let findings = command("saved").output().unwrap();
    assert_eq!(findings.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<Value>(&findings.stdout).unwrap()["local_readiness"],
        "blocked"
    );
    let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(reader);
    let descriptor: std::os::fd::OwnedFd = writer.into();
    let broken = command("saved")
        .stdout(Stdio::from(descriptor))
        .output()
        .unwrap();
    assert_eq!(broken.status.code(), Some(2));
    assert!(broken.stdout.is_empty());
}

#[test]
fn fifo_controls_and_source_refuse_without_waiting_for_a_writer() {
    for name in [
        "opaal.toml",
        "offline-authority.toml",
        "offline-tools.toml",
        "saved-tasks.opaal",
    ] {
        let project = Project::new();
        let path = project.0.join(name);
        fs::remove_file(&path).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_opaal"))
            .args([
                "project",
                "doctor",
                "--project",
                "opaal.toml",
                "--task",
                "saved",
                "--environment",
                "offline",
                "--format",
                "json",
            ])
            .current_dir(&project.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("doctor blocked opening {name}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{name}");
        let view: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(view["local_readiness"], "blocked");
        assert!(
            view["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["code"] == "DOCTOR001")
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn cli_does_not_read_streamed_stdin_or_execute_initializers() {
    use std::io::Write as _;
    let project = Project::new();
    // An initializer that would throw is still statically valid and never invoked.
    let path = project.0.join("saved-tasks.opaal");
    let source = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{source}\nthrow \"PRIVATE_CANARY\"\n")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .current_dir(&project.0)
        .env_clear()
        .args([
            "project",
            "doctor",
            "--project",
            "opaal.toml",
            "--task",
            "saved",
            "--environment",
            "offline",
            "--input",
            "expected_id=x",
            "--input",
            "expected_platform=x",
            "--input",
            "expected_revision=PRIVATE_CANARY",
            "--format",
            "json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"unclosed PRIVATE_CANARY input").unwrap();
    // Leaving the pipe open would block a frontend that tried to consume stdin.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE_CANARY"));
}
