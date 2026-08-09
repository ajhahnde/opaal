#![forbid(unsafe_code)]

//! Acceptance coverage for the standard internal-command family.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use flash_platform::FakePlatform;
use flash_runtime::builtin::{
    BuiltinOutcome, BuiltinOutput, SessionState, execute_builtin, standard_registry,
};
use flash_runtime::command::{Carrier, CommandOutput};
use flash_runtime::eval::RuntimeErrorKind;
use flash_runtime::plan::{ExecutionPlan, plan_pipeline};
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::{Duration, Environment, ScopeStack, Status, Value};
use flash_syntax::{ParseOutcome, Pipeline, SourceFile, SourceId, StatementKind, parse};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(1), "test.fsh", text)
}

fn pipeline(file: &SourceFile) -> Pipeline {
    let script = match parse(file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}"),
    };
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("expected a job statement");
    };
    job.chain.or_terms()[0].and_terms()[0].clone()
}

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

fn build(text: &str, environment: &Environment, probe: &dyn ExecutableProbe) -> ExecutionPlan {
    let file = source(text);
    plan_pipeline(
        &pipeline(&file),
        "/work",
        &file,
        &mut ScopeStack::new(),
        environment,
        &standard_registry(),
        probe,
    )
    .expect("built-in plan should build")
}

fn state() -> SessionState {
    SessionState::new(
        "/work",
        Environment::from_snapshot([
            ("PATH", OsString::from("/bin")),
            ("HOME", OsString::from("/home/me")),
        ]),
    )
}

