#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn local_dependencies(manifest: &Path) -> BTreeSet<String> {
    let source = fs::read_to_string(manifest).expect("manifest should be readable");
    let mut in_dependencies = false;
    let mut dependencies = BTreeSet::new();

    for line in source.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
        } else if in_dependencies
            && line.starts_with("flashshell-")
            && let Some((name, _)) = line.split_once('=')
        {
            dependencies.insert(name.trim().to_owned());
        }
    }

    dependencies
}

#[test]
fn workspace_crates_follow_the_ratified_dependency_direction() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cases: &[(&str, &[&str])] = &[
        ("flashshell-syntax", &[]),
        ("flashshell-platform", &[]),
        (
            "flashshell-runtime",
            &["flashshell-platform", "flashshell-syntax"],
        ),
        ("flashshell-platform-posix", &["flashshell-platform"]),
        (
            "flashshell-cli",
            &[
                "flashshell-platform-posix",
                "flashshell-runtime",
                "flashshell-syntax",
            ],
        ),
    ];

    for (package, expected) in cases {
        let manifest = workspace.join("crates").join(package).join("Cargo.toml");
        let expected = expected.iter().map(|name| (*name).to_owned()).collect();
        assert_eq!(
            local_dependencies(&manifest),
            expected,
            "unexpected local dependency edge for {package}"
        );
    }
}
