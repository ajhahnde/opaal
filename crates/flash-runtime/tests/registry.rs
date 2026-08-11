//! Acceptance tests for the command registry, minimal command signature, and
//! internal-before-external name resolution.
//!
//! Resolution is pure over an injected registry, environment, and executable
//! probe; no real filesystem, process, or platform is touched.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use flash_runtime::Environment;
use flash_runtime::builtin::standard_registry;
use flash_runtime::command::{
    Carrier, CommandClassification, CommandLifecycle, CommandNamespaceEntry, CommandOutput,
    CommandRegistry, CommandRegistryError, CommandSignature, NamespaceClass,
};
use flash_runtime::resolve::{ExecutableProbe, Resolution, ResolutionError, resolve_command};

struct FakeExecutables(HashSet<OsString>);

impl FakeExecutables {
    fn new<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self(paths.into_iter().map(Into::into).collect())
    }
}

impl ExecutableProbe for FakeExecutables {
    fn is_executable(&self, path: &OsStr) -> bool {
        self.0.contains(path)
    }
}

fn sig(name: &str, inputs: impl IntoIterator<Item = Carrier>, output: Carrier) -> CommandSignature {
    CommandSignature::new(name, inputs, output)
}

fn lifecycle() -> CommandLifecycle {
    CommandLifecycle::introduced(1)
}

/// Extracts the searched name from a `NotFound`, failing on any other outcome.
fn not_found_name(error: ResolutionError) -> OsString {
    match error {
        ResolutionError::NotFound { name } => name,
        other => panic!("expected NotFound, found {other:?}"),
    }
}

#[test]
fn a_signature_exposes_its_name_carriers_and_output() {
    let signature = sig("where", [Carrier::ValueStream], Carrier::ValueStream).with_flags([
        "--reverse",
        "--ignore-case",
        "--reverse",
    ]);

    assert_eq!(signature.name(), "where");
    assert_eq!(
        signature.output(),
        CommandOutput::Fixed(Carrier::ValueStream)
    );
    assert!(signature.accepts(Carrier::ValueStream));
    assert!(!signature.accepts(Carrier::ByteStream));
    assert_eq!(
        signature.flags().collect::<Vec<_>>(),
        ["--ignore-case", "--reverse"]
    );
}

#[test]
fn a_signature_can_accept_more_than_one_input_carrier() {
    let signature = sig(
        "collect",
        [Carrier::ValueStream, Carrier::Value],
        Carrier::Value,
    );

    assert!(signature.accepts(Carrier::Value));
    assert!(signature.accepts(Carrier::ValueStream));
    assert!(!signature.accepts(Carrier::Empty));
}

#[test]
fn a_passthrough_signature_resolves_to_each_actual_input_carrier() {
    let signature =
        CommandSignature::passthrough("check", [Carrier::ByteStream, Carrier::ValueStream]);

    assert_eq!(signature.output(), CommandOutput::SameAsInput);
    assert_eq!(
        signature.output().resolve(Carrier::ByteStream),
        Carrier::ByteStream
    );
    assert_eq!(
        signature.output().resolve(Carrier::ValueStream),
        Carrier::ValueStream
    );
}

#[test]
fn the_registry_looks_up_a_registered_signature() {
    let mut registry = CommandRegistry::new();
    assert!(registry.is_empty());

    assert!(registry.register(sig("pwd", [Carrier::Empty], Carrier::Value)));

    assert!(registry.contains("pwd"));
    assert_eq!(registry.lookup("pwd").expect("registered").name(), "pwd");
    assert!(registry.lookup("cd").is_none());
    assert_eq!(registry.len(), 1);
}

#[test]
fn registering_a_duplicate_name_is_rejected_and_keeps_the_first() {
    let mut registry = CommandRegistry::new();
    assert!(registry.register(sig("cd", [Carrier::Empty], Carrier::Empty)));

    // A second signature for the same name is rejected; the first is kept.
    assert!(!registry.register(sig("cd", [Carrier::Value], Carrier::Value)));

    let kept = registry.lookup("cd").expect("still registered");
    assert_eq!(kept.output(), CommandOutput::Fixed(Carrier::Empty));
    assert!(kept.accepts(Carrier::Empty));
    assert!(!kept.accepts(Carrier::Value));
    assert_eq!(registry.len(), 1);
}

