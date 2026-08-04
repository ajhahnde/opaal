#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[test]
fn lexical_golden_manifest_is_complete_and_readable() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = workspace.join("tests/golden/lexical");
    let manifest = fs::read_to_string(root.join("manifest.tsv"))
        .expect("lexical golden manifest should be readable");

    let mut classes = BTreeSet::new();
    let mut paths = BTreeSet::new();

    for (index, line) in manifest.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line_number = index + 1;
        let fields: Vec<_> = line.splitn(3, '\t').collect();
        assert_eq!(
            fields.len(),
            3,
            "manifest line {line_number} should have class, path, and reason"
        );

        let class = fields[0];
        let relative_path = fields[1];
        let reason = fields[2];
        assert!(
            matches!(class, "complete" | "incomplete" | "invalid"),
            "unknown class on manifest line {line_number}: {class}"
        );
        assert!(
            !reason.trim().is_empty(),
            "manifest line {line_number} should record an expected reason"
        );
        assert!(
            paths.insert(relative_path),
            "duplicate golden path on manifest line {line_number}: {relative_path}"
        );

        let source = fs::read(root.join(relative_path)).unwrap_or_else(|error| {
            panic!("golden source {relative_path} should be readable: {error}")
        });
        assert!(
            !source.is_empty(),
            "golden source {relative_path} should not be empty"
        );
        classes.insert(class);
    }

    assert_eq!(
        classes,
        BTreeSet::from(["complete", "incomplete", "invalid"]),
        "the lexical corpus should cover every completeness class"
    );
}
