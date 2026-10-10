#![cfg(any(target_os = "macos", target_os = "linux"))]
#![forbid(unsafe_code)]
#[path = "support/data_processing.rs"]
mod support;

use std::io::Write;
use std::process::{Output, Stdio};
use support::{ReportDir, failure};

#[test]
fn packaged_processor_and_ci_job_check_references_use_public_entry_points() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let work = ReportDir::new();
    let target = if cfg!(target_os = "macos") {
        "macos-arm64"
    } else {
        "linux-x86_64"
    };
    let result = std::process::Command::new("python3")
        .current_dir(&root)
        .args([
            "-c",
            "import sys; from pathlib import Path; from ci.package_standard_input_output import check_copies; from ci.qualify_standard_input_output import references; root=Path.cwd(); inputs=root/'tests/golden/standard-input-output'; check_copies(root, inputs); references(Path(sys.argv[1]).resolve(), inputs, Path(sys.argv[2]), sys.argv[3])",
            support::OPAAL,
            work.0.to_str().unwrap(),
            target,
        ])
        .output()
        .unwrap();
    support::success(&result);
}

fn controlled(work: &ReportDir, body: &str) -> String {
    work.write("opaal.toml", "schema_version = 1\n[project]\nname = 'stream_processor'\nroot_module = 'tasks.opaal'\nrequired_opaal = '>=1.2.0,<2.0.0'\n[paths]\nroot = '.'\nevidence = 'output'\n[environments.ci]\nauthority = 'authority.toml'\ntool_lock = 'tools.toml'\n");
    work.write("authority.toml", format!("schema_version = 1\nproject = 'stream_processor'\nenvironment = 'ci'\n{}", ["entropy.system", "stdin.read", "stdout.write", "stderr.write"].map(|effect| format!("[[rules]]\ndecision = 'grant'\neffect = '{effect}'\nscope = 'evaluation'\nrequired_enforcement = 'enforced'\n")).join("")));
    let platform = if cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    work.write("tools.toml", format!("schema_version = 1\nproject = 'stream_processor'\nenvironment = 'ci'\nplatform = '{platform}'\ntools = []\n[child_environment]\ninherit = []\n"));
    work.write("tasks.opaal", format!("import std::io as io\nimport std::random as random\nimport std::string as string\naction process() -> Null effects {{ entropy.system(); stdin.read(); stdout.write(); stderr.write(); }} {{ {body} }}\ntask process = process\n"));
    let planned = work.run(&[
        "plan",
        "--project",
        "opaal.toml",
        "--task",
        "process",
        "--environment",
        "ci",
        "--expires-in",
        "900s",
        "--out",
        "plan.json",
    ]);
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work.0.join("plan.json")).unwrap()).unwrap();
    assert_eq!(plan["schema"], "opaal.plan.v3");
    plan["digest"].as_str().unwrap().to_owned()
}

fn controlled_child(work: &ReportDir, digest: &str, extra: &[&str]) -> std::process::Child {
    work.command()
        .args([
            "execute",
            "--plan",
            "plan.json",
            "--accept",
            digest,
            "--run-id",
            "0123456789abcdef0123456789abcdef",
            "--journal",
            "run.jsonl",
        ])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn controlled_binary_streams_keep_receipts_separate_and_total_all_calls() {
    let work = ReportDir::new();
    let digest = controlled(
        &work,
        "let data = io::read_stdin(65536)\nio::write_stdout(data)\nio::print('tail')\nio::write_stderr(data)\nlet entropy = random::bytes(8)\nreturn io::write_stdout(io::read_stdin(0))",
    );
    let mut child = controlled_child(&work, &digest, &["--receipt-out", "receipt.json"]);
    let input = (0..65536).map(|n| n as u8).collect::<Vec<_>>();
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, [&input[..], b"tail"].concat());
    assert_eq!(result.stderr, input);
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work.0.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["schema"], "opaal.execution-receipt.v1");
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["run_id"], "0123456789abcdef0123456789abcdef");
    assert_eq!(receipt["plan_digest"], digest);
    assert_eq!(receipt["primary"]["class"], "success");
    assert!(receipt["primary"]["value_digest"].is_null());
    assert_eq!(receipt["secondary"], serde_json::json!([]));
    assert_eq!(receipt["journal_state"], "complete");
    for (role, count) in [
        ("stdin.read", 65536),
        ("stdout.write", 65540),
        ("stderr.write", 65536),
        ("entropy.system", 8),
    ] {
        assert_eq!(receipt["progress"][role]["confirmed_bytes"], count);
        assert_eq!(receipt["progress"][role]["uncertain_bytes_upper_bound"], 0);
    }
    let journal = std::fs::read(work.0.join("run.jsonl")).unwrap();
    assert!(
        opaal_runtime::workflow::audit_journal(&journal)
            .unwrap()
            .is_complete()
    );
}

