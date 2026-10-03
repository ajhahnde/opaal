#![cfg(any(target_os = "macos", target_os = "linux"))]
#![forbid(unsafe_code)]
#[path = "support/data_processing.rs"]
mod support;
use opaal_runtime::workflow::PlanArtifact;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use support::{ReportDir, effects, failure, success};

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/golden/numeric-operations")
            .join(name),
    )
    .unwrap()
}
fn numeric_work() -> ReportDir {
    let work = ReportDir::new();
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/numeric-report");
    for name in [
        "report.opaal",
        "show.opaal",
        "caught.opaal",
        "domain.opaal",
        "mixed.opaal",
        "tasks.opaal",
        "opaal.toml",
        "authority.toml",
    ] {
        work.write(name, fs::read(examples.join(name)).unwrap());
    }
    work.write(
        "tools.toml",
        fs::read(examples.join(if cfg!(target_os = "macos") {
            "tools-macos.toml"
        } else {
            "tools-linux.toml"
        }))
        .unwrap(),
    );
    std::os::unix::fs::symlink("/usr/bin/printf", work.0.join("bin/printf")).unwrap();
    work
}
fn numeric_plan(work: &ReportDir) -> PlanArtifact {
    let output = work.run(&[
        "plan",
        "--project",
        "opaal.toml",
        "--task",
        "summarize",
        "--environment",
        "ci",
        "--expires-in",
        "900s",
        "--out",
        "report.plan.json",
    ]);
    success(&output);
    let plan = PlanArtifact::parse(&fs::read(work.0.join("report.plan.json")).unwrap()).unwrap();
    assert!(plan.is_executable());
    assert_eq!(
        output.stdout,
        format!("plan {}\n", plan.digest()).as_bytes()
    );
    plan
}

#[test]
fn standalone_examples_check_format_and_match_independent_reference_bytes() {
    let work = numeric_work();
    for (source, expected) in [
        ("show.opaal", "expected-show.txt"),
        ("caught.opaal", "expected-caught.txt"),
    ] {
        success(&work.run(&["check", source]));
        let output = work.run(&[source]);
        success(&output);
        assert_eq!(output.stdout, fixture(expected));
    }
    for source in [
        "report.opaal",
        "show.opaal",
        "caught.opaal",
        "domain.opaal",
        "mixed.opaal",
        "tasks.opaal",
    ] {
        success(&work.run(&["format", "--check", source]));
    }
    for source in ["domain.opaal", "mixed.opaal"] {
        let output = work.run(&[source]);
        failure(&output);
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    failure(&work.run(&["check", "mixed.opaal"]));
    for (args, expected) in [
        (vec![], "expected-report.json"),
        (vec!["--text"], "expected-show.txt"),
        (vec!["--caught"], "expected-caught.txt"),
    ] {
        let output = Command::new("python3")
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tests/golden/numeric-operations/reference.py"),
            )
            .args(args)
            .output()
            .unwrap();
        success(&output);
        assert_eq!(output.stdout, fixture(expected));
    }
}

#[test]
fn interactive_report_and_help_reuse_imports_without_executing_domain_errors() {
    let work = numeric_work();
    let mut child = work
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"import './report.opaal' as report\nimport std::math as math\nhelp math::sqrt\nreport::build().percent\nreport::fallback()\nexit 0\n").unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    assert!(output.stderr.is_empty(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("std::math::sqrt(value: Float) -> Float"),
        "{text}"
    );
    assert!(text.contains("93.0"), "{text}");
    assert!(text.contains("0.0"), "{text}");
}

#[test]
fn controlled_task_preserves_exact_report_and_only_the_declared_write_effect() {
    let work = numeric_work();
    success(&work.run(&["task", "inspect", "--project", "opaal.toml", "summarize"]));
    success(&work.run(&[
        "check",
        "--project",
        "opaal.toml",
        "--task",
        "summarize",
        "--environment",
        "ci",
    ]));
    let plan = numeric_plan(&work);
    assert_eq!(
        plan.value()["authority"]["requests"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(plan.value()["tools"].as_array().unwrap().is_empty());
    assert!(plan.value()["secrets"].as_array().unwrap().is_empty());
    success(&work.run(&["plan", "inspect", "report.plan.json"]));
    fs::remove_file(work.0.join("bin/cat")).unwrap();
    success(&work.execute(plan.digest(), "report.run.jsonl"));
    assert_eq!(
        fs::read(work.0.join("report.json")).unwrap(),
        fixture("expected-report.json")
    );
    let audit = work.audit("report.run.jsonl");
    assert!(audit.is_complete());
    assert_eq!(audit.value()["primary"]["class"], "success");
    assert_eq!(audit.value()["primary"]["partial"], false);
    assert_eq!(effects(&audit), ["filesystem.write"]);
    success(&work.run(&[
        "audit",
        "--project",
        "opaal.toml",
        "--journal",
        "report.run.jsonl",
        "--out",
        "report.audit.json",
    ]));
    success(&work.run(&["audit", "inspect", "report.audit.json"]));
}

#[test]
fn stale_denied_and_failed_output_routes_preserve_existing_refusals() {
    let work = numeric_work();
    let plan = numeric_plan(&work);
    let mut source = fs::read(work.0.join("report.opaal")).unwrap();
    source.push(b'\n');
    work.write("report.opaal", source);
    failure(&work.execute(plan.digest(), "stale.run.jsonl"));
    assert!(!work.0.join("stale.run.jsonl").exists());
    assert!(!work.0.join("report.json").exists());

    let work = numeric_work();
    let authority = fs::read_to_string(work.0.join("authority.toml"))
        .unwrap()
        .replace("decision = \"grant\"", "decision = \"deny\"")
        .replace("required_enforcement = \"enforced\"\n", "");
    work.write("authority.toml", authority);
    failure(&work.run(&[
        "check",
        "--project",
        "opaal.toml",
        "--task",
        "summarize",
        "--environment",
        "ci",
    ]));
    assert!(!work.0.join("report.json").exists());

    for failure_kind in ["write", "journal"] {
        let work = numeric_work();
        let plan = numeric_plan(&work);
        if failure_kind == "write" {
            fs::create_dir(work.0.join("report.json")).unwrap();
        } else {
            work.write("failed.run.jsonl", b"occupied");
        }
        failure(&work.execute(plan.digest(), "failed.run.jsonl"));
        if failure_kind == "write" {
            assert!(work.0.join("report.json").is_dir());
        } else {
            assert_eq!(
                fs::read(work.0.join("failed.run.jsonl")).unwrap(),
                b"occupied"
            );
            assert!(!work.0.join("report.json").exists());
        }
    }
}
