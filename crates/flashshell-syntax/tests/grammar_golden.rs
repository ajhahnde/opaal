#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const REQUIRED_FAMILIES: [&str; 16] = [
    "background",
    "bindings",
    "calls-and-grouping",
    "closures",
    "commands",
    "conditional-chains",
    "control-flow",
    "control-transfer",
    "environment",
    "functions",
    "literals-and-collections",
    "operators",
    "pipelines",
    "redirections",
    "script-separators",
    "substitution",
];

#[test]
fn grammar_golden_manifest_covers_every_family() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = workspace.join("tests/golden/grammar");
    let manifest = fs::read_to_string(root.join("manifest.tsv"))
        .expect("grammar golden manifest should be readable");

    let mut classes = BTreeSet::new();
    let mut complete_families = BTreeSet::new();
    let mut paths = BTreeSet::new();

    for (index, line) in manifest.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line_number = index + 1;
        let fields: Vec<_> = line.splitn(4, '\t').collect();
        assert_eq!(
            fields.len(),
            4,
            "manifest line {line_number} should have class, family, path, and reason"
        );

        let class = fields[0];
        let family = fields[1];
        let relative_path = fields[2];
        let reason = fields[3];

        assert!(
            matches!(class, "complete" | "incomplete" | "invalid"),
            "unknown class on manifest line {line_number}: {class}"
        );
        assert!(
            REQUIRED_FAMILIES.contains(&family),
            "unknown grammar family on manifest line {line_number}: {family}"
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
        if class == "complete" {
            complete_families.insert(family);
        }
    }

    assert_eq!(
        classes,
        BTreeSet::from(["complete", "incomplete", "invalid"]),
        "the grammar corpus should cover every completeness class"
    );
    assert_eq!(
        complete_families,
        BTreeSet::from(REQUIRED_FAMILIES),
        "the complete grammar corpus should cover every required family"
    );
}