#[test]
fn the_standard_manifest_is_the_exact_v1_core_with_no_aliases_or_reservations() {
    let registry = standard_registry();
    let expected = [
        "bg", "cd", "check", "collect", "command", "decode", "each", "encode", "exit", "fg",
        "first", "from", "get", "help", "jobs", "kill", "last", "length", "lines", "ls", "open",
        "pwd", "save", "select", "sort", "to", "update", "wait", "where", "which",
    ];

    assert_eq!(registry.language_major(), 1);
    assert_eq!(registry.core_names().collect::<Vec<_>>(), expected);
    assert_eq!(
        registry.alias_names().collect::<Vec<_>>(),
        Vec::<&str>::new()
    );
    assert_eq!(
        registry.reserved_names().collect::<Vec<_>>(),
        Vec::<&str>::new()
    );
    assert_eq!(
        registry
            .namespace_entries()
            .map(|entry| (entry.name(), entry.class()))
            .collect::<Vec<_>>(),
        expected
            .into_iter()
            .map(|name| (name, NamespaceClass::Core))
            .collect::<Vec<_>>()
    );
}

#[test]
fn namespace_iteration_and_classification_cover_all_entry_classes() {
    let registry = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::reserved("future", 1, "future command", Some("pwd")),
            CommandNamespaceEntry::alias("cwd", "pwd", lifecycle()),
            CommandNamespaceEntry::core(sig("pwd", [Carrier::Empty], Carrier::Value), lifecycle()),
        ],
    )
    .expect("valid namespace");

    assert_eq!(
        registry
            .namespace_entries()
            .map(|entry| (entry.name(), entry.class()))
            .collect::<Vec<_>>(),
        [
            ("cwd", NamespaceClass::Alias),
            ("future", NamespaceClass::Reserved),
            ("pwd", NamespaceClass::Core),
        ]
    );
    assert!(matches!(
        registry.classify("pwd"),
        CommandClassification::Core { signature, lifecycle }
            if signature.name() == "pwd" && lifecycle.introduced_major() == 1
    ));
    assert!(matches!(
        registry.classify("cwd"),
        CommandClassification::Alias {
            canonical_name: "pwd",
            signature,
            lifecycle,
        } if signature.name() == "pwd" && lifecycle.introduced_major() == 1
    ));
    assert!(matches!(
        registry.classify("future"),
        CommandClassification::Reserved {
            purpose: "future command",
            replacement: Some("pwd"),
            introduced_major: 1,
        }
    ));
    assert!(matches!(
        registry.classify("missing"),
        CommandClassification::Unknown
    ));
}

#[test]
fn aliases_resolve_to_the_canonical_signature_without_copying_it() {
    let registry = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(sig("pwd", [Carrier::Empty], Carrier::Value), lifecycle()),
            CommandNamespaceEntry::alias("cwd", "pwd", lifecycle()),
        ],
    )
    .expect("valid namespace");

    let canonical = registry.lookup("pwd").expect("core signature");
    let alias = registry.lookup("cwd").expect("alias signature");
    assert!(std::ptr::eq(canonical, alias));
    assert_eq!(alias.name(), "pwd");
}

#[test]
fn namespace_construction_rejects_duplicate_empty_and_invalid_alias_entries() {
    let duplicate = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(sig("pwd", [Carrier::Empty], Carrier::Value), lifecycle()),
            CommandNamespaceEntry::reserved("pwd", 1, "tombstone", None),
        ],
    );
    assert!(matches!(
        duplicate,
        Err(CommandRegistryError::DuplicateName { name }) if name == "pwd"
    ));

    for entry in [
        CommandNamespaceEntry::core(sig("", [Carrier::Empty], Carrier::Empty), lifecycle()),
        CommandNamespaceEntry::alias("", "pwd", lifecycle()),
        CommandNamespaceEntry::reserved("", 1, "future command", None),
    ] {
        assert!(matches!(
            CommandRegistry::try_from_entries(1, [entry]),
            Err(CommandRegistryError::EmptyName)
        ));
    }

    let core =
        || CommandNamespaceEntry::core(sig("pwd", [Carrier::Empty], Carrier::Value), lifecycle());
    let missing = CommandRegistry::try_from_entries(
        1,
        [
            core(),
            CommandNamespaceEntry::alias("cwd", "missing", lifecycle()),
        ],
    );
    assert!(matches!(
        missing,
        Err(CommandRegistryError::MissingAliasTarget { name, target })
            if name == "cwd" && target == "missing"
    ));

    let self_alias = CommandRegistry::try_from_entries(
        1,
        [
            core(),
            CommandNamespaceEntry::alias("cwd", "cwd", lifecycle()),
        ],
    );
    assert!(matches!(
        self_alias,
        Err(CommandRegistryError::SelfAlias { name }) if name == "cwd"
    ));

    let chain = CommandRegistry::try_from_entries(
        1,
        [
            core(),
            CommandNamespaceEntry::alias("cwd", "pwd", lifecycle()),
            CommandNamespaceEntry::alias("whereami", "cwd", lifecycle()),
        ],
    );
    assert!(matches!(
        chain,
        Err(CommandRegistryError::AliasTargetNotCore { name, target })
            if name == "whereami" && target == "cwd"
    ));

    let cycle = CommandRegistry::try_from_entries(
        1,
        [
            core(),
            CommandNamespaceEntry::alias("left", "right", lifecycle()),
            CommandNamespaceEntry::alias("right", "left", lifecycle()),
        ],
    );
    assert!(matches!(
        cycle,
        Err(CommandRegistryError::AliasTargetNotCore { .. })
    ));
}

