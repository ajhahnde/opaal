#![forbid(unsafe_code)]
#[path = "support/data_processing.rs"]
mod support;

use opaal_runtime::workflow::{audit_journal, parse_execution_receipt};
use serde_json::{Value, json};
use std::fs;
use std::io::Read as _;
use std::io::Write as _;
use std::process::Stdio;
use support::{ReportDir, success};

fn project(result: &str, body: &str, grant: bool) -> (ReportDir, String) {
    let work = ReportDir::new();
    work.write("opaal.toml", "schema_version = 1\n[project]\nname = 'native_check'\nroot_module = 'tasks.opaal'\nrequired_opaal = '>=1.3.0,<2.0.0'\n[paths]\nroot = '.'\nevidence = 'output'\n[environments.ci]\nauthority = 'authority.toml'\ntool_lock = 'tools.toml'\n");
    work.write("authority.toml", format!("schema_version = 1\nproject = 'native_check'\nenvironment = 'ci'\n{}", if grant {
        ["stdin.read", "stdout.write"].map(|effect| format!("[[rules]]\ndecision = 'grant'\neffect = '{effect}'\nscope = 'evaluation'\nrequired_enforcement = 'enforced'\n")).join("")
    } else { "rules = []\n".to_owned() }));
    let platform = if cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    work.write("tools.toml", format!("schema_version = 1\nproject = 'native_check'\nenvironment = 'ci'\nplatform = '{platform}'\ntools = []\n[child_environment]\ninherit = []\n"));
    work.write("tasks.opaal", format!("import std::io as io\nimport std::data as data\naction run() -> {result} effects {{ stdin.read(); stdout.write(); }} {{ {body} }}\ntask sample = run\n"));
    let planned = work.run(&[
        "plan",
        "--project",
        "opaal.toml",
        "--task",
        "sample",
        "--environment",
        "ci",
        "--expires-in",
        "900s",
        "--out",
        "plan.json",
        "--format",
        "digest",
    ]);
    if grant {
        success(&planned);
    } else {
        assert_eq!(planned.status.code(), Some(1));
    }
    let plan: Value = serde_json::from_slice(&fs::read(work.0.join("plan.json")).unwrap()).unwrap();
    (work, plan["digest"].as_str().unwrap().to_owned())
}

