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
            && line.starts_with("flash-")
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
        ("flash-syntax", &[]),
        ("flash-migrate", &["flash-syntax"]),
        ("flash-platform", &[]),
        ("flash-runtime", &["flash-platform", "flash-syntax"]),
        ("flash-lsp", &["flash-runtime", "flash-syntax"]),
        ("flash-platform-posix", &["flash-platform"]),
        (
            "flash-cli",
            &[
                // The terminal editor is generic over `P: Platform`, so the
                // client names the capability crate directly instead of
                // reaching it only through an adapter.
                "flash-platform",
                "flash-platform-posix",
                "flash-runtime",
                "flash-syntax",
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
fn migration_tool_has_no_mutation_or_execution_host_api() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source_root = workspace.join("crates/flash-migrate/src");
    let source = ["lib.rs", "main.rs", "scan.rs", "sha256.rs"]
        .into_iter()
        .map(|name| fs::read_to_string(source_root.join(name)).unwrap())
        .collect::<String>();

    for forbidden in [
        "fs::write(",
        "fs::remove_file(",
        "fs::remove_dir",
        "fs::create_dir",
        "fs::rename(",
        "fs::read_dir(",
        "fs::set_permissions(",
        "fs::symlink_metadata(",
        "File::create(",
        "OpenOptions",
        "std::process::Command",
        "Command::new(",
        "env::var(",
        "env::var_os(",
        "env::vars(",
        "env::current_dir(",
        "env::set_current_dir(",
        "env::current_exe(",
        "std::net::",
        "std::thread::",
        "std::time::",
    ] {
        assert!(
            !source.contains(forbidden),
            "migration source must not access `{forbidden}`"
        );
    }

    assert!(source.contains("fs::canonicalize(path)"));
    assert!(source.contains("fs::File::open(path)"));
    assert!(source.contains(".take(limit)"));
    assert!(source.contains(".read_to_end(&mut bytes)"));
    assert!(source.contains("env::args_os().skip(1)"));
}

#[test]
fn downstream_seams_have_no_owner_implementation_or_wire_contract() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = fs::read_to_string(workspace.join("crates/flash-runtime/src/seam.rs"))
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
        "flash_platform",
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