#[test]
fn namespace_construction_validates_lifecycle_and_replacement_metadata() {
    let future = CommandRegistry::try_from_entries(
        1,
        [CommandNamespaceEntry::core(
            sig("pwd", [Carrier::Empty], Carrier::Value),
            CommandLifecycle::introduced(2),
        )],
    );
    assert!(matches!(
        future,
        Err(CommandRegistryError::InvalidIntroducedMajor {
            name,
            introduced_major: 2,
            language_major: 1,
        }) if name == "pwd"
    ));

    let replacement_without_deprecation = CommandRegistry::try_from_entries(
        1,
        [CommandNamespaceEntry::core(
            sig("pwd", [Carrier::Empty], Carrier::Value),
            lifecycle().with_replacement("cd"),
        )],
    );
    assert!(matches!(
        replacement_without_deprecation,
        Err(CommandRegistryError::ReplacementWithoutDeprecation { name }) if name == "pwd"
    ));

    let empty_deprecation = CommandRegistry::try_from_entries(
        1,
        [CommandNamespaceEntry::core(
            sig("pwd", [Carrier::Empty], Carrier::Value),
            lifecycle().deprecated_since(""),
        )],
    );
    assert!(matches!(
        empty_deprecation,
        Err(CommandRegistryError::EmptyDeprecation { name }) if name == "pwd"
    ));

    let invalid_replacement = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(
                sig("pwd", [Carrier::Empty], Carrier::Value),
                lifecycle()
                    .deprecated_since("0.9")
                    .with_replacement("missing"),
            ),
            CommandNamespaceEntry::core(sig("cd", [Carrier::Empty], Carrier::Empty), lifecycle()),
        ],
    );
    assert!(matches!(
        invalid_replacement,
        Err(CommandRegistryError::InvalidReplacementTarget { name, replacement })
            if name == "pwd" && replacement == "missing"
    ));

    let empty_purpose = CommandRegistry::try_from_entries(
        1,
        [CommandNamespaceEntry::reserved("future", 1, "", None)],
    );
    assert!(matches!(
        empty_purpose,
        Err(CommandRegistryError::EmptyReservationPurpose { name }) if name == "future"
    ));
}

#[test]
fn a_bare_name_resolves_to_the_internal_command_before_external() {
    let mut registry = CommandRegistry::new();
    registry.register(sig("git", [Carrier::Empty], Carrier::ByteStream));
    let env = Environment::from_snapshot([("PATH", "/usr/bin")]);
    // An external `git` also exists, but the internal one wins for a bare name.
    let probe = FakeExecutables::new(["/usr/bin/git"]);

    let resolved = resolve_command(OsStr::new("git"), false, &registry, &env, &probe)
        .expect("resolves internal");

    match resolved {
        Resolution::Internal {
            source_name,
            canonical_name,
            signature,
        } => {
            assert_eq!(source_name, "git");
            assert_eq!(canonical_name, "git");
            assert_eq!(signature.name(), "git");
        }
        Resolution::External(other) => panic!("expected internal, found {other:?}"),
    }
}

#[test]
fn an_alias_resolves_with_source_and_canonical_executor_identity() {
    let registry = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(sig("pwd", [Carrier::Empty], Carrier::Value), lifecycle()),
            CommandNamespaceEntry::alias("cwd", "pwd", lifecycle()),
        ],
    )
    .expect("valid namespace");
    let env = Environment::from_snapshot([("PATH", "/usr/bin")]);
    let probe = FakeExecutables::new(["/usr/bin/cwd"]);

    let resolved = resolve_command(OsStr::new("cwd"), false, &registry, &env, &probe)
        .expect("alias resolves internally");

    match resolved {
        Resolution::Internal {
            source_name,
            canonical_name,
            signature,
        } => {
            assert_eq!(source_name, "cwd");
            assert_eq!(canonical_name, "pwd");
            assert_eq!(signature.name(), "pwd");
        }
        Resolution::External(other) => panic!("expected internal alias, found {other:?}"),
    }
}