#[test]
fn standard_registry_has_exact_carrier_contracts() {
    let registry = standard_registry();
    assert_eq!(
        registry.names().collect::<Vec<_>>(),
        [
            "bg", "cd", "check", "collect", "command", "decode", "each", "encode", "exit", "fg",
            "first", "from", "get", "help", "jobs", "kill", "last", "length", "lines", "ls",
            "open", "pwd", "save", "select", "sort", "to", "update", "wait", "where", "which"
        ]
    );

    let fixed = |name| registry.lookup(name).expect("registered").output();
    assert_eq!(fixed("cd"), CommandOutput::Fixed(Carrier::Empty));
    assert_eq!(fixed("pwd"), CommandOutput::Fixed(Carrier::Value));
    assert_eq!(fixed("which"), CommandOutput::Fixed(Carrier::ValueStream));
    assert_eq!(fixed("command"), CommandOutput::Fixed(Carrier::ByteStream));
    assert_eq!(fixed("exit"), CommandOutput::Fixed(Carrier::Empty));
    assert_eq!(fixed("check"), CommandOutput::SameAsInput);
    assert_eq!(fixed("help"), CommandOutput::Fixed(Carrier::ByteStream));
    assert!(registry.lookup("help").unwrap().accepts(Carrier::Empty));
    assert_eq!(fixed("jobs"), CommandOutput::Fixed(Carrier::ValueStream));
    for name in ["fg", "bg", "wait", "kill"] {
        assert_eq!(fixed(name), CommandOutput::Fixed(Carrier::Empty));
    }
    for name in ["jobs", "fg", "bg", "wait", "kill"] {
        let signature = registry.lookup(name).expect("job command is registered");
        assert!(signature.accepts(Carrier::Empty));
        assert!(!signature.accepts(Carrier::ByteStream));
        assert!(!signature.accepts(Carrier::Value));
        assert!(!signature.accepts(Carrier::ValueStream));
    }
    assert_eq!(
        registry.lookup("kill").unwrap().flags().collect::<Vec<_>>(),
        [
            "--continue",
            "--hangup",
            "--interrupt",
            "--kill",
            "--stop",
            "--terminate"
        ]
    );
    assert!(registry.lookup("command").unwrap().accepts(Carrier::Empty));
    assert!(
        registry
            .lookup("command")
            .unwrap()
            .accepts(Carrier::ByteStream)
    );
    for carrier in [
        Carrier::Empty,
        Carrier::ByteStream,
        Carrier::Value,
        Carrier::ValueStream,
    ] {
        assert!(registry.lookup("check").unwrap().accepts(carrier));
    }

    for signature in registry.signatures() {
        assert!(
            !signature.documentation().invocation().is_empty(),
            "{} must have an invocation",
            signature.name()
        );
        assert!(
            !signature
                .documentation()
                .documentation()
                .summary()
                .is_empty(),
            "{} must have a summary",
            signature.name()
        );
    }

    // The explicit byte/structured boundary commands. `decode`/`from` parse a
    // `ByteStream` into structured values; `encode`/`to` serialize structured
    // values back into a `ByteStream`. These make the pipeline-validation
    // `encode`/`to` and `decode`/`from` bridge suggestions name real commands.
    // `ls` is a structured producer: it takes no pipeline input and yields one
    // record per directory entry.
    assert_eq!(fixed("ls"), CommandOutput::Fixed(Carrier::ValueStream));
    {
        let signature = registry.lookup("ls").unwrap();
        assert!(signature.accepts(Carrier::Empty));
        assert!(!signature.accepts(Carrier::ByteStream));
        assert!(!signature.accepts(Carrier::ValueStream));
    }

    // File bytes cross the file boundary unchanged. `open` is a byte producer
    // and `save` is a byte sink; parsing and serialization remain explicit
    // `from`/`to` stages rather than being hidden in either command.
    assert_eq!(fixed("open"), CommandOutput::Fixed(Carrier::ByteStream));
    {
        let signature = registry.lookup("open").unwrap();
        assert!(signature.accepts(Carrier::Empty));
        assert!(!signature.accepts(Carrier::ByteStream));
        assert!(!signature.accepts(Carrier::ValueStream));
    }
    assert_eq!(fixed("save"), CommandOutput::Fixed(Carrier::Empty));
    {
        let signature = registry.lookup("save").unwrap();
        assert!(signature.accepts(Carrier::ByteStream));
        assert!(!signature.accepts(Carrier::Empty));
        assert!(!signature.accepts(Carrier::ValueStream));
    }

    assert_eq!(fixed("decode"), CommandOutput::Fixed(Carrier::ValueStream));
    assert_eq!(fixed("from"), CommandOutput::Fixed(Carrier::ValueStream));
    assert_eq!(fixed("encode"), CommandOutput::Fixed(Carrier::ByteStream));
    assert_eq!(fixed("to"), CommandOutput::Fixed(Carrier::ByteStream));
    for name in ["decode", "from"] {
        let signature = registry.lookup(name).unwrap();
        assert!(signature.accepts(Carrier::ByteStream));
        assert!(!signature.accepts(Carrier::Value));
    }
    for name in ["encode", "to"] {
        let signature = registry.lookup(name).unwrap();
        assert!(signature.accepts(Carrier::Value));
        assert!(signature.accepts(Carrier::ValueStream));
        assert!(!signature.accepts(Carrier::ByteStream));
    }

    // The closure-free structured commands. Each consumes a value stream;
    // `first`/`last`/`lines` reshape it to a value stream, `collect`/`length`
    // produce one value.
    assert_eq!(fixed("first"), CommandOutput::Fixed(Carrier::ValueStream));
    assert_eq!(fixed("last"), CommandOutput::Fixed(Carrier::ValueStream));
    assert_eq!(fixed("collect"), CommandOutput::Fixed(Carrier::Value));
    assert_eq!(fixed("length"), CommandOutput::Fixed(Carrier::Value));
    assert_eq!(fixed("lines"), CommandOutput::Fixed(Carrier::ValueStream));
    for name in ["first", "last", "collect", "length", "lines"] {
        let signature = registry.lookup(name).unwrap();
        assert!(signature.accepts(Carrier::ValueStream));
        assert!(!signature.accepts(Carrier::ByteStream));
    }

    // The closure-driven structured commands. `each` maps and `where` filters the
    // value stream, so each consumes and produces a value stream.
    assert_eq!(fixed("each"), CommandOutput::Fixed(Carrier::ValueStream));
    assert_eq!(fixed("where"), CommandOutput::Fixed(Carrier::ValueStream));
    for name in ["each", "where"] {
        let signature = registry.lookup(name).unwrap();
        assert!(signature.accepts(Carrier::ValueStream));
        assert!(!signature.accepts(Carrier::ByteStream));
    }

    // The read-only record projections. `select` narrows records and `get`
    // extracts a field, so each consumes and produces a value stream.
    assert_eq!(fixed("select"), CommandOutput::Fixed(Carrier::ValueStream));
    assert_eq!(fixed("get"), CommandOutput::Fixed(Carrier::ValueStream));
    assert_eq!(fixed("update"), CommandOutput::Fixed(Carrier::ValueStream));
    assert_eq!(fixed("sort"), CommandOutput::Fixed(Carrier::ValueStream));
    for name in ["select", "get", "update", "sort"] {
        let signature = registry.lookup(name).unwrap();
        assert!(signature.accepts(Carrier::ValueStream));
        assert!(!signature.accepts(Carrier::ByteStream));
    }
}