#[test]
fn controlled_input_errors_leave_data_sinks_empty_and_retain_consumed_counts() {
    for (input, count) in [(&b"abcde"[..], 5), (&b"\xff"[..], 1)] {
        let work = ReportDir::new();
        let digest = controlled(
            &work,
            "let text = string::decode_utf8(io::read_stdin(4))\nio::print(text)\nreturn io::eprintln('processed')",
        );
        let mut child = controlled_child(&work, &digest, &["--receipt-out", "receipt.json"]);
        child.stdin.take().unwrap().write_all(input).unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(!result.status.success());
        assert!(
            result.stdout.is_empty() && result.stderr.is_empty(),
            "{result:?}"
        );
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(work.0.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(receipt["primary"]["class"], "error");
        assert_eq!(receipt["progress"]["stdin.read"]["confirmed_bytes"], count);
        assert_eq!(receipt["progress"]["stdout.write"]["confirmed_bytes"], 0);
        assert_eq!(receipt["journal_state"], "complete");
    }
}

#[test]
fn controlled_preflight_refuses_before_consuming_input_or_creating_a_journal() {
    use std::io::Read;
    for case in [
        "missing",
        "existing",
        "symlink",
        "alias",
        "journal",
        "secret",
        "stdin-source",
        "stdout-source",
        "stdout-input",
    ] {
        let work = ReportDir::new();
        let digest = controlled(&work, "return io::write_stdout(io::read_stdin(4))");
        let mut extra = vec!["--receipt-out", "receipt.json"];
        match case {
            "missing" => extra.clear(),
            "existing" => work.write("receipt.json", b"sentinel"),
            "symlink" => {
                std::os::unix::fs::symlink("tasks.opaal", work.0.join("receipt.json")).unwrap()
            }
            "alias" => {
                std::fs::hard_link(work.0.join("tasks.opaal"), work.0.join("receipt.json")).unwrap()
            }
            "journal" => extra[1] = "run.jsonl",
            "secret" => extra.extend(["--secret-stdin", "token"]),
            "stdout-input" => work.write("data", b"data-sentinel"),
            _ => {}
        }
        let source_before = std::fs::read(work.0.join("tasks.opaal")).unwrap();
        let mut command = work.command();
        command
            .args([
                "execute",
                "--plan",
                "plan.json",
                "--accept",
                &digest,
                "--journal",
                "run.jsonl",
            ])
            .args(&extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        work.write("input.bin", b"input-sentinel");
        let mut original_input = std::fs::File::open(work.0.join(if case == "stdin-source" {
            "tasks.opaal"
        } else {
            "input.bin"
        }))
        .unwrap();
        command.stdin(original_input.try_clone().unwrap());
        if case == "stdout-source" {
            command.stdout(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(work.0.join("tasks.opaal"))
                    .unwrap(),
            );
        }
        if case == "stdout-input" {
            command.stdout(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(work.0.join("data"))
                    .unwrap(),
            );
            original_input = std::fs::File::open(work.0.join("data")).unwrap();
            command.stdin(original_input.try_clone().unwrap());
        }
        let result = command.output().unwrap();
        assert!(!result.status.success(), "{case}: {result:?}");
        assert!(!work.0.join("run.jsonl").exists(), "{case}");
        let mut unread = Vec::new();
        original_input.read_to_end(&mut unread).unwrap();
        assert_eq!(
            unread,
            if case == "stdout-input" {
                b"data-sentinel".to_vec()
            } else if case == "stdin-source" {
                source_before.clone()
            } else {
                b"input-sentinel".to_vec()
            },
            "{case}"
        );
        assert_eq!(
            std::fs::read(work.0.join("tasks.opaal")).unwrap(),
            source_before
        );
        if case == "existing" {
            assert_eq!(
                std::fs::read(work.0.join("receipt.json")).unwrap(),
                b"sentinel"
            );
        }
    }
}

#[test]
fn controlled_caught_broken_pipe_keeps_stderr_entropy_and_receipt_available() {
    let work = ReportDir::new();
    let digest = controlled(
        &work,
        "let input = io::read_stdin(0)\ntry { io::print('broken') } catch error { io::eprintln('caught') }\nlet entropy = random::bytes(8)\nreturn io::eprintln('available')",
    );
    let mut child = controlled_child(&work, &digest, &["--receipt-out", "receipt.json"]);
    drop(child.stdout.take());
    drop(child.stdin.take());
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stderr, b"caught\navailable\n");
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work.0.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["primary"]["class"], "success");
    assert_eq!(receipt["progress"]["stdout.write"]["confirmed_bytes"], 0);
    assert_eq!(receipt["progress"]["stderr.write"]["confirmed_bytes"], 17);
    assert_eq!(receipt["progress"]["entropy.system"]["confirmed_bytes"], 8);
}

