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

use flashshell_platform::{Capabilities, FakePlatform, TerminalSize};
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

fn terminal_platform() -> FakePlatform {
    FakePlatform::with_terminal(Capabilities::full(), true, TerminalSize::new(80, 24))
}

/// Submit one buffer with a fresh throwaway output sink, asserting success.
fn submit(session: &mut Session, text: &str, probe: &dyn ExecutableProbe) -> SubmitOutcome {
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            text,
            probe,
            &terminal_platform(),
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
            &terminal_platform(),
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
fn an_internal_structured_pipeline_preserves_values_until_final_presentation() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd missing | select name | get name | first 1",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the all-internal structured pipeline should execute");

    assert_eq!(String::from_utf8(sink).unwrap(), "pwd\n");
    let status = session.current_status().expect("pipeline records a status");
    assert_eq!(status.code(), Some(0));
    assert_eq!(status.stages().len(), 4);
}

#[test]
fn a_terminal_structured_command_can_materialize_under_its_bound() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd missing | length",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("length should consume the live value stream");

    assert_eq!(String::from_utf8(sink).unwrap(), "2\n");
}

#[test]
fn closure_free_reshapers_compose_without_serializing_an_edge() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd missing | get kind | lines | sort | last 1 | collect",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the closure-free structured commands should compose");

    assert_eq!(
        String::from_utf8(sink).unwrap(),
        "[\"internalmissing\"]\n",
        "lines treats adjacent String values as chunks of one logical text stream"
    );
}

#[test]
fn ls_is_a_live_structured_source() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "ls | length",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the fake platform's empty directory should be a live stream");

    assert_eq!(String::from_utf8(sink).unwrap(), "0\n");
}

#[test]
fn a_typed_argument_to_a_word_only_builtin_is_rejected() {
    let mut session = session();
    let probe = Probe::default();
    let original = session.cwd().to_owned();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "cd {|| 1}",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("cd must not reinterpret a callable as a native path");

    assert!(error.render().contains("expected a word argument"));
    assert_eq!(session.cwd(), original);
    assert!(sink.is_empty());
}

#[test]
fn a_lazy_structured_failure_does_not_commit_pipeline_status() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "which pwd | get absent",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("the missing field is discovered when presentation pulls");

    assert!(error.render().contains("record has no field `absent`"));
    assert!(session.current_status().is_none());
    assert!(sink.is_empty());
}

#[test]
fn each_and_where_execute_captured_closures_lazily() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "let wanted = 'internal'\n\
             which pwd missing | where {|row| $row.kind == $wanted} | \
             each {|row| $row.name} | first 1",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("where and each should execute their captured closures");

    assert_eq!(String::from_utf8(sink).unwrap(), "pwd\n");
}

#[test]
fn update_supports_closure_and_static_replacements() {
    let probe = Probe::default();

    let mut closure_session = session();
    let mut closure_sink = Vec::new();
    closure_session
        .submit(
            "<interactive>",
            "which pwd | update kind {|kind| 'changed'} | get kind",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut closure_sink,
        )
        .expect("update should apply a closure to the current field");
    assert_eq!(String::from_utf8(closure_sink).unwrap(), "changed\n");

    let mut static_session = session();
    let mut static_sink = Vec::new();
    static_session
        .submit(
            "<interactive>",
            "which pwd | update kind known | get kind",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut static_sink,
        )
        .expect("update should accept a static text replacement");
    assert_eq!(String::from_utf8(static_sink).unwrap(), "known\n");
}

#[test]
fn a_successful_lazy_closure_pipeline_commits_its_environment() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "def mark(row) {\n\
                 export SEEN = 'yes'\n\
                 return $row\n\
             }\n\
             which pwd | each {|row| mark($row)} | first 1",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("a successful closure application should commit its environment");

    assert_eq!(session.environment().get("SEEN"), Some(OsStr::new("yes")));
}

#[test]
fn a_failing_lazy_closure_pipeline_rolls_back_its_environment() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "def mark(row) {\n\
                 export LEAK = 'no'\n\
                 return $row\n\
             }\n\
             which pwd | each {|row| mark($row)} | get absent",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("a downstream lazy failure should reject closure side effects");

    assert!(error.render().contains("record has no field `absent`"));
    assert_eq!(session.environment().get("LEAK"), None);
    assert!(sink.is_empty());
}

#[test]
fn structured_output_requires_an_interactive_output_terminal() {
    let mut session = session();
    let probe = Probe::default();
    let platform = FakePlatform::with_terminal_ends(
        Capabilities::full(),
        true,
        false,
        TerminalSize::new(80, 24),
    );
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "pwd",
            &probe,
            &platform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("nonterminal structured output must require serialization");

    assert!(error.render().contains("explicit `encode`/`to`"));
    assert!(sink.is_empty());
    assert!(session.current_status().is_none());
}

#[test]
fn a_structured_stdout_redirection_is_refused_before_execution() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "pwd > out.txt",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("redirected structured output must require serialization");

    assert!(error.render().contains("redirected output"));
    assert!(error.render().contains("explicit `encode`/`to`"));
    assert!(sink.is_empty());
    assert!(session.current_status().is_none());
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
