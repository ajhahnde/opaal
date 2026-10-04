#![cfg(any(target_os = "macos", target_os = "linux"))]
#![forbid(unsafe_code)]
#[path = "support/data_processing.rs"]
mod support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use support::{ReportDir, failure, success};

#[test]
fn public_source_uses_the_real_same_image_worker_and_checks_domains() {
    let work = ReportDir::new();
    std::os::unix::fs::symlink("/usr/bin/printf", work.0.join("bin/printf")).unwrap();
    work.write(
        "random.opaal",
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/golden/random-values/source.opaal"),
        )
        .unwrap(),
    );
    success(&work.run(&["check", "random.opaal"]));
    success(&work.run(&["format", "--check", "random.opaal"]));
    let output = work.run(&["random.opaal"]);
    success(&output);
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let parts = text.trim().split(' ').collect::<Vec<_>>();
    assert_eq!(parts.len(), 2);
    let shard: i64 = parts[0].strip_prefix("shard=").unwrap().parse().unwrap();
    let fraction: f64 = parts[1].strip_prefix("fraction=").unwrap().parse().unwrap();
    assert!((0..4).contains(&shard));
    assert!((0.0..1.0).contains(&fraction));
    assert_eq!(
        fraction * 2.0_f64.powi(53),
        (fraction * 2.0_f64.powi(53)).trunc()
    );
}

#[test]
fn domain_errors_pure_wrappers_and_imported_initializers_fail_without_output() {
    let work = ReportDir::new();
    for (body, code) in [
        ("random::int(4, 4)", "RANDOM001"),
        ("random::bytes(-1)", "RANDOM001"),
        ("random::bytes(1048577)", "RANDOM001"),
        (
            "def pure() -> Int { return random::int(7, 8) }\npure()",
            "refused[unsupported]",
        ),
        (
            "let callback = {|| random::bytes(0)}\ncallback()",
            "refused[unsupported]",
        ),
    ] {
        work.write(
            "invalid.opaal",
            format!("import std::random as random\n{body}\n"),
        );
        let output = work.run(&["invalid.opaal"]);
        failure(&output);
        assert!(output.stdout.is_empty());
        assert!(std::str::from_utf8(&output.stderr).unwrap().contains(code));
    }
    work.write(
        "dependency.opaal",
        "import std::random as random\nlet sample = random::bytes(0)\nexport { sample }\n",
    );
    work.write(
        "root.opaal",
        "import './dependency.opaal' as dependency\n42\n",
    );
    failure(&work.run(&["root.opaal"]));
}

#[test]
fn retained_cells_get_fresh_entropy_and_recover_after_domain_errors() {
    let work = ReportDir::new();
    let mut child = work
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"import std::random as random\nlet shard = random::int(0, 4)\nshard\nlet fraction = random::float()\nfraction\nlet identifier = random::bytes(3)\nidentifier\nrandom::int(4, 4)\nrandom::bytes(-1)\nrandom::int(7, 8)\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "{text}");
    let values = lines[..4]
        .iter()
        .zip([">> >> >> ", ">> >> ", ">> >> ", ">> >> >> "])
        .map(|(line, prefix)| line.strip_prefix(prefix).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines[4], ">> ");
    assert!((0..4).contains(&values[0].parse::<i64>().unwrap()));
    assert!((0.0..1.0).contains(&values[1].parse::<f64>().unwrap()));
    let mut encoded = values[2].bytes();
    let mut count = 0;
    while let Some(byte) = encoded.next() {
        if byte == b'\\' {
            match encoded.next().unwrap() {
                b'x' => {
                    assert!(encoded.next().unwrap().is_ascii_hexdigit());
                    assert!(encoded.next().unwrap().is_ascii_hexdigit());
                }
                b'"' | b'\\' => {}
                byte => panic!("unexpected byte escape {byte}"),
            }
        }
        count += 1;
    }
    assert_eq!(count, 3);
    assert_eq!(values[3], "7");
    assert_eq!(
        std::str::from_utf8(&output.stderr)
            .unwrap()
            .matches("RANDOM001")
            .count(),
        2
    );
}
