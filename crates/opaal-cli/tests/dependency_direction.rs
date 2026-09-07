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
            && line.starts_with("opaal-")
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
        ("opaal-syntax", &[]),
        ("opaal-platform", &[]),
        ("opaal-runtime", &["opaal-platform", "opaal-syntax"]),
        ("opaal-lsp", &["opaal-runtime", "opaal-syntax"]),
        ("opaal-platform-posix", &["opaal-platform"]),
        (
            "opaal-cli",
            &[
                // The terminal editor is generic over `P: Platform`, so the
                // client names the capability crate directly instead of
                // reaching it only through an adapter.
                "opaal-platform",
                "opaal-platform-posix",
                "opaal-runtime",
                "opaal-syntax",
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

#[test]
fn downstream_seams_have_no_owner_implementation_or_wire_contract() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = fs::read_to_string(workspace.join("crates/opaal-runtime/src/seam.rs"))
        .expect("the downstream seam owner should be readable");

    for required in [
        "EvaluationContextId",
        "EffectSet",
        "CapabilityRequest",
        "AuthorityVerdict",
        "ResourceOwnerId",
        "CancellationScopeId",
        "Deadline",
        "CleanupOutcome",
        "ActionId",
        "ProjectId",
        "TaskId",
    ] {
        assert!(source.contains(required), "missing typed seam `{required}`");
    }

    for forbidden in [
        "std::env",
        "std::fs",
        "std::net",
        "std::process",
        "std::thread",
        "std::time",
        "opaal_platform",
        "serde",
        "serialize",
        "deserialize",
        "pub fn grant",
        "pub fn execute",
        "pub fn discover",
    ] {
        assert!(
            !source.contains(forbidden),
            "downstream seam source must not implement `{forbidden}`"
        );
    }
}