#[test]
fn check_planning_preserves_the_upstream_carrier_and_requires_it() {
    let environment = Environment::from_snapshot([("PATH", "/bin")]);
    let probe = Probe::new(["/bin/tool"]);
    let plan = build("^tool | check", &environment, &probe);
    assert_eq!(plan.stages()[1].output_carrier(), Carrier::ByteStream);
    let empty_plan = build("cd /next | check", &environment, &probe);
    assert_eq!(empty_plan.stages()[1].output_carrier(), Carrier::Empty);

    let file = source("check");
    let error = plan_pipeline(
        &pipeline(&file),
        "/work",
        &file,
        &mut ScopeStack::new(),
        &environment,
        &standard_registry(),
        &probe,
    )
    .expect_err("check without upstream status must not plan");
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::CheckRequiresUpstream
    ));
}

#[test]
fn cd_updates_logical_cwd_pwd_and_oldpwd_atomically() {
    let probe = Probe::default();
    let mut session = state();
    let plan = build("cd /next", session.environment(), &probe);

    let outcome = execute_builtin(
        &plan.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::full(),
    )
    .expect("cd should succeed");

    let BuiltinOutcome::Completed(completion) = outcome else {
        panic!("cd should complete internally");
    };
    assert_eq!(completion.output(), &BuiltinOutput::Empty);
    assert_eq!(session.cwd(), Path::new("/next"));
    assert_eq!(
        session.environment().get("OLDPWD"),
        Some(OsStr::new("/work"))
    );
    assert_eq!(session.environment().get("PWD"), Some(OsStr::new("/next")));

    let snapshot = session.clone();
    let failed_plan = build("cd /denied", session.environment(), &probe);
    let error = execute_builtin(
        &failed_plan.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect_err("unsupported cwd resolution should fail");
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::WorkingDirectory(_)
    ));
    assert_eq!(session, snapshot);
}

#[test]
fn cd_without_an_argument_uses_the_native_home_entry() {
    let probe = Probe::default();
    let mut session = state();
    let plan = build("cd", session.environment(), &probe);
    execute_builtin(
        &plan.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::full(),
    )
    .expect("HOME should be a valid default target");
    assert_eq!(session.cwd(), Path::new("/home/me"));
}

