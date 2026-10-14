#![forbid(unsafe_code)]

use std::fs;
use std::io::{self, Write};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use opaal_cli::project::{ExecuteProjectRequest, run_accepted_task};
use opaal_runtime::workflow::{JournalChain, PlanArtifact, audit_journal, parse_execution_receipt};
use serde_json::{Value, json};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "accepted-run-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&parent).unwrap();
        let root = parent.join("project");
        let initialized = Command::new(env!("CARGO_BIN_EXE_opaal"))
            .args([
                "project",
                "init",
                "--example",
                "image-check",
                "--destination",
            ])
            .arg(&root)
            .args(["--environment", "offline"])
            .output()
            .unwrap();
        assert!(initialized.status.success(), "{:?}", initialized);
        Self(root)
    }

    fn command(&self, arguments: &[&str], input: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_opaal"))
            .current_dir(&self.0)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn plan(&self, revision: &str) -> String {
        let result = self.command(&[
            "plan", "--project", "opaal.toml", "--task", "saved", "--environment", "offline",
            "--input", "expected_id=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--input", "expected_platform=linux/amd64", "--input", &format!("expected_revision={revision}"),
            "--expires-in", "900s", "--out", "plan.json", "--format", "digest",
        ], b"");
        assert!(result.status.success(), "{:?}", result);
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    }

    fn run(&self, digest: &str, input: &[u8], selected: bool) -> Output {
        let mut arguments = vec![
            "task",
            "run",
            "--plan",
            "plan.json",
            "--accept",
            digest,
            "--run-id",
            "11111111111111111111111111111111",
            "--journal",
            "run.jsonl",
            "--receipt-out",
            "receipt.json",
            "--audit-out",
            "audit.json",
        ];
        if selected {
            arguments.push("--require-true");
        }
        self.command(&arguments, input)
    }

    fn read(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.0.join(name)).unwrap()).unwrap()
    }

    fn write_receipt(&self, receipt: &Value) {
        let mut bytes = serde_json::to_vec(receipt).unwrap();
        bytes.push(b'\n');
        fs::write(self.0.join("receipt.json"), bytes).unwrap();
    }

    fn explain(&self, audit: &str) -> Output {
        self.command(
            &[
                "task",
                "explain",
                "--receipt",
                "receipt.json",
                "--audit",
                audit,
                "--format",
                "json",
            ],
            b"",
        )
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        fs::remove_dir_all(self.0.parent().unwrap()).unwrap();
    }
}