#[test]
fn a_reserved_bare_name_is_refused_before_path_lookup() {
    let registry = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(sig("pwd", [Carrier::Empty], Carrier::Value), lifecycle()),
            CommandNamespaceEntry::reserved(
                "future",
                1,
                "reserved for a future structured command",
                Some("pwd"),
            ),
        ],
    )
    .expect("valid namespace");
    let env = Environment::from_snapshot([("PATH", "/usr/bin")]);
    let probe = FakeExecutables::new(["/usr/bin/future"]);

    let error = resolve_command(OsStr::new("future"), false, &registry, &env, &probe)
        .expect_err("reservation wins before PATH");

    assert!(matches!(
        error,
        ResolutionError::Reserved {
            name,
            purpose,
            replacement: Some(replacement),
        } if name == "future"
            && purpose == "reserved for a future structured command"
            && replacement == "pwd"
    ));
}

#[test]
fn forced_external_resolution_bypasses_a_reservation() {
    let registry = CommandRegistry::try_from_entries(
        1,
        [CommandNamespaceEntry::reserved(
            "future",
            1,
            "future command",
            None,
        )],
    )
    .expect("valid namespace");
    let env = Environment::from_snapshot([("PATH", "/usr/bin")]);
    let probe = FakeExecutables::new(["/usr/bin/future"]);

    let resolved = resolve_command(OsStr::new("future"), true, &registry, &env, &probe)
        .expect("forced external bypasses reservation");

    match resolved {
        Resolution::External(command) => {
            assert_eq!(command.path(), Path::new("/usr/bin/future"));
        }
        Resolution::Internal { .. } => panic!("forced external resolved internally"),
    }
}

#[test]
fn a_bare_name_missing_from_the_registry_falls_back_to_external() {
    let registry = CommandRegistry::new();
    let env = Environment::from_snapshot([("PATH", "/usr/bin")]);
    let probe = FakeExecutables::new(["/usr/bin/ls"]);

    let resolved =
        resolve_command(OsStr::new("ls"), false, &registry, &env, &probe).expect("resolves");

    match resolved {
        Resolution::External(command) => assert_eq!(command.path(), Path::new("/usr/bin/ls")),
        Resolution::Internal { .. } => panic!("expected external"),
    }
}

#[test]
fn a_bare_name_in_neither_place_is_not_found() {
    let registry = CommandRegistry::new();
    let env = Environment::from_snapshot([("PATH", "/usr/bin")]);
    let probe = FakeExecutables::new(["/usr/bin/other"]);

    let error = resolve_command(OsStr::new("missing"), false, &registry, &env, &probe)
        .expect_err("not found");

    assert_eq!(not_found_name(error), OsString::from("missing"));
}

#[test]
fn an_external_marked_name_skips_the_registry() {
    let mut registry = CommandRegistry::new();
    // An internal `git` is registered, but `^git` must resolve externally.
    registry.register(sig("git", [Carrier::Empty], Carrier::ByteStream));
    let env = Environment::from_snapshot([("PATH", "/usr/bin")]);
    let probe = FakeExecutables::new(["/usr/bin/git"]);

    let resolved = resolve_command(OsStr::new("git"), true, &registry, &env, &probe)
        .expect("resolves external");

    match resolved {
        Resolution::External(command) => assert_eq!(command.path(), Path::new("/usr/bin/git")),
        Resolution::Internal { .. } => panic!("expected external"),
    }
}

#[test]
fn an_external_marked_name_with_only_an_internal_command_is_not_found() {
    let mut registry = CommandRegistry::new();
    registry.register(sig("git", [Carrier::Empty], Carrier::ByteStream));
    let env = Environment::new();
    let probe = FakeExecutables::new(["/anything"]);

    // `^git` never uses the registry, and there is no external git.
    let error =
        resolve_command(OsStr::new("git"), true, &registry, &env, &probe).expect_err("not found");

    assert_eq!(not_found_name(error), OsString::from("git"));
}

#[test]
fn a_non_utf8_name_cannot_be_internal_and_resolves_externally() {
    let raw = OsString::from_vec(vec![b'g', 0xFF, b't']);
    let mut path_value = b"/usr/bin/".to_vec();
    path_value.extend_from_slice(raw.as_bytes());
    let candidate = OsString::from_vec(path_value);

    let mut registry = CommandRegistry::new();
    // A UTF-8 command name can never equal the invalid bytes; the registry is
    // skipped and external resolution runs.
    registry.register(sig("git", [Carrier::Empty], Carrier::ByteStream));
    let env = Environment::from_snapshot([("PATH", "/usr/bin")]);
    let probe = FakeExecutables::new([candidate.clone()]);

    let resolved =
        resolve_command(&raw, false, &registry, &env, &probe).expect("resolves external");

    match resolved {
        Resolution::External(command) => {
            assert_eq!(command.path().as_os_str().as_bytes(), candidate.as_bytes());
        }
        Resolution::Internal { .. } => panic!("expected external"),
    }
}