fn execute(work: &ReportDir, digest: &str, selected: bool, input: &[u8]) -> std::process::Output {
    let mut command = work.command();
    command.args([
        "execute",
        "--plan",
        "plan.json",
        "--accept",
        digest,
        "--journal",
        "run.jsonl",
        "--receipt-out",
        "receipt.json",
    ]);
    if selected {
        command.arg("--require-true");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn native_selection_observes_typed_bool_and_preserves_execution_primary() {
    let inventory: Value = serde_json::from_str(include_str!(
        "../../../tests/golden/native-ci-outcomes/cases.json"
    ))
    .unwrap();
    let cases = inventory["cases"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["id"].as_str().unwrap();
        let body = case["body"].as_str().unwrap();
        let exit = i32::try_from(case["exit"].as_i64().unwrap()).unwrap();
        let primary = case["primary"].as_str().unwrap();
        let assessment = case["assessment"].as_bool();
        let (work, digest) = project("Bool", body, true);
        let output = execute(&work, &digest, true, b"");
        assert_eq!(output.status.code(), Some(exit), "{name}: {output:?}");
        assert_eq!(output.stdout, case["stdout"].as_str().unwrap().as_bytes());
        assert!(output.stderr.is_empty(), "{name}: {output:?}");
        let receipt =
            parse_execution_receipt(&fs::read(work.0.join("receipt.json")).unwrap()).unwrap();
        let audit = audit_journal(&fs::read(work.0.join("run.jsonl")).unwrap()).unwrap();
        assert_eq!(receipt["schema"], "opaal.execution-receipt.v2");
        assert_eq!(audit.value()["schema"], "opaal.audit.v4");
        assert_eq!(receipt["plan_digest"], digest);
        assert_eq!(receipt["run_id"], audit.value()["run_id"]);
        assert_eq!(receipt["primary"]["class"], primary);
        assert_eq!(receipt["primary"], audit.value()["primary"]);
        assert_eq!(receipt["result_policy"], "require-true");
        assert_eq!(
            receipt["assessment"],
            assessment.map_or(Value::Null, |value| json!({"kind":"bool", "value":value}))
        );
        assert_eq!(receipt["assessment"], audit.value()["assessment"]);
        assert_eq!(receipt["secondary"], json!([]));
    }
}

#[test]
fn ordinary_false_keeps_success_and_no_assessment() {
    let (work, digest) = project("Bool", "return false", true);
    let output = execute(&work, &digest, false, b"");
    success(&output);
    let receipt = parse_execution_receipt(&fs::read(work.0.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["result_policy"], "execution");
    assert_eq!(receipt["assessment"], Value::Null);
}

#[test]
fn signature_receipt_stale_and_refused_preflight_have_no_task_effects() {
    for (result, body) in [
        ("Null", "return null"),
        ("Record", "return ({passed:true})"),
        ("String", "return 'true'"),
    ] {
        let (work, digest) = project(result, body, true);
        let output = execute(&work, &digest, true, b"");
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(!work.0.join("run.jsonl").exists());
        assert!(!work.0.join("receipt.json").exists());
    }
    for mode in [
        "missing-receipt",
        "existing-receipt",
        "stale",
        "refused",
        "duplicate",
    ] {
        let (work, digest) = project(
            "Bool",
            "io::print('effect'); return true",
            mode != "refused",
        );
        let mut args = vec![
            "execute",
            "--plan",
            "plan.json",
            "--accept",
            &digest,
            "--journal",
            "run.jsonl",
            "--require-true",
        ];
        if mode != "missing-receipt" {
            args.extend(["--receipt-out", "receipt.json"]);
        }
        if mode == "existing-receipt" {
            work.write("receipt.json", "keep");
        }
        if mode == "stale" {
            work.write(
                "tasks.opaal",
                "action run() -> Bool { return false }\ntask sample = run\n",
            );
        }
        if mode == "duplicate" {
            args.push("--require-true");
        }
        let output = work.run(&args);
        assert_eq!(output.status.code(), Some(2), "{mode}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(!work.0.join("run.jsonl").exists());
        if mode == "existing-receipt" {
            assert_eq!(fs::read(work.0.join("receipt.json")).unwrap(), b"keep");
        }
    }
}

#[test]
fn broken_output_cannot_pass_even_when_its_error_is_caught() {
    for caught in [false, true] {
        let body = if caught {
            "let input = io::read_stdin(1); try { io::print('report') } catch error { }; return true"
        } else {
            "let input = io::read_stdin(1); io::print('report'); return true"
        };
        let (work, digest) = project("Bool", body, true);
        let mut child = work
            .command()
            .args([
                "execute",
                "--plan",
                "plan.json",
                "--accept",
                &digest,
                "--journal",
                "run.jsonl",
                "--receipt-out",
                "receipt.json",
                "--require-true",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let receipt =
            parse_execution_receipt(&fs::read(work.0.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(
            receipt["primary"]["class"],
            if caught { "success" } else { "error" }
        );
        assert_eq!(
            receipt["assessment"],
            if caught {
                json!({"kind":"bool", "value":true})
            } else {
                Value::Null
            }
        );
        assert_eq!(
            receipt["secondary"],
            json!([{"category":"evidence", "code":"EXECUTE_OUTPUT"}])
        );
    }
}

#[test]
fn requested_cancellation_of_blocked_input_retains_130_and_typed_reason() {
    let (work, digest) = project(
        "Bool",
        "io::print('ready'); let input = io::read_stdin(1); return true",
        true,
    );
    let mut child = work
        .command()
        .args([
            "execute",
            "--plan",
            "plan.json",
            "--accept",
            &digest,
            "--journal",
            "run.jsonl",
            "--receipt-out",
            "receipt.json",
            "--require-true",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut marker = [0; 5];
    stdout.read_exact(&mut marker).unwrap();
    assert_eq!(&marker, b"ready");
    let signal = std::process::Command::new("/bin/kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(signal.success());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let receipt = parse_execution_receipt(&fs::read(work.0.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["primary"]["class"], "cancelled");
    assert_eq!(receipt["primary"]["cancel_reason"], "requested");
    assert_eq!(receipt["assessment"], Value::Null);
    let audit = audit_journal(&fs::read(work.0.join("run.jsonl")).unwrap()).unwrap();
    assert_eq!(receipt["primary"], audit.value()["primary"]);
}