#[test]
fn native_runs_preserve_report_bytes_and_independently_select_outcomes() {
    for (revision, fixture, selected, exit, conclusion) in [
        ("abc123", "image.json", true, 0, "passed"),
        ("wrong", "image.json", true, 1, "negative"),
        ("abc123", "invalid.json", true, 2, "failed"),
        ("wrong", "image.json", false, 0, "execution-complete"),
    ] {
        let project = Project::new();
        let digest = project.plan(revision);
        let input = fs::read(project.0.join("fixtures").join(fixture)).unwrap();
        let run = project.run(&digest, &input, selected);
        assert_eq!(run.status.code(), Some(exit), "{:?}", run);
        let receipt = project.read("receipt.json");
        let audit = project.read("audit.json");
        assert_eq!(receipt["plan_digest"], digest);
        assert_eq!(audit["plan_digest"], digest);
        assert_eq!(audit["run_id"], receipt["run_id"]);
        assert_eq!(audit["primary"], receipt["primary"]);
        assert_eq!(receipt["journal_state"], "complete");
        assert_eq!(audit["completeness"], "complete");
        assert_eq!(
            receipt["progress"]["stdout.write"]["confirmed_bytes"],
            run.stdout.len()
        );
        for name in ["receipt.json", "audit.json", "run.jsonl"] {
            assert_eq!(
                fs::metadata(project.0.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let explained = project.explain("audit.json");
        assert_eq!(explained.status.code(), Some(0), "{:?}", explained);
        let view: Value = serde_json::from_slice(&explained.stdout).unwrap();
        assert_eq!(view["conclusion"], conclusion);
        assert_eq!(view["progress"], receipt["progress"]);
        assert_eq!(view["partial"], receipt["primary"]["partial"]);
        assert!(
            run.stderr
                .windows(b"conclusion:".len())
                .any(|bytes| bytes == b"conclusion:")
        );
        assert!(explained.stdout.len() <= 64 * 1024 && explained.stdout.ends_with(b"\n"));
        // The old executor remains an independent report-byte and exit observer.
        let legacy = project.command(
            &[
                "execute",
                "--plan",
                "plan.json",
                "--accept",
                &digest,
                "--journal",
                "legacy.jsonl",
                "--receipt-out",
                "legacy.json",
                "--require-true",
            ],
            &input,
        );
        assert_eq!(run.stdout, legacy.stdout);
    }
}

#[test]
fn pure_task_views_compare_the_receipts_metadata_projection_and_allow_discarded_output() {
    for (result_type, value, selected, expected) in [
        ("Bool", "true", true, "passed"),
        ("Bool", "false", true, "negative"),
        ("String", "'value-canary'", false, "execution-complete"),
    ] {
        let project = Project::new();
        fs::write(project.0.join("saved-tasks.opaal"), format!(
            "action saved(expected_id: String, expected_platform: String, expected_revision: String) -> {result_type} effects {{}} {{ return {value} }}\ntask saved = saved\n"
        )).unwrap();
        let digest = project.plan("abc123");
        let run = project.run(&digest, b"", selected);
        assert_eq!(
            run.status.code(),
            Some(if value == "false" { 1 } else { 0 })
        );
        assert!(project.read("receipt.json")["primary"]["value_digest"].is_null());
        assert!(project.read("audit.json")["primary"]["value_digest"].is_string());
        let output = project.explain("audit.json");
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let view: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(view["conclusion"], expected);
        assert!(!String::from_utf8(output.stdout).unwrap().contains("canary"));
        let discarded = Command::new(env!("CARGO_BIN_EXE_opaal"))
            .current_dir(&project.0)
            .args([
                "task",
                "explain",
                "--receipt",
                "receipt.json",
                "--audit",
                "audit.json",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(discarded.success());
    }
}

#[test]
fn occupied_alias_and_symlink_evidence_paths_refuse_before_stdin_or_effects() {
    for occupied in ["run.jsonl", "receipt.json", "audit.json"] {
        for mode in ["file", "hardlink", "symlink", "directory"] {
            let project = Project::new();
            let digest = project.plan("abc123");
            let path = project.0.join(occupied);
            match mode {
                "file" => fs::write(&path, b"existing-canary").unwrap(),
                "hardlink" => fs::hard_link(project.0.join("plan.json"), &path).unwrap(),
                "symlink" => symlink("plan.json", &path).unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                _ => unreachable!(),
            }
            let plan = fs::read(project.0.join("plan.json")).unwrap();
            let run = project.run(&digest, b"", true);
            assert_eq!(run.status.code(), Some(2), "{:?}", run);
            assert!(run.stdout.is_empty());
            assert_eq!(fs::read(project.0.join("plan.json")).unwrap(), plan);
            for other in ["run.jsonl", "receipt.json", "audit.json"] {
                if other != occupied {
                    assert!(!project.0.join(other).exists());
                }
            }
            if mode == "file" {
                assert_eq!(fs::read(path).unwrap(), b"existing-canary");
            }
        }
    }
    for (journal, receipt, audit) in [
        ("same", "same", "audit"),
        ("same", "receipt", "same"),
        ("journal", "same", "same"),
        ("journal", "receipt", "./journal"),
        ("plan.json", "receipt", "audit"),
    ] {
        let project = Project::new();
        let digest = project.plan("abc123");
        let output = project.command(
            &[
                "task",
                "run",
                "--plan",
                "plan.json",
                "--accept",
                &digest,
                "--journal",
                journal,
                "--receipt-out",
                receipt,
                "--audit-out",
                audit,
            ],
            b"",
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn wrong_acceptance_and_stale_inputs_never_run_or_create_evidence() {
    for changed in [
        "acceptance",
        "policy.opaal",
        "opaal.toml",
        "offline-authority.toml",
        "offline-tools.toml",
    ] {
        let project = Project::new();
        let digest = project.plan("abc123");
        if changed != "acceptance" {
            let path = project.0.join(changed);
            let mut bytes = fs::read(&path).unwrap();
            bytes.extend_from_slice(b"\n# changed\n");
            fs::write(path, bytes).unwrap();
        }
        let digest = if changed == "acceptance" {
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        } else {
            &digest
        };
        let run = project.run(digest, b"", true);
        assert_eq!(run.status.code(), Some(2));
        assert!(run.stdout.is_empty());
        for name in ["run.jsonl", "receipt.json", "audit.json"] {
            assert!(!project.0.join(name).exists());
        }
    }
    let project = Project::new();
    project.plan("abc123");
    let mut value = project.read("plan.json");
    value["created_at"] = json!("2026-01-01T00:00:00.000000000Z");
    value["expires_at"] = json!("2026-01-01T00:15:00.000000000Z");
    for observation in value["observations"].as_array_mut().unwrap() {
        if observation["kind"] == "wall-clock" {
            observation["observed_at"] = json!("2026-01-01T00:00:00.000000000Z");
        }
    }
    value.as_object_mut().unwrap().remove("digest");
    let expired = PlanArtifact::seal(value).unwrap();
    fs::write(project.0.join("plan.json"), expired.bytes()).unwrap();
    assert_eq!(
        project.run(expired.digest(), b"", true).status.code(),
        Some(2)
    );
    for name in ["run.jsonl", "receipt.json", "audit.json"] {
        assert!(!project.0.join(name).exists());
    }
}

#[test]
fn explanation_is_passive_safe_and_conservative_for_legacy_or_missing_evidence() {
    let project = Project::new();
    let digest = project.plan("abc123");
    let input = fs::read(project.0.join("fixtures/image.json")).unwrap();
    assert_eq!(project.run(&digest, &input, true).status.code(), Some(0));
    let original = project.read("receipt.json");
    // Source and journal are unnecessary for passive explanation.
    fs::remove_file(project.0.join("policy.opaal")).unwrap();
    fs::remove_file(project.0.join("run.jsonl")).unwrap();
    let missing = project.explain("absent.json");
    assert_eq!(missing.status.code(), Some(0));
    let view: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(view["conclusion"], "unproven");
    for (category, omitted, expected) in [
        ("cleanup", 0, "failed"),
        ("evidence", 0, "failed"),
        ("none", 1, "failed"),
    ] {
        let mut receipt = original.clone();
        receipt["primary"]["code"] = json!("canary_unknown_code");
        if category != "none" {
            receipt["secondary"] = json!([{"category":category,"code":"secondary_canary"}]);
        }
        receipt["omitted_secondary_count"] = json!(omitted);
        project.write_receipt(&receipt);
        let output = project.explain("absent.json");
        assert_eq!(output.status.code(), Some(0));
        let view: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(view["conclusion"], expected);
        assert!(view["primary_code"].is_null());
        assert!(!String::from_utf8(output.stdout).unwrap().contains("canary"));
    }
    let mut legacy = original;
    legacy["schema"] = json!("opaal.execution-receipt.v1");
    legacy["schema_version"] = json!(1);
    for name in ["result_policy", "assessment"] {
        legacy.as_object_mut().unwrap().remove(name);
    }
    legacy["primary"]
        .as_object_mut()
        .unwrap()
        .remove("cancel_reason");
    project.write_receipt(&legacy);
    parse_execution_receipt(&fs::read(project.0.join("receipt.json")).unwrap()).unwrap();
    let output = project.explain("audit.json");
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let view: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["conclusion"], "unproven");
    assert!(view["assessment"].is_null() && view["cancel_reason"].is_null());
}

#[test]
fn corrupt_future_mismatched_and_same_class_partial_artifacts_are_refused() {
    let project = Project::new();
    let digest = project.plan("abc123");
    let input = fs::read(project.0.join("fixtures/image.json")).unwrap();
    assert_eq!(project.run(&digest, &input, true).status.code(), Some(0));
    let receipt = project.read("receipt.json");
    for name in ["schema_version", "run_id", "partial", "progress", "extra"] {
        let mut changed = receipt.clone();
        match name {
            "schema_version" => changed[name] = json!(99),
            "run_id" => changed[name] = json!("22222222222222222222222222222222"),
            "partial" => changed["primary"][name] = json!(true),
            "progress" => changed[name]["stdout.write"]["confirmed_bytes"] = json!(1),
            "extra" => changed[name] = json!(true),
            _ => unreachable!(),
        }
        project.write_receipt(&changed);
        let output = project.explain("audit.json");
        assert_eq!(output.status.code(), Some(2), "{name}: {:?}", output);
        assert!(output.stdout.is_empty());
    }
    project.write_receipt(&receipt);
    fs::write(project.0.join("bad.json"), b"{}").unwrap();
    assert_eq!(project.explain("bad.json").status.code(), Some(2));
    // A validated header/prefix can produce an incomplete audit, never a pass.
    let journal = fs::read(project.0.join("run.jsonl")).unwrap();
    let header: Value =
        serde_json::from_slice(journal.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    let (_, line) = JournalChain::begin(
        header["run_id"].as_str().unwrap(),
        header["payload"].clone(),
    )
    .unwrap();
    let incomplete = audit_journal(&line).unwrap();
    fs::write(project.0.join("prefix.json"), incomplete.bytes()).unwrap();
    let output = project.explain("prefix.json");
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let view: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["audit"], "incomplete");
    assert_eq!(view["conclusion"], "unproven");
    for host in [true, false] {
        let mut payload = header["payload"].clone();
        payload.as_object_mut().unwrap().remove("result_policy");
        if !host {
            payload.as_object_mut().unwrap().remove("standard_host");
        }
        let (_, line) = JournalChain::begin(header["run_id"].as_str().unwrap(), payload).unwrap();
        let legacy = audit_journal(&line).unwrap();
        assert_eq!(legacy.value()["schema_version"], if host { 3 } else { 2 });
        fs::write(project.0.join("legacy-audit.json"), legacy.bytes()).unwrap();
        let output = project.explain("legacy-audit.json");
        assert_eq!(output.status.code(), Some(0));
        let view: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(view["conclusion"], "unproven");
    }
}

#[test]
fn recorded_cancellation_and_secondary_limits_remain_safe_without_audit() {
    let project = Project::new();
    let digest = project.plan("abc123");
    let input = fs::read(project.0.join("fixtures/image.json")).unwrap();
    assert_eq!(project.run(&digest, &input, true).status.code(), Some(0));
    let original = project.read("receipt.json");
    let mut receipt = original.clone();
    receipt["primary"]["class"] = json!("cancelled");
    receipt["primary"]["code"] = json!("EXECUTE_CANCELLED");
    receipt["primary"]["cancel_reason"] = json!("requested");
    receipt["assessment"] = Value::Null;
    receipt["journal_state"] = json!("incomplete");
    receipt["secondary"] = json!([{"category":"journal","code":"JOURNAL005"}]);
    project.write_receipt(&receipt);
    let output = project.explain("absent.json");
    assert_eq!(output.status.code(), Some(0));
    let view: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["conclusion"], "cancelled");
    assert_eq!(view["cancel_reason"], "requested");
    for count in [1_024, 1_025] {
        let mut receipt = original.clone();
        receipt["secondary"] = json!(vec![
            json!({"category":"evidence","code":"EXECUTE_EVIDENCE_OVERFLOW"});
            count
        ]);
        project.write_receipt(&receipt);
        let output = project.explain("absent.json");
        assert_eq!(
            output.status.code(),
            Some(if count == 1_024 { 0 } else { 2 })
        );
        assert!(output.stdout.len() <= 64 * 1024);
        if count == 1_024 {
            let view: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(view["conclusion"], "failed");
        }
    }
}

struct BrokenSummary;
impl Write for BrokenSummary {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("summary-canary"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn summary_failure_prevents_success_and_native_output_paths_keep_their_bytes() {
    let project = Project::new();
    fs::write(project.0.join("saved-tasks.opaal"), "action saved(expected_id: String, expected_platform: String, expected_revision: String) -> Bool effects {} { return true }\ntask saved = saved\n").unwrap();
    let digest = project.plan("abc123");
    #[cfg(target_os = "linux")]
    let native = PathBuf::from(std::ffi::OsString::from_vec(
        b"native-\xff.audit.json".to_vec(),
    ));
    #[cfg(not(target_os = "linux"))]
    let native = PathBuf::from("native-λ.audit.json");
    let request = ExecuteProjectRequest::new(
        project.0.join("plan.json"),
        digest,
        None,
        None,
        project.0.join("run.jsonl"),
    )
    .with_receipt_out(Some(project.0.join("receipt.json")))
    .with_require_true(true);
    let run = run_accepted_task(
        &request,
        &project.0.join(&native),
        false,
        &mut io::empty(),
        &mut BrokenSummary,
        opaal_platform_posix::standard_host::SignalLease::capture().unwrap(),
    );
    assert_eq!(run.exit_code, 2);
    assert!(project.0.join(native).exists());
    assert_eq!(project.read("receipt.json")["primary"]["class"], "success");
}

#[test]
fn new_invocations_require_all_arguments_and_refuse_overrides_or_duplicate_flags() {
    let project = Project::new();
    for arguments in [
        vec!["task", "run"],
        vec![
            "task",
            "run",
            "--plan",
            "x",
            "--accept",
            "x",
            "--journal",
            "x",
            "--receipt-out",
            "x",
        ],
        vec!["task", "run", "--input", "x=y"],
        vec!["task", "run", "--format", "json"],
        vec!["task", "run", "--plan", "x", "--plan", "y"],
        vec!["task", "run", "--audit-out", "x", "--audit-out", "y"],
        vec!["task", "run", "--require-true", "--require-true"],
        vec!["task", "explain"],
        vec![
            "task",
            "explain",
            "--receipt",
            "x",
            "--format",
            "text",
            "--format",
            "text",
        ],
        vec!["task", "explain", "--receipt", "x", "--format", "xml"],
        vec!["task", "explain", "--project", "x"],
    ] {
        assert_eq!(
            project.command(&arguments, b"").status.code(),
            Some(2),
            "{arguments:?}"
        );
    }
    for command in ["run", "explain"] {
        assert_eq!(
            project
                .command(&["task", command, "--help"], b"")
                .status
                .code(),
            Some(0)
        );
    }
    let rejected = project.command(&["task", "run", "argument-canary\u{1b}[31m"], b"");
    assert_eq!(rejected.status.code(), Some(2));
    assert!(
        !String::from_utf8(rejected.stderr)
            .unwrap()
            .contains("canary")
    );
}

#[test]
fn racing_runs_keep_one_exclusive_evidence_owner() {
    let project = Project::new();
    let digest = project.plan("abc123");
    let input = fs::read(project.0.join("fixtures/image.json")).unwrap();
    let start = || {
        Command::new(env!("CARGO_BIN_EXE_opaal"))
            .current_dir(&project.0)
            .args([
                "task",
                "run",
                "--plan",
                "plan.json",
                "--accept",
                &digest,
                "--journal",
                "run.jsonl",
                "--receipt-out",
                "receipt.json",
                "--audit-out",
                "audit.json",
                "--require-true",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut first = start();
    let mut second = start();
    for child in [&mut first, &mut second] {
        // The losing preflight may already have closed stdin.
        let _ = child.stdin.take().unwrap().write_all(&input);
    }
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    let mut exits = [first.status.code().unwrap(), second.status.code().unwrap()];
    exits.sort();
    assert_eq!(exits, [0, 2]);
    assert_eq!(
        project.read("audit.json")["run_id"],
        project.read("receipt.json")["run_id"]
    );
}
