//! Acceptance tests for the command registry, minimal command signature, and
//! internal-before-external name resolution.
//!
//! Resolution is pure over an injected registry, environment, and executable
//! probe; no real filesystem, process, or platform is touched.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use flashshell_runtime::Environment;
use flashshell_runtime::command::{Carrier, CommandOutput, CommandRegistry, CommandSignature};
use flashshell_runtime::resolve::{ExecutableProbe, Resolution, ResolutionError, resolve_command};

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
fn a_bare_name_resolves_to_the_internal_command_before_external() {
    let mut registry = CommandRegistry::new();
    registry.register(sig("git", [Carrier::Empty], Carrier::ByteStream));
    let env = Environment::from_snapshot([("PATH", "/usr/bin")]);
    // An external `git` also exists, but the internal one wins for a bare name.
    let probe = FakeExecutables::new(["/usr/bin/git"]);

    let resolved = resolve_command(OsStr::new("git"), false, &registry, &env, &probe)
        .expect("resolves internal");

    match resolved {
        Resolution::Internal(signature) => assert_eq!(signature.name(), "git"),
        Resolution::External(other) => panic!("expected internal, found {other:?}"),
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
        Resolution::Internal(signature) => panic!("expected external, found {signature:?}"),
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
        Resolution::Internal(signature) => panic!("expected external, found {signature:?}"),
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
        Resolution::Internal(signature) => panic!("expected external, found {signature:?}"),
    }
}