#[test]
fn controlled_binding_refusal_finalizes_without_consuming_or_emitting_data() {
    let work = ReportDir::new();
    let digest = controlled(&work, "return io::write_stdout(io::read_stdin(4))");
    work.write("input.bin", b"input-sentinel");
    let result = work
        .command()
        .args([
            "execute",
            "--plan",
            "plan.json",
            "--accept",
            &digest,
            "--run-id",
            "0123456789abcdef0123456789abcdef",
            "--journal",
            "run.jsonl",
            "--receipt-out",
            "receipt.json",
        ])
        .stdin(
            std::fs::OpenOptions::new()
                .write(true)
                .open(work.0.join("input.bin"))
                .unwrap(),
        )
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty() && result.stderr.is_empty());
    assert_eq!(
        std::fs::read(work.0.join("input.bin")).unwrap(),
        b"input-sentinel"
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work.0.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["primary"]["class"], "refused");
    assert_eq!(receipt["journal_state"], "complete");
    assert_eq!(receipt["secondary"], serde_json::json!([]));
    for role in [
        "entropy.system",
        "stdin.read",
        "stdout.write",
        "stderr.write",
    ] {
        assert_eq!(receipt["progress"][role]["confirmed_bytes"], 0);
        assert_eq!(receipt["progress"][role]["uncertain_bytes_upper_bound"], 0);
    }
    let audit =
        opaal_runtime::workflow::audit_journal(&std::fs::read(work.0.join("run.jsonl")).unwrap())
            .unwrap();
    assert!(audit.is_complete());
    assert_eq!(audit.value()["primary"]["class"], "refused");
}

fn source(work: &ReportDir, body: &str, input: &[u8]) -> Output {
    work.write("source.opaal", format!("import std::io as io\n{body}\n"));
    let mut child = work
        .command()
        .arg("source.opaal")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn source_streams_preserve_binary_bytes_distinct_sinks_and_repeated_eof() {
    let work = ReportDir::new();
    let input = (0..65536).map(|n| n as u8).collect::<Vec<_>>();
    let result = source(
        &work,
        "let data = io::read_stdin(65536)\nio::write_stdout(data)\nio::write_stderr(data)\nio::write_stdout(io::read_stdin(0))",
        &input,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, input);
    assert_eq!(result.stderr, input);
}

#[test]
fn processor_decodes_strictly_and_never_returns_partial_input() {
    let work = ReportDir::new();
    let body = "import std::string as string\nlet text = string::decode_utf8(io::read_stdin(4096))\nio::print(text)\nio::eprintln('processed')";
    for input in [&b""[..], "Grüße\n".as_bytes()] {
        let result = source(&work, body, input);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, input);
        assert_eq!(result.stderr, b"processed\n");
    }
    for input in [vec![255], vec![b'a'; 4097]] {
        let result = source(&work, body, &input);
        failure(&result);
        assert!(result.stdout.is_empty());
        assert!(!result.stderr.is_empty());
        assert!(!result.stderr.ends_with(b"processed\n"));
    }
}

#[test]
fn text_helpers_emit_only_the_requested_lf_and_pure_calls_refuse() {
    let work = ReportDir::new();
    let result = source(
        &work,
        "io::print('a')\nio::println('b')\nio::println('')\nio::eprint('c')\nio::eprintln('d')",
        b"",
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"ab\n\n");
    assert_eq!(result.stderr, b"cd\n");
    let result = source(
        &work,
        "def pure() -> Null { return io::print('unreachable') }\npure()",
        b"",
    );
    failure(&result);
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("refused["));
}

#[test]
fn nonterminal_editor_retains_ownership_of_later_source_lines() {
    let work = ReportDir::new();
    let mut child = work
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"import std::io as io\nio::read_stdin(4096)\nio::print('later-cell')\nexit\n")
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("refused["));
    assert!(String::from_utf8_lossy(&result.stdout).contains("later-cell"));
}

#[test]
fn a_closed_stdout_is_catchable_and_leaves_stderr_and_entropy_available() {
    let work = ReportDir::new();
    work.write("source.opaal", "import std::io as io\nimport std::random as random\nlet input = io::read_stdin(0)\ntry { io::print('broken-pipe') } catch error { io::eprintln('caught') }\nlet entropy = random::bytes(8)\nio::eprintln('still-available')\n");
    let mut child = work
        .command()
        .arg("source.opaal")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the reader before allowing the blocked stdin operation to reach EOF.
    drop(child.stdout.take());
    drop(child.stdin.take());
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stdout.is_empty());
    assert_eq!(result.stderr, b"caught\nstill-available\n");
}
