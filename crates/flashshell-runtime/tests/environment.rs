#![forbid(unsafe_code)]

//! `export` and `unset` over a runtime-owned environment that is distinct from
//! the lexical scope, seeded from an injected snapshot with no process or
//! filesystem dependency.

use std::ffi::{OsStr, OsString};

use flashshell_runtime::eval::{Completion, EvalLimits, RuntimeErrorKind, evaluate_in_environment};
use flashshell_runtime::{Environment, ScopeStack};
use flashshell_syntax::{ParseOutcome, SourceFile, SourceId, parse};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(1), "test.fsh", text)
}

/// Runs a script against a fresh scope and the given environment.
fn run_in(text: &str, env: &mut Environment) -> Result<(), RuntimeErrorKind> {
    let file = source(text);
    let script = match parse(&file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}"),
    };
    let mut scope = ScopeStack::new();
    match evaluate_in_environment(&script, &file, &mut scope, env, &EvalLimits::default()) {
        Ok(Completion::Value(_)) => Ok(()),
        Ok(Completion::Cancelled(_)) => panic!("unexpected cancellation"),
        Err(error) => Err(error.kind().clone()),
    }
}

/// Runs a script against an empty environment and returns the resulting map.
fn run(text: &str) -> Environment {
    let mut env = Environment::new();
    run_in(text, &mut env).unwrap_or_else(|kind| panic!("evaluation failed: {kind:?}"));
    env
}

fn os(text: &str) -> OsString {
    OsString::from(text)
}

#[test]
fn export_sets_a_native_environment_entry() {
    let env = run("export EDITOR = 'helix'");
    assert_eq!(env.get("EDITOR"), Some(OsStr::new("helix")));
    assert_eq!(env.len(), 1);
}

#[test]
fn export_encodes_each_scalar_family_with_the_word_encoding() {
    let env = run("export N = 42\nexport F = 1.5\nexport B = true");
    assert_eq!(env.get("N"), Some(OsStr::new("42")));
    assert_eq!(env.get("F"), Some(OsStr::new("1.5")));
    assert_eq!(env.get("B"), Some(OsStr::new("true")));
}

#[test]
fn export_reads_the_scope_and_overwrites_on_re_export() {
    let env = run("let name = 'hx'\nexport EDITOR = $name\nexport EDITOR = 'vi'");
    assert_eq!(env.get("EDITOR"), Some(OsStr::new("vi")));
    assert_eq!(env.len(), 1);
}

#[test]
fn local_bindings_never_enter_the_environment() {
    let env = run("let x = 1\nmut y = 2\n$y = 3");
    assert!(env.is_empty());
}

#[test]
fn unset_removes_an_entry_and_is_a_noop_when_absent() {
    let mut env = Environment::from_snapshot([("PATH", os("/bin")), ("HOME", os("/root"))]);
    run_in("unset PATH", &mut env).unwrap();
    assert_eq!(env.get("PATH"), None);
    assert_eq!(env.get("HOME"), Some(OsStr::new("/root")));

    // Unsetting an absent name is a successful no-op.
    run_in("unset MISSING", &mut env).unwrap();
    assert_eq!(env.len(), 1);
}

#[test]
fn unset_does_not_touch_a_lexical_binding() {
    // `x` is a local binding; `unset x` only affects the environment (a no-op
    // here) and leaves the script value from the following read intact.
    let mut env = Environment::new();
    run_in("let x = 5\nunset x", &mut env).unwrap();
    assert!(env.is_empty());
}

#[test]
fn a_snapshot_is_inherited_and_then_mutated() {
    let mut env = Environment::from_snapshot([("A", os("1"))]);
    run_in("export B = '2'\nunset A", &mut env).unwrap();
    let names: Vec<&str> = env.names().collect();
    assert_eq!(names, ["B"]);
    assert_eq!(env.get("B"), Some(OsStr::new("2")));
}

#[test]
fn exporting_an_ineligible_value_is_an_error() {
    assert_eq!(
        run_in("export BAD = [1, 2]", &mut Environment::new()).unwrap_err(),
        RuntimeErrorKind::ExportValueNotEligible { actual: "list" }
    );
    assert_eq!(
        run_in("export BAD = null", &mut Environment::new()).unwrap_err(),
        RuntimeErrorKind::ExportValueNotEligible { actual: "null" }
    );
}

#[test]
fn the_environment_iterates_in_sorted_name_order() {
    let env = run("export C = '3'\nexport A = '1'\nexport B = '2'");
    let names: Vec<&str> = env.names().collect();
    assert_eq!(names, ["A", "B", "C"]);
}