#[test]
fn pwd_returns_the_stored_path_without_platform_access() {
    let probe = Probe::default();
    let mut session = state();
    let plan = build("pwd", session.environment(), &probe);
    let outcome = execute_builtin(
        &plan.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect("pwd should not consult the platform");
    let BuiltinOutcome::Completed(completion) = outcome else {
        panic!("pwd should complete internally");
    };
    let BuiltinOutput::Value(Value::Path(path)) = completion.output() else {
        panic!("pwd should produce one Path value");
    };
    assert_eq!(path.as_os_str(), OsStr::new("/work"));
    assert!(completion.status().is_ok());
}

#[test]
fn which_reports_internal_external_and_missing_names_in_order() {
    let probe = Probe::new(["/bin/tool"]);
    let mut session = state();
    let plan = build("which pwd tool absent", session.environment(), &probe);
    let outcome = execute_builtin(
        &plan.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect("which misses are normal data");
    let BuiltinOutcome::Completed(completion) = outcome else {
        panic!("which should complete internally");
    };
    let BuiltinOutput::ValueStream(records) = completion.output() else {
        panic!("which should produce a value stream");
    };
    let kinds: Vec<&str> = records
        .iter()
        .map(|value| {
            let Value::Record(record) = value else {
                panic!("which item should be a record");
            };
            let Some(Value::String(kind)) = record.get("kind") else {
                panic!("which record should have a kind");
            };
            kind.as_ref()
        })
        .collect();
    assert_eq!(kinds, ["internal", "external", "missing"]);
    assert_eq!(completion.status().code(), Some(1));
    assert_eq!(session.current_status().unwrap().code(), Some(1));
}

#[test]
fn command_forces_external_resolution_and_preserves_native_argv() {
    let probe = Probe::new(["/bin/pwd"]);
    let mut session = state();
    let plan = build("command pwd 'two words' ''", session.environment(), &probe);
    let outcome = execute_builtin(
        &plan.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect("command should resolve the external pwd");
    let BuiltinOutcome::External(invocation) = outcome else {
        panic!("command should request external execution");
    };
    assert_eq!(invocation.executable(), Path::new("/bin/pwd"));
    assert_eq!(
        invocation.argv(),
        [
            OsString::from("pwd"),
            OsString::from("two words"),
            OsString::new()
        ]
    );
}

#[test]
fn exit_returns_a_request_without_terminating_the_host() {
    let probe = Probe::default();
    let mut session = state();
    session.set_current_status(Some(Status::exit(23, Duration::ZERO).unwrap()));
    let default_plan = build("exit", session.environment(), &probe);
    let default = execute_builtin(
        &default_plan.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect("default exit should use current status");
    let BuiltinOutcome::Exit(request) = default else {
        panic!("exit should return a request");
    };
    assert_eq!(request.code(), 23);

    let explicit_plan = build("exit 255", session.environment(), &probe);
    let explicit = execute_builtin(
        &explicit_plan.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect("255 is a valid explicit code");
    let BuiltinOutcome::Exit(request) = explicit else {
        panic!("exit should return a request");
    };
    assert_eq!(request.code(), 255);
}

#[test]
fn check_forwards_success_and_converts_only_unsuccessful_status() {
    let probe = Probe::new(["/bin/tool"]);
    let mut session = state();
    let plan = build("^tool | check", session.environment(), &probe);
    let check = &plan.stages()[1];
    let success = Status::exit(0, Duration::ZERO).unwrap();
    let outcome = execute_builtin(
        check,
        Carrier::ByteStream,
        Some(&success),
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect("successful status should pass check");
    let BuiltinOutcome::Completed(completion) = outcome else {
        panic!("successful check should complete");
    };
    assert_eq!(
        completion.output(),
        &BuiltinOutput::ForwardInput(Carrier::ByteStream)
    );

    let failed = Status::exit(7, Duration::ZERO).unwrap();
    let error = execute_builtin(
        check,
        Carrier::ByteStream,
        Some(&failed),
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect_err("unsuccessful status should become a runtime error");
    assert_eq!(
        error.kind(),
        &RuntimeErrorKind::UnsuccessfulStatus {
            status: Box::new(failed)
        }
    );
    assert_eq!(error.span(), check.span());
}

#[test]
fn builtins_reject_invalid_arity_and_exit_codes() {
    let probe = Probe::default();
    let mut session = state();
    let pwd = build("pwd extra", session.environment(), &probe);
    let error = execute_builtin(
        &pwd.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect_err("pwd takes no arguments");
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::BuiltinArity { .. }
    ));

    let exit = build("exit 256", session.environment(), &probe);
    let error = execute_builtin(
        &exit.stages()[0],
        Carrier::Empty,
        None,
        &mut session,
        &standard_registry(),
        &probe,
        &FakePlatform::none(),
    )
    .expect_err("exit code is bounded to one byte");
    assert!(matches!(error.kind(), RuntimeErrorKind::InvalidExitCode));
}
