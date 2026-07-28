#![forbid(unsafe_code)]

//! Acceptance coverage for the persistent interactive session driver.
//!
//! A `Session` retains scope, environment, logical cwd, and last status across
//! independently submitted edit buffers, dispatches single-stage internal
//! built-ins against that state, executes external foreground pipelines, and
//! surfaces recoverable failures without discarding the accumulated state. It
//! never depends on a real process, terminal, or clock. Most tests drive the
//! host-free `FakePlatform`; file-boundary acceptance uses the POSIX adapter
//! against isolated temporary directories.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flashshell_platform::{
    Capabilities, FakePlatform, Platform, ProcessStatus, SpawnRequest, TerminalSize,
};
use flashshell_platform_posix::PosixPlatform;
use flashshell_runtime::eval::FakeClock;
use flashshell_runtime::plan::SessionOptions;
use flashshell_runtime::resolve::ExecutableProbe;
use flashshell_runtime::session::{Session, SubmitOutcome};
use flashshell_runtime::{Environment, Status};

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
fn explicit_codec_boundaries_round_trip_live_bytes() {
    let mut session = session();
    let probe = Probe::default();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            "which pwd | get kind | encode utf8 | decode utf8 | encode utf8",
            &probe,
            &terminal_platform(),
            &FakeClock::new(),
            &mut sink,
        )
        .expect("explicit codec boundaries should keep bytes byte-correct");

    assert_eq!(sink, b"internal");
}

#[test]
fn live_file_and_format_boundaries_preserve_json_and_text() {
    let temp = TempDir::new("session-boundaries");
    fs::write(
        temp.path().join("input.json"),
        br#"{"name":"FlashOS","active":true}"#,
    )
    .expect("JSON fixture should be written");
    fs::write(temp.path().join("input.txt"), b"one\r\ntwo\n")
        .expect("text fixture should be written");
    fs::write(temp.path().join("input.bin"), [0, 0xff, 7])
        .expect("binary fixture should be written");

    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::default();
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "open input.json | from json | to json | save output.json\n\
             open input.txt | from text | first 1 | to text | save first.txt\n\
             open input.bin | decode bytes | encode bytes | save output.bin",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("file and format boundaries should execute end to end");

    assert!(sink.is_empty());
    assert_eq!(
        fs::read(temp.path().join("output.json")).unwrap(),
        br#"{"name":"FlashOS","active":true}"#
    );
    assert_eq!(fs::read(temp.path().join("first.txt")).unwrap(), b"one\n");
    assert_eq!(
        fs::read(temp.path().join("output.bin")).unwrap(),
        [0, 0xff, 7]
    );
}

#[test]
fn mixed_process_boundaries_stream_in_both_directions() {
    let temp = TempDir::new("session-mixed-boundaries");
    fs::write(temp.path().join("lines.txt"), b"one\ntwo\n")
        .expect("text fixture should be written");
    let binary: Vec<u8> = (0u8..=255).cycle().take(2 * 1024 * 1024).collect();
    fs::write(temp.path().join("input.bin"), &binary).expect("binary fixture should be written");

    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::new(["/bin/cat"]);
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "^/bin/cat < lines.txt | from text | first 1 | to text\n\
             open input.bin | decode bytes | encode bytes | ^/bin/cat > output.bin\n\
             ^/bin/cat < input.bin | decode bytes | encode bytes | \
             ^/bin/cat > roundtrip.bin",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("mixed boundaries should stream without capture");

    assert_eq!(sink, b"one\n");
    assert_eq!(fs::read(temp.path().join("output.bin")).unwrap(), binary);
    assert_eq!(fs::read(temp.path().join("roundtrip.bin")).unwrap(), binary);
}

#[test]
fn an_early_external_exit_stops_the_internal_byte_producer() {
    let temp = TempDir::new("session-mixed-early-exit");
    fs::write(temp.path().join("large.bin"), vec![b'x'; 2 * 1024 * 1024])
        .expect("large fixture should be written");

    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::new(["/usr/bin/head"]);
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "open large.bin | ^/usr/bin/head -c 1 > first.bin",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("a closed consumer pipe should stop the internal producer");

    assert!(sink.is_empty());
    assert_eq!(fs::read(temp.path().join("first.bin")).unwrap(), b"x");
    assert_eq!(session.current_status().and_then(Status::code), Some(0),);
}

