#![cfg(any(target_os = "macos", target_os = "linux"))]
#![forbid(unsafe_code)]

#[path = "support/data_processing.rs"]
mod support;

use std::fs;
use std::os::unix::fs::symlink;
use support::{INVALID, ReportDir, VALID, effects, failure, fixture, success};

#[test]
fn controlled_plan_execute_and_audit_match_the_ordinary_report() {
    for name in std::iter::once("jobs").chain(VALID.iter().copied()) {
        let work = ReportDir::new();
        let expected = if name == "jobs" {
            fixture("expected-report.json")
        } else {
            work.write("jobs.json", fixture(&format!("valid/{name}.json")));
            fixture(&format!("valid/{name}.expected.json"))
        };
        let inspect = work.run(&["task", "inspect", "--project", "opaal.toml", "summarize"]);
        success(&inspect);
        let text = String::from_utf8(inspect.stdout).unwrap();
        assert!(
            text.contains("signature summarize(input: Path) -> Record"),
            "{text}"
        );
        assert!(text.contains("filesystem.read project.root"), "{text}");
        assert!(text.contains("filesystem.write project.evidence"), "{text}");
        success(&work.run(&[
            "check",
            "--project",
            "opaal.toml",
            "--task",
            "summarize",
            "--environment",
            "ci",
            "--input-file",
            "input=jobs.json",
        ]));
        let plan = work.plan();
        assert!(plan.value()["tools"].as_array().unwrap().is_empty());
        assert!(plan.value()["secrets"].as_array().unwrap().is_empty());
        let planned_effects = plan.value()["authority"]["requests"].as_array().unwrap();
        assert_eq!(planned_effects.len(), 2);
        success(&work.run(&["plan", "inspect", "report.plan.json"]));
        let ordinary = work.run(&["json-report.opaal"]);
        success(&ordinary);
        assert_eq!(ordinary.stdout, expected);
        // Remove every ambient executable before controlled execution.
        fs::remove_file(work.0.join("bin/cat")).unwrap();
        let run = work.execute(plan.digest(), "report.run.jsonl");
        success(&run);
        assert_eq!(fs::read(work.0.join("report.json")).unwrap(), expected);
        let audit = work.audit("report.run.jsonl");
        assert!(audit.is_complete());
        assert_eq!(audit.value()["primary"]["class"], "success");
        assert_eq!(audit.value()["primary"]["partial"], false);
        assert_eq!(effects(&audit), ["filesystem.read", "filesystem.write"]);
        let events = audit.value()["events"].as_array().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["kind"] == "action-start")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["kind"] == "effect-after")
                .count(),
            2
        );
        success(&work.run(&[
            "audit",
            "--project",
            "opaal.toml",
            "--journal",
            "report.run.jsonl",
            "--out",
            "report.audit.json",
        ]));
        let artifact = opaal_runtime::workflow::AuditArtifact::parse(
            &fs::read(work.0.join("report.audit.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(artifact.value()["primary"], audit.value()["primary"]);
        success(&work.run(&["audit", "inspect", "report.audit.json"]));
    }
}

#[test]
fn malformed_data_is_not_parsed_by_check_or_plan_and_execution_retains_read_evidence() {
    for name in INVALID {
        let work = ReportDir::new();
        work.write("jobs.json", fixture(&format!("invalid/{name}.json")));
        success(&work.run(&[
            "check",
            "--project",
            "opaal.toml",
            "--task",
            "summarize",
            "--environment",
            "ci",
            "--input-file",
            "input=jobs.json",
        ]));
        let plan = work.plan();
        let run = work.execute(plan.digest(), "invalid.run.jsonl");
        failure(&run);
        assert!(!work.0.join("report.json").exists());
        let audit = work.audit("invalid.run.jsonl");
        assert!(audit.is_complete(), "{name}");
        assert_eq!(audit.value()["primary"]["class"], "error", "{name}");
        assert_eq!(audit.value()["primary"]["partial"], true, "{name}");
        assert_eq!(effects(&audit), ["filesystem.read"], "{name}");
        let after = audit.value()["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["kind"] == "effect-after")
            .unwrap();
        assert_eq!(after["payload"]["outcome"]["class"], "success", "{name}");
        assert_eq!(
            after["payload"]["operation"]["bytes"],
            fixture(&format!("invalid/{name}.json")).len()
        );
    }
}

#[test]
fn changed_input_source_or_symlink_refuses_before_a_journal_or_effect() {
    for change in ["input", "source", "symlink"] {
        let work = ReportDir::new();
        let plan = work.plan();
        match change {
            "input" => work.write("jobs.json", fixture("valid/empty.json")),
            "source" => {
                let mut source = fixture("report.opaal");
                source.push(b'\n');
                work.write("report.opaal", source);
            }
            "symlink" => {
                fs::rename(work.0.join("jobs.json"), work.0.join("substituted.json")).unwrap();
                symlink("substituted.json", work.0.join("jobs.json")).unwrap();
            }
            _ => unreachable!(),
        }
        let output = work.execute(plan.digest(), "stale.run.jsonl");
        failure(&output);
        assert!(!output.stderr.is_empty(), "{change}");
        assert!(!work.0.join("stale.run.jsonl").exists(), "{change}");
        assert!(!work.0.join("report.json").exists(), "{change}");
    }
}

#[test]
fn denied_write_and_wrong_acceptance_do_not_start_the_task() {
    let work = ReportDir::new();
    let authority = String::from_utf8(fixture("authority.toml")).unwrap()
        .replace("decision = \"grant\"\neffect = \"filesystem.write\"\nscope = \"project.evidence\"\nrequired_enforcement = \"enforced\"", "decision = \"deny\"\neffect = \"filesystem.write\"\nscope = \"project.evidence\"");
    work.write("authority.toml", authority);
    let check = work.run(&[
        "check",
        "--project",
        "opaal.toml",
        "--task",
        "summarize",
        "--environment",
        "ci",
        "--input-file",
        "input=jobs.json",
    ]);
    failure(&check);
    assert!(
        String::from_utf8_lossy(&check.stderr).contains("CHECK005"),
        "{check:?}"
    );
    let refused = work.run(&[
        "plan",
        "--project",
        "opaal.toml",
        "--task",
        "summarize",
        "--environment",
        "ci",
        "--input-file",
        "input=jobs.json",
        "--expires-in",
        "900s",
        "--out",
        "refused.plan.json",
    ]);
    failure(&refused);
    let plan = opaal_runtime::workflow::PlanArtifact::parse(
        &fs::read(work.0.join("refused.plan.json")).unwrap(),
    )
    .unwrap();
    assert!(!plan.is_executable());
    let rules = plan.value()["authority"]["rules"].as_array().unwrap();
    assert_eq!(rules[0]["effect"], "filesystem.read");
    assert_eq!(rules[0]["decision"], "grant");
    assert_eq!(rules[1]["effect"], "filesystem.write");
    assert_eq!(rules[1]["decision"], "deny");
    assert!(!work.0.join("report.json").exists());
    let work = ReportDir::new();
    work.plan();
    let output = work.execute(
        "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "rejected.run.jsonl",
    );
    failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("EXECUTE002"));
    assert!(!work.0.join("rejected.run.jsonl").exists());
    assert!(!work.0.join("report.json").exists());
}

#[test]
fn journal_creation_and_reuse_refuse_before_task_effects() {
    let work = ReportDir::new();
    let plan = work.plan();
    failure(&work.execute(plan.digest(), "missing-parent/report.run.jsonl"));
    assert!(!work.0.join("report.json").exists());
    success(&work.execute(plan.digest(), "report.run.jsonl"));
    let original_journal = fs::read(work.0.join("report.run.jsonl")).unwrap();
    // A sentinel proves the rejected rerun did not write a fresh report.
    work.write("report.json", b"previous-output");
    failure(&work.execute(plan.digest(), "report.run.jsonl"));
    assert_eq!(
        fs::read(work.0.join("report.run.jsonl")).unwrap(),
        original_journal
    );
    assert_eq!(
        fs::read(work.0.join("report.json")).unwrap(),
        b"previous-output"
    );
}

#[test]
fn a_write_target_directory_refuses_during_revalidation() {
    let work = ReportDir::new();
    let plan = work.plan();
    fs::create_dir(work.0.join("report.json")).unwrap();
    let run = work.execute(plan.digest(), "write-failure.run.jsonl");
    failure(&run);
    assert!(!work.0.join("write-failure.run.jsonl").exists());
    assert!(work.0.join("report.json").is_dir());
}

#[test]
fn corrupt_and_future_plans_refuse_without_effects() {
    for change in ["corrupt", "future"] {
        let work = ReportDir::new();
        let plan = work.plan();
        let bytes = if change == "corrupt" {
            b"{".to_vec()
        } else {
            String::from_utf8(plan.bytes().to_vec())
                .unwrap()
                .replace("opaal.plan.v2", "opaal.plan.v3")
                .into_bytes()
        };
        work.write("report.plan.json", bytes);
        failure(&work.execute(plan.digest(), "invalid-plan.run.jsonl"));
        assert!(!work.0.join("invalid-plan.run.jsonl").exists());
        assert!(!work.0.join("report.json").exists());
    }
}

#[path = "support/report_faults.rs"]
mod report_faults;

#[test]
fn controlled_report_faults_preserve_effects_and_partial_outcomes() {
    use opaal_platform::operational::OperationalCall;
    use opaal_runtime::outcome::PrimaryOutcome;
    use report_faults::Fault;
    for fault in [
        Fault::None,
        Fault::Read,
        Fault::WriteBeforeRename,
        Fault::DurabilityAfterRename,
        Fault::JournalAfterWrite,
        Fault::CancelBefore,
        Fault::CancelAfterWrite,
    ] {
        let (outcome, journal, written, calls) = report_faults::run(fault);
        assert!(
            calls.iter().all(|call| matches!(
                call,
                OperationalCall::Read { .. } | OperationalCall::Write { .. }
            )),
            "{fault:?}: {calls:?}"
        );
        match fault {
            Fault::None => {
                assert!(
                    matches!(outcome.primary(), PrimaryOutcome::Completed(_)),
                    "{outcome:?}"
                );
                assert_eq!(written, Some(fixture("expected-report.json")));
                assert_eq!(journal.effects.len(), 2);
                assert!(!journal.ended.unwrap().outcome().partial());
            }
            Fault::CancelBefore => {
                assert!(
                    matches!(outcome.primary(), PrimaryOutcome::Cancelled(_)),
                    "{outcome:?}"
                );
                assert!(calls.is_empty());
                assert!(journal.before.is_empty());
                assert!(written.is_none());
            }
            Fault::CancelAfterWrite => {
                assert!(
                    matches!(outcome.primary(), PrimaryOutcome::Cancelled(_)),
                    "{outcome:?}"
                );
                assert_eq!(written, Some(fixture("expected-report.json")));
                assert_eq!(journal.effects[0].1.class(), "success");
                assert_eq!(journal.effects[1].1.class(), "cancelled");
                assert!(journal.ended.unwrap().outcome().partial());
            }
            Fault::JournalAfterWrite => {
                let PrimaryOutcome::FatalHostFailure(fatal) = outcome.primary() else {
                    panic!("{outcome:?}");
                };
                assert!(fatal.message().contains("JOURNAL005"));
                assert_eq!(written, Some(fixture("expected-report.json")));
                assert_eq!(journal.effects.len(), 1);
                assert_eq!(journal.effects[0].1.class(), "success");
                assert!(outcome.evidence().iter().any(|evidence| matches!(
                    evidence,
                    opaal_runtime::outcome::OutcomeEvidence::PartialEffect(_)
                )));
            }
            _ => {
                assert!(
                    matches!(outcome.primary(), PrimaryOutcome::Error(_)),
                    "{fault:?}: {outcome:?}"
                );
                let ended = journal.ended.unwrap();
                assert!(ended.outcome().partial(), "{fault:?}");
                if fault == Fault::Read {
                    assert!(written.is_none());
                    assert_eq!(journal.effects.len(), 1);
                    assert_eq!(journal.effects[0].1.class(), "error");
                } else {
                    assert_eq!(journal.effects[0].1.class(), "success");
                    if fault == Fault::WriteBeforeRename {
                        assert!(written.is_none());
                    } else {
                        assert_eq!(written, Some(fixture("expected-report.json")));
                    }
                    assert_eq!(journal.effects[1].1.class(), "error");
                    assert!(journal.effects[1].1.partial());
                }
            }
        }
    }
}

#[test]
fn an_expired_plan_refuses_before_task_effects() {
    let work = ReportDir::new();
    let plan = work.plan();
    let mut value = plan.value().clone();
    value.as_object_mut().unwrap().remove("digest");
    value["created_at"] = "2026-01-01T00:00:00.000000000Z".into();
    value["expires_at"] = "2026-01-01T00:15:00.000000000Z".into();
    for observation in value["observations"].as_array_mut().unwrap() {
        if observation["kind"] == "wall-clock" {
            observation["observed_at"] = "2026-01-01T00:00:00.000000000Z".into();
        }
    }
    let expired = opaal_runtime::workflow::PlanArtifact::seal(value).unwrap();
    work.write("report.plan.json", expired.bytes());
    let output = work.execute(expired.digest(), "expired.run.jsonl");
    failure(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("expir"),
        "{output:?}"
    );
    assert!(!work.0.join("expired.run.jsonl").exists());
    assert!(!work.0.join("report.json").exists());
}
