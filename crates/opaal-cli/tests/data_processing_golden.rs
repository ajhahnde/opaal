#![cfg(any(target_os = "macos", target_os = "linux"))]
#![forbid(unsafe_code)]

#[path = "support/data_processing.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use support::{INVALID, ReportDir, VALID, failure, fixture, fixtures, success};

#[test]
fn ordinary_reports_have_exact_json_and_escaped_human_bytes() {
    let work = ReportDir::new();
    for (source, expected) in [
        ("json-report.opaal", "expected-report.json"),
        ("text-report.opaal", "expected-report.txt"),
    ] {
        let output = work.run(&[source]);
        success(&output);
        assert_eq!(output.stdout, fixture(expected));
    }
    for name in VALID {
        work.write("jobs.json", fixture(&format!("valid/{name}.json")));
        for (source, extension) in [("json-report.opaal", "json"), ("text-report.opaal", "txt")] {
            let output = work.run(&[source]);
            success(&output);
            assert_eq!(
                output.stdout,
                fixture(&format!("valid/{name}.expected.{extension}")),
                "{name}"
            );
        }
    }
}

#[test]
fn invalid_documents_fail_without_a_report() {
    let work = ReportDir::new();
    for name in INVALID {
        work.write("jobs.json", fixture(&format!("invalid/{name}.json")));
        for source in ["json-report.opaal", "text-report.opaal"] {
            let output = work.run(&[source]);
            failure(&output);
            assert!(output.stdout.is_empty(), "{name}: {output:?}");
            assert!(!output.stderr.is_empty(), "{name}: {output:?}");
        }
    }
}

#[test]
fn absent_input_and_executable_cannot_produce_an_empty_success() {
    let work = ReportDir::new();
    fs::remove_file(work.0.join("jobs.json")).unwrap();
    let missing_input = work.run(&["json-report.opaal"]);
    failure(&missing_input);
    assert!(missing_input.stdout.is_empty());
    work.write("jobs.json", fixture("valid/empty.json"));
    fs::remove_file(work.0.join("bin/cat")).unwrap();
    let missing_tool = work.run(&["json-report.opaal"]);
    failure(&missing_tool);
    assert!(missing_tool.stdout.is_empty());
}

#[test]
fn valid_report_bytes_do_not_prove_successful_acquisition() {
    let work = ReportDir::new();
    // Synthetic producer writes a complete valid document, then exits 7.
    let producer = work.0.join("bin/producer");
    fs::write(&producer, "#!/bin/sh\n/bin/cat jobs.json\nexit 7\n").unwrap();
    fs::set_permissions(&producer, fs::Permissions::from_mode(0o700)).unwrap();
    let source = String::from_utf8(fixture("json-report.opaal"))
        .unwrap()
        .replace("^cat jobs.json", "^producer");
    work.write("failed-producer.opaal", source);
    let output = work.run(&["failed-producer.opaal"]);
    // Default pipelines select the final stage status, not producer success.
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, fixture("expected-report.json"));

    // The documented shell acquisition gate never launches processing after
    // that same unsuccessful export, even though jobs.json is valid.
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg("producer > exported.json && opaal json-report.opaal > acquired-report.json")
        .current_dir(&work.0)
        .env_clear()
        .env("PATH", work.0.join("bin"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        fs::read(work.0.join("exported.json")).unwrap(),
        fixture("jobs.json")
    );
    assert!(!work.0.join("acquired-report.json").exists());
}

#[test]
fn a_closed_output_sink_is_a_failure() {
    let work = ReportDir::new();
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    let output = work
        .command()
        .arg("json-report.opaal")
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .output()
        .unwrap();
    failure(&output);
    assert!(!output.stderr.is_empty(), "{output:?}");
}

#[test]
fn independent_python_reference_agrees_on_values_bytes_and_failures() {
    let root = fixtures();
    for name in std::iter::once("jobs").chain(VALID.iter().copied()) {
        let input = if name == "jobs" {
            root.join("jobs.json")
        } else {
            root.join(format!("valid/{name}.json"))
        };
        for text in [false, true] {
            let mut command = Command::new("python3");
            command.arg(root.join("reference.py")).arg(&input);
            if text {
                command.arg("--text");
            }
            let output = command.output().unwrap();
            success(&output);
            let expected = if name == "jobs" {
                fixture(if text {
                    "expected-report.txt"
                } else {
                    "expected-report.json"
                })
            } else {
                fixture(&format!(
                    "valid/{name}.expected.{}",
                    if text { "txt" } else { "json" }
                ))
            };
            assert_eq!(output.stdout, expected);
        }
    }
    for name in INVALID {
        let output = Command::new("python3")
            .arg(root.join("reference.py"))
            .arg(root.join(format!("invalid/{name}.json")))
            .output()
            .unwrap();
        failure(&output);
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"reference: invalid input\n");
    }
}