#[test]
fn mixed_pipeline_statuses_aggregate_in_source_order() {
    let temp = TempDir::new("session-mixed-status");
    let mut session = Session::new(
        temp.path(),
        environment(),
        SessionOptions::default().with_pipefail(true),
    );
    let probe = Probe::new(["/usr/bin/false", "/bin/cat"]);
    let mut sink = Vec::new();
    session
        .submit(
            "<interactive>",
            "^/usr/bin/false | decode bytes | encode bytes | ^/bin/cat > output.bin",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("nonzero process completion should remain a normal status");

    let status = session
        .current_status()
        .expect("pipeline status should commit");
    assert_eq!(status.code(), Some(1));
    assert_eq!(status.stages().len(), 4);
    assert_eq!(
        status.stages().iter().map(Status::code).collect::<Vec<_>>(),
        vec![Some(1), Some(0), Some(0), Some(0)]
    );
}

#[test]
fn a_lazy_byte_boundary_failure_does_not_commit_status() {
    let temp = TempDir::new("session-byte-failure");
    fs::write(temp.path().join("bad.txt"), [0xff]).expect("byte fixture should be written");
    let mut session = Session::new(temp.path(), environment(), SessionOptions::default());
    let probe = Probe::default();
    let mut sink = Vec::new();

    let error = session
        .submit(
            "<interactive>",
            "open bad.txt | decode utf8 | encode utf8",
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect_err("strict decoding should surface malformed input lazily");

    assert!(error.render().contains("malformed input at byte offset 0"));
    assert!(sink.is_empty());
    assert!(session.current_status().is_none());
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

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("flashshell-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("temporary directory should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).expect("temporary directory should be removed");
    }
}

#[test]
fn the_external_members_around_an_internal_island_share_one_process_group() {
    let temp = TempDir::new("session-mixed-group");
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-job-observer-fixture"));
    let mut environment = environment();
    environment.set(
        "FLASH_PROBE_REPORT",
        temp.path().join("report.bin").into_os_string(),
    );
    environment.set(
        "FLASH_PROBE_GROUP_REPORT",
        temp.path().as_os_str().to_os_string(),
    );
    let mut session = Session::new(temp.path(), environment, SessionOptions::default());
    let probe = Probe::new([fixture.as_os_str()]);
    let observer = fixture.to_string_lossy().into_owned();
    let mut sink = Vec::new();

    session
        .submit(
            "<interactive>",
            format!("^{observer} | from text | to text | ^{observer}"),
            &probe,
            &PosixPlatform,
            &FakeClock::new(),
            &mut sink,
        )
        .expect("the mixed pipeline should execute");

    // An internal island splits the pipeline but not the job: the external
    // stages on either side of it stay members of the same group, and that
    // group is not the shell's own.
    let groups = reported_groups(temp.path());
    assert_eq!(groups.len(), 2, "both external members report a group");
    assert_eq!(groups[0], groups[1]);
    assert_ne!(groups[0], shell_group());
}

/// The group of a child spawned without a placement, which is the shell's own.
fn shell_group() -> u64 {
    let temp = TempDir::new("session-shell-group");
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-job-observer-fixture"));
    let argv = [OsString::from("inheritor")];
    let environment = [
        (
            OsString::from("FLASH_PROBE_REPORT"),
            temp.path().join("report.bin").into_os_string(),
        ),
        (
            OsString::from("FLASH_PROBE_GROUP_REPORT"),
            temp.path().as_os_str().to_os_string(),
        ),
    ];
    let request = SpawnRequest::new(&fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid");
    let mut child = PosixPlatform.spawn(&request).expect("the fixture spawns");
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));

    let groups = reported_groups(temp.path());
    assert_eq!(groups.len(), 1);
    groups[0]
}

/// Every process group reported by the observers that ran in `directory`.
fn reported_groups(directory: &Path) -> Vec<u64> {
    let mut groups: Vec<u64> = fs::read_dir(directory)
        .expect("the probe directory should be readable")
        .map(|entry| entry.expect("the entry should be readable").path())
        .filter(|path| path.extension() == Some(OsStr::new("group")))
        .map(|path| {
            fs::read_to_string(path)
                .expect("the fixture should report its process group")
                .trim()
                .parse()
                .expect("a process group is an integer")
        })
        .collect();
    groups.sort_unstable();
    groups
}
