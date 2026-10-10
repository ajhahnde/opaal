#![forbid(unsafe_code)]
#[path = "support/stdio.rs"]
mod support;
use opaal_runtime::Value;
use opaal_runtime::eval::CancellationToken;
use opaal_runtime::operational::standard_source::StandardBinding;
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::script::{
    ScriptExecutionOutcome, execute_ambient_module_program_outcome_with_standard_host,
};
use opaal_runtime::{HostEnvironmentLimits, NativeSessionSnapshot};
use support::Harness;

struct NoExecutables;
impl opaal_runtime::resolve::ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &std::ffi::OsStr) -> bool {
        panic!("no helper process")
    }
}
fn run(body: &str, h: Harness) -> ScriptExecutionOutcome {
    let program = support::load(body);
    let snapshot = NativeSessionSnapshot::from_snapshot(
        "/stdio",
        std::iter::empty::<(&str, &str)>(),
        HostEnvironmentLimits::OPAAL,
    )
    .unwrap();
    let clock = h.clock.clone();
    let mut binding = StandardBinding::new(
        support::authority(),
        CancellationToken::never(),
        clock.clone(),
        None,
        h.state,
    );
    execute_ambient_module_program_outcome_with_standard_host(
        &program,
        &[],
        snapshot,
        &opaal_runtime::builtin::standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &h.platform,
        clock,
        &mut Vec::new(),
        &mut binding,
    )
}

#[test]
fn source_processor_has_exact_payload_and_diagnostics_and_closes_the_binding() {
    let h = Harness::new("Grüße\n".as_bytes());
    let script = h.script.clone();
    let outcome = run(
        "import std::string as string\nlet bytes = io::read_stdin(4096)\nlet text = string::decode_utf8(bytes)\nio::print(text)\nio::eprintln('processed')",
        h,
    );
    assert!(
        matches!(outcome.primary(), PrimaryOutcome::Completed(_)),
        "{outcome:?}"
    );
    let script = script.lock().unwrap();
    assert_eq!(script.stdout, "Grüße\n".as_bytes());
    assert_eq!(script.stderr, b"processed\n");
    assert_eq!(script.closed, 1);
}

#[test]
fn pure_functions_callbacks_and_undeclared_actions_refuse_before_transfer() {
    for body in [
        "def pure() -> Null { return io::print('x') }\npure()",
        "let callback = {|| io::print('x')}\ncallback()",
        "import std::list as list\nlist::map[Int, Null]([1], {|x| io::print('x')})",
        "action pure() -> Null effects {} { return io::print('x') }\npure()",
    ] {
        let h = Harness::new(b"sentinel");
        let script = h.script.clone();
        let outcome = run(body, h);
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Refused(_)),
            "{body}: {outcome:?}"
        );
        assert!(script.lock().unwrap().calls.is_empty());
    }
}

#[test]
fn undeclared_sibling_or_nested_streams_and_entropy_cannot_borrow_another_effect() {
    for body in [
        "action run() -> Null effects { stdout.write(); } { return io::eprint('x') }\nrun()",
        "import std::random as random\naction run() -> Int effects { stdout.write(); } { return random::int(7, 8) }\nrun()",
    ] {
        let h = Harness::new(b"sentinel");
        let script = h.script.clone();
        let outcome = run(body, h);
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Refused(_)),
            "{body}: {outcome:?}"
        );
        assert!(script.lock().unwrap().calls.is_empty());
    }
    assert!(support::try_load("action child() -> Null effects { stderr.write(); } { return io::eprint('x') }\naction run() -> Null effects { stdout.write(); } { return child() }\nrun()").is_err());
}

#[test]
fn function_alias_fails_explicitly_before_transfer() {
    let h = Harness::new(b"sentinel");
    let script = h.script.clone();
    let outcome = run("let alias = io::print\nalias('x')", h);
    let PrimaryOutcome::Error(error) = outcome.primary() else {
        panic!("{outcome:?}")
    };
    assert!(
        error
            .render()
            .contains("operational function alias without host identity")
    );
    assert!(script.lock().unwrap().calls.is_empty());
}

#[test]
fn declared_action_runs_and_catching_overflow_does_not_restore_consumed_input() {
    let h = Harness::new(b"abc");
    let script = h.script.clone();
    let outcome = run(
        "action run() -> Bytes effects { stdin.read(); stdout.write(); } {\ntry { let ignored = io::read_stdin(1) } catch error { 0 }\nlet rest = io::read_stdin(1)\nio::write_stdout(rest)\nreturn rest\n}\nrun()",
        h,
    );
    let PrimaryOutcome::Completed(completed) = outcome.primary() else {
        panic!("{outcome:?}")
    };
    assert_eq!(completed.value(), &Value::bytes(b"c".to_vec()));
    assert_eq!(script.lock().unwrap().stdout, b"c");
}

#[test]
fn invalid_utf8_and_runtime_any_type_errors_emit_no_language_output() {
    for (body, input) in [
        (
            "import std::string as string\nio::print(string::decode_utf8(io::read_stdin(1)))",
            &b"\xff"[..],
        ),
        ("let bytes: Any = 'text'\nio::write_stdout(bytes)", &b""[..]),
    ] {
        let h = Harness::new(input);
        let script = h.script.clone();
        assert!(matches!(run(body, h).primary(), PrimaryOutcome::Error(_)));
        let script = script.lock().unwrap();
        assert!(script.stdout.is_empty() && script.stderr.is_empty());
    }
}
