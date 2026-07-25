#![forbid(unsafe_code)]

//! Acceptance coverage for the persistent interactive session driver.
//!
//! A `Session` retains scope, environment, logical cwd, and last status across
//! independently submitted edit buffers, dispatches single-stage internal
//! built-ins against that state, executes external foreground pipelines, and
//! surfaces recoverable failures without discarding the accumulated state. It
//! never depends on a real process, terminal, or clock: every test drives the
//! host-free `FakePlatform` and `FakeClock`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use flashshell_platform::FakePlatform;
use flashshell_runtime::Environment;
use flashshell_runtime::eval::FakeClock;
use flashshell_runtime::plan::SessionOptions;
use flashshell_runtime::resolve::ExecutableProbe;
use flashshell_runtime::session::{Session, SubmitOutcome};

#[derive(Default)]
struct Probe {
    paths: Vec<PathBuf>,
}

impl Probe {
    fn new(paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            paths: paths.into_iter().map(Into::into).collect(),
        }
    }
}

impl ExecutableProbe for Probe {
    fn is_executable(&self, path: &OsStr) -> bool {
        self.paths
            .iter()
            .any(|candidate| candidate.as_os_str() == path)
    }
}

fn environment() -> Environment {
    Environment::from_snapshot([
        ("PATH", OsString::from("/bin")),
        ("HOME", OsString::from("/home/me")),
    ])
}

fn session() -> Session {
    Session::new("/work", environment(), SessionOptions::default())
}

/// Submit one buffer with a fresh throwaway output sink, asserting success.
fn submit(session: &mut Session, text: &str, probe: &dyn ExecutableProbe) -> SubmitOutcome {
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            text,
            probe,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("submission should succeed")
}

#[test]
fn pure_bindings_persist_across_submissions() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(&mut session, "let base = 41", &probe),
        SubmitOutcome::Continued
    );
    // A later submission observes the earlier binding through the same scope.
    assert_eq!(
        submit(&mut session, "export DERIVED = $base", &probe),
        SubmitOutcome::Continued
    );

    assert_eq!(session.environment().get("DERIVED"), Some(OsStr::new("41")));
}

#[test]
fn cd_updates_the_logical_cwd_across_submissions() {
    let mut session = session();
    let probe = Probe::default();

    submit(&mut session, "cd /srv", &probe);
    assert_eq!(session.cwd(), Path::new("/srv"));

    // A relative target resolves against the retained logical cwd.
    submit(&mut session, "cd data", &probe);
    assert_eq!(session.cwd(), Path::new("/srv/data"));
}

#[test]
fn exit_with_an_explicit_code_requests_termination() {
    let mut session = session();
    let probe = Probe::default();

    assert_eq!(
        submit(&mut session, "exit 7", &probe),
        SubmitOutcome::Exit(7)
    );
}

#[test]
fn exit_without_an_argument_uses_the_last_status() {
    let mut session = session();
    let probe = Probe::new(["/bin/tool"]);

    // A successful external leaves status zero, which a bare exit then reports.
    submit(&mut session, "^tool", &probe);
    assert_eq!(submit(&mut session, "exit", &probe), SubmitOutcome::Exit(0));
}

#[test]
fn external_commands_execute_and_record_their_status() {
    let mut session = session();
    let probe = Probe::new(["/bin/tool"]);

    assert_eq!(
        submit(&mut session, "^tool", &probe),
        SubmitOutcome::Continued
    );
    assert_eq!(
        session.current_status().and_then(|status| status.code()),
        Some(0)
    );
}

#[test]
fn pwd_renders_the_logical_cwd_to_the_output_sink() {
    let mut session = session();
    let probe = Probe::default();
    submit(&mut session, "cd /srv", &probe);

    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "pwd",
            &probe,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("pwd should succeed");

    let rendered = String::from_utf8(sink).expect("pwd output is UTF-8");
    assert!(
        rendered.contains("/srv"),
        "pwd should print the logical cwd, got {rendered:?}"
    );
}

#[test]
fn a_recoverable_error_preserves_state_and_renders_a_diagnostic() {
    let mut session = session();
    let probe = Probe::default();
    submit(&mut session, "let keep = 5", &probe);

    let mut sink = Vec::new();
    let error = session
        .submit(
            "<interactive>",
            "$missing",
            &probe,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("an unknown binding is a recoverable failure");
    assert!(
        !error.render().is_empty(),
        "the failure must render a diagnostic"
    );

    // The earlier binding survives the failed submission.
    assert_eq!(
        submit(&mut session, "export STILL = $keep", &probe),
        SubmitOutcome::Continued
    );
    assert_eq!(session.environment().get("STILL"), Some(OsStr::new("5")));
}

#[test]
fn several_statements_in_one_buffer_run_in_source_order() {
    let mut session = session();
    let probe = Probe::default();

    submit(&mut session, "let a = 1\nexport B = $a", &probe);
    assert_eq!(session.environment().get("B"), Some(OsStr::new("1")));
}
