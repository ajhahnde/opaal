#![forbid(unsafe_code)]

//! Turning one parsed command pipeline into an inspectable execution plan:
//! argv, resolved command, cwd, child environment, pipeline edges, and ordered
//! redirections, all retaining source spans, without spawning any process.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use flash_runtime::builtin::standard_registry;
use flash_runtime::command::{
    Carrier, CommandLifecycle, CommandNamespaceEntry, CommandRegistry, CommandSignature,
};
use flash_runtime::eval::RuntimeErrorKind;
use flash_runtime::plan::{
    ExecutionPlan, PlannedArgument, PlannedResolution, RedirectionAction, plan_pipeline,
};
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::{BindingMutability, Environment, ScopeStack, Value};
use flash_syntax::{
    OutputMode, ParseOutcome, Pipeline, SourceFile, SourceId, StageKind, StatementKind, parse,
};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(1), "test.fsh", text)
}

/// Parses one bare command statement and returns its single pipeline.
fn pipeline(file: &SourceFile) -> Pipeline {
    let script = match parse(file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}"),
    };
    let statement = &script.statements()[0];
    let StatementKind::Job(job) = statement.kind() else {
        panic!("expected a bare command statement");
    };
    job.chain.or_terms()[0].and_terms()[0].clone()
}

/// An executable probe that accepts a fixed set of native paths.
struct FakeProbe {
    executables: Vec<OsString>,
}

impl FakeProbe {
    fn with(paths: &[&str]) -> Self {
        Self {
            executables: paths.iter().map(OsString::from).collect(),
        }
    }
}

impl ExecutableProbe for FakeProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        self.executables.iter().any(|candidate| candidate == path)
    }
}

/// Plans one pipeline over a `/bin`-only `PATH` and a chosen probe/registry/scope.
fn plan_with(
    text: &str,
    cwd: &str,
    scope: &mut ScopeStack,
    environment: &Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
) -> Result<ExecutionPlan, RuntimeErrorKind> {
    let file = source(text);
    let pipeline = pipeline(&file);
    plan_pipeline(&pipeline, cwd, &file, scope, environment, registry, probe)
        .map_err(|error| error.kind().clone())
}

fn argv_values(plan: &ExecutionPlan, stage: usize) -> Vec<OsString> {
    plan.stages()[stage]
        .argv()
        .iter()
        .map(|word| word.value().to_os_string())
        .collect()
}

#[test]
fn external_command_plans_argv_resolution_cwd_and_spans() {
    let file = source("^echo hello world");
    let pipeline = pipeline(&file);
    let probe = FakeProbe::with(&["/bin/echo"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);
    let registry = CommandRegistry::new();
    let plan = plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("plan");

    assert_eq!(plan.cwd(), Path::new("/work"));
    assert_eq!(plan.span(), pipeline.span());
    assert!(plan.edges().is_empty());
    assert_eq!(plan.stages().len(), 1);

    let stage = &plan.stages()[0];
    assert_eq!(stage.span(), pipeline.stages()[0].span());
    assert!(stage.redirections().is_empty());
    assert_eq!(
        argv_values(&plan, 0),
        vec![
            OsString::from("echo"),
            OsString::from("hello"),
            OsString::from("world"),
        ]
    );
    assert_eq!(
        stage.resolution(),
        &PlannedResolution::External {
            path: PathBuf::from("/bin/echo"),
        }
    );
}

#[test]
fn command_lowers_to_the_external_stage_contract() {
    let registry = flash_runtime::builtin::standard_registry();
    let probe = FakeProbe::with(&["/bin/tool"]);
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "program",
            BindingMutability::Immutable,
            Value::string("tool"),
        )
        .expect("declare program");
    scope
        .declare(
            "arguments",
            BindingMutability::Immutable,
            Value::list(vec![Value::string("two words"), Value::string("")]),
        )
        .expect("declare arguments");
    let environment = Environment::from_snapshot([("PATH", "/bin")]);

    let plan = plan_with(
        "command $program ...$arguments",
        "/work",
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("the command stage should lower");
    let stage = &plan.stages()[0];

    assert_eq!(
        stage.resolution(),
        &PlannedResolution::External {
            path: PathBuf::from("/bin/tool"),
        }
    );
    assert_eq!(
        argv_values(&plan, 0),
        [
            OsString::from("tool"),
            OsString::from("two words"),
            OsString::new(),
        ]
    );
    assert!(stage.arguments().is_empty());
    assert!(stage.accepts_input(Carrier::ByteStream));
    assert_eq!(stage.output_carrier(), Carrier::ByteStream);
}

#[test]
fn command_requires_a_target_during_planning() {
    let error = plan_with(
        "command",
        "/work",
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &flash_runtime::builtin::standard_registry(),
        &FakeProbe::with(&[]),
    )
    .expect_err("command without a target should fail");

    assert!(matches!(
        error,
        RuntimeErrorKind::BuiltinArity {
            command: "command",
            minimum: 1,
            maximum: None,
            actual: 0,
        }
    ));
}

#[test]
fn command_treats_a_double_dash_target_as_a_literal_external_name() {
    let plan = plan_with(
        "command -- tool",
        "/work",
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &standard_registry(),
        &FakeProbe::with(&["/bin/--"]),
    )
    .expect("command owns a literal double-dash policy");

    assert_eq!(
        plan.stages()[0].resolution(),
        &PlannedResolution::External {
            path: PathBuf::from("/bin/--"),
        }
    );
    assert_eq!(
        argv_values(&plan, 0),
        [OsString::from("--"), OsString::from("tool")]
    );
}

#[test]
fn builtin_planning_normalizes_terminators_and_validates_expanded_dynamic_tails() {
    let registry = standard_registry();
    let environment = Environment::new();
    let probe = FakeProbe::with(&[]);
    let plan = plan_with(
        "which -- --literal",
        "/work",
        &mut ScopeStack::new(),
        &environment,
        &registry,
        &probe,
    )
    .expect("the option terminator protects a dash-leading positional");
    assert_eq!(
        argv_values(&plan, 0),
        [OsString::from("which"), OsString::from("--literal")]
    );
    assert_eq!(plan.stages()[0].arguments().len(), 1);

    let mut scope = ScopeStack::new();
    scope
        .declare(
            "arguments",
            BindingMutability::Immutable,
            Value::list(vec![Value::string("one"), Value::string("two")]),
        )
        .expect("declare spread");
    let error = plan_with(
        "pwd ...$arguments",
        "/work",
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect_err("expanded spreads receive exact runtime schema validation");
    assert!(matches!(
        error,
        RuntimeErrorKind::BuiltinArity {
            command: "pwd",
            minimum: 0,
            maximum: Some(0),
            actual: 2,
        }
    ));

    let error = plan_with(
        "kill --stop --kill %1",
        "/work",
        &mut ScopeStack::new(),
        &environment,
        &registry,
        &probe,
    )
    .expect_err("conflicting options are rejected from the shared schema");
    assert!(matches!(
        error,
        RuntimeErrorKind::BuiltinArgument { command: "kill", message }
            if message.contains("conflict")
    ));
}

#[cfg(unix)]
#[test]
fn command_preserves_a_native_non_utf8_target_as_argv_zero() {
    use std::os::unix::ffi::OsStringExt;

    let target = OsString::from_vec(b"/bin/tool-\xff".to_vec());
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "program",
            BindingMutability::Immutable,
            Value::Path(flash_runtime::NativePath::new(target.clone())),
        )
        .expect("declare native program");
    let plan = plan_with(
        "command $program argument",
        "/work",
        &mut scope,
        &Environment::new(),
        &flash_runtime::builtin::standard_registry(),
        &FakeProbe {
            executables: vec![target.clone()],
        },
    )
    .expect("native command target should lower");

    assert_eq!(
        plan.stages()[0].resolution(),
        &PlannedResolution::External {
            path: PathBuf::from(target.clone()),
        }
    );
    assert_eq!(plan.stages()[0].argv()[0].value(), target.as_os_str());
}

#[test]
fn bare_command_resolves_internal_before_external() {
    let mut registry = CommandRegistry::new();
    registry.register(CommandSignature::new(
        "cd",
        [Carrier::Empty],
        Carrier::Empty,
    ));
    // `cd` is also present on PATH, but the internal command must win.
    let probe = FakeProbe::with(&["/bin/cd"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);

    let plan = plan_with("cd", "/work", &mut scope, &environment, &registry, &probe).expect("plan");
    assert_eq!(
        plan.stages()[0].resolution(),
        &PlannedResolution::Internal {
            source_name: "cd".to_owned(),
            canonical_name: "cd".to_owned(),
        }
    );
}

#[test]
fn alias_planning_retains_source_spelling_and_canonical_executor() {
    let registry = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(
                CommandSignature::new("pwd", [Carrier::Empty], Carrier::Value),
                CommandLifecycle::introduced(1),
            ),
            CommandNamespaceEntry::alias("cwd", "pwd", CommandLifecycle::introduced(1)),
        ],
    )
    .expect("valid namespace");
    let probe = FakeProbe::with(&["/bin/cwd"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);

    let plan =
        plan_with("cwd", "/work", &mut scope, &environment, &registry, &probe).expect("plan");

    assert_eq!(
        plan.stages()[0].resolution(),
        &PlannedResolution::Internal {
            source_name: "cwd".to_owned(),
            canonical_name: "pwd".to_owned(),
        }
    );
    assert_eq!(plan.stages()[0].output_carrier(), Carrier::Value);
}

#[test]
fn reserved_planning_fails_before_path_with_structured_guidance() {
    let registry = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(
                CommandSignature::new("pwd", [Carrier::Empty], Carrier::Value),
                CommandLifecycle::introduced(1),
            ),
            CommandNamespaceEntry::reserved(
                "future",
                1,
                "reserved for a future command",
                Some("pwd"),
            ),
        ],
    )
    .expect("valid namespace");
    let probe = FakeProbe::with(&["/bin/future"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);

    let error = plan_with(
        "future",
        "/work",
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect_err("reserved bare name");

    let rendered = error.to_string();
    assert!(rendered.contains("command `future` is reserved"));
    assert!(rendered.contains("use `pwd` instead"));
    assert!(rendered.contains("`^future`"));
    assert!(rendered.contains("`command future`"));

    assert!(matches!(
        error,
        RuntimeErrorKind::ReservedCommand(details)
            if details.name() == "future"
                && details.purpose() == "reserved for a future command"
                && details.replacement() == Some("pwd")
    ));
}

#[test]
fn forced_external_planning_bypasses_a_reservation() {
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
    let probe = FakeProbe::with(&["/bin/future"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);

    let plan = plan_with(
        "^future",
        "/work",
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("forced external plan");

    assert_eq!(
        plan.stages()[0].resolution(),
        &PlannedResolution::External {
            path: PathBuf::from("/bin/future"),
        }
    );
}

#[test]
fn forced_external_skips_the_registry() {
    let mut registry = CommandRegistry::new();
    registry.register(CommandSignature::new(
        "echo",
        [Carrier::Empty],
        Carrier::ByteStream,
    ));
    let probe = FakeProbe::with(&["/bin/echo"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);

    let plan = plan_with(
        "^echo hi",
        "/work",
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("plan");
    assert_eq!(
        plan.stages()[0].resolution(),
        &PlannedResolution::External {
            path: PathBuf::from("/bin/echo"),
        }
    );
}

#[test]
fn argv_includes_spread_arguments_in_source_order() {
    let file = source("^ls first ...$args last");
    let pipeline = pipeline(&file);
    let probe = FakeProbe::with(&["/bin/ls"]);
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "args",
            BindingMutability::Immutable,
            Value::list(vec![Value::string("-l"), Value::string("-a")]),
        )
        .expect("declare args");
    let environment = Environment::from_snapshot([("PATH", "/bin")]);
    let registry = CommandRegistry::new();
    let plan = plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("plan");

    assert_eq!(
        argv_values(&plan, 0),
        vec![
            OsString::from("ls"),
            OsString::from("first"),
            OsString::from("-l"),
            OsString::from("-a"),
            OsString::from("last"),
        ]
    );
}

#[test]
fn pipeline_edges_record_operator_kind_and_span() {
    let file = source("^a | ^b");
    let pipeline = pipeline(&file);
    let probe = FakeProbe::with(&["/bin/a", "/bin/b"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);
    let registry = CommandRegistry::new();
    let plan = plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("plan");

    assert_eq!(plan.stages().len(), 2);
    assert_eq!(plan.edges().len(), 1);
    let edge = &plan.edges()[0];
    assert_eq!(edge.kind(), *pipeline.operators()[0].kind());
    assert_eq!(edge.operator_span(), pipeline.operators()[0].span());
    assert_eq!(argv_values(&plan, 0), vec![OsString::from("a")]);
    assert_eq!(argv_values(&plan, 1), vec![OsString::from("b")]);
}

#[test]
fn stdout_and_stderr_edge_is_distinct() {
    let file = source("^a |& ^b");
    let pipeline = pipeline(&file);
    let probe = FakeProbe::with(&["/bin/a", "/bin/b"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);
    let registry = CommandRegistry::new();
    let plan = plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("plan");
    assert_eq!(
        plan.edges()[0].kind(),
        flash_syntax::PipeOperator::StdoutAndStderr
    );
}

#[test]
fn redirections_are_ordered_with_descriptors_modes_and_targets() {
    let file = source("^build > out.txt 2>> err.txt < in.txt");
    let pipeline = pipeline(&file);
    let probe = FakeProbe::with(&["/bin/build"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);
    let registry = CommandRegistry::new();
    let plan = plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("plan");

    // Redirection targets are not argv.
    assert_eq!(argv_values(&plan, 0), vec![OsString::from("build")]);

    let redirections = plan.stages()[0].redirections();
    assert_eq!(redirections.len(), 3);
    match redirections[0].action() {
        RedirectionAction::Output {
            descriptor,
            mode,
            target,
            ..
        } => {
            assert_eq!(*descriptor, 1);
            assert_eq!(*mode, OutputMode::Truncate);
            assert_eq!(target.value(), OsStr::new("out.txt"));
        }
        other => panic!("expected an output redirection, got {other:?}"),
    }
    match redirections[1].action() {
        RedirectionAction::Output {
            descriptor,
            mode,
            target,
            ..
        } => {
            assert_eq!(*descriptor, 2);
            assert_eq!(*mode, OutputMode::Append);
            assert_eq!(target.value(), OsStr::new("err.txt"));
        }
        other => panic!("expected an append redirection, got {other:?}"),
    }
    match redirections[2].action() {
        RedirectionAction::Input {
            descriptor, target, ..
        } => {
            assert_eq!(*descriptor, 0);
            assert_eq!(target.value(), OsStr::new("in.txt"));
        }
        other => panic!("expected an input redirection, got {other:?}"),
    }
}

#[test]
fn duplicate_and_close_descriptors_are_planned() {
    let file = source("^build 2>&1 3>&-");
    let pipeline = pipeline(&file);
    let probe = FakeProbe::with(&["/bin/build"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);
    let registry = CommandRegistry::new();
    let plan = plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("plan");

    let redirections = plan.stages()[0].redirections();
    assert_eq!(redirections.len(), 2);
    match redirections[0].action() {
        RedirectionAction::Duplicate {
            descriptor, source, ..
        } => {
            assert_eq!(*descriptor, 2);
            assert_eq!(*source, 1);
        }
        other => panic!("expected a duplicate redirection, got {other:?}"),
    }
    match redirections[1].action() {
        RedirectionAction::Close { descriptor, .. } => assert_eq!(*descriptor, 3),
        other => panic!("expected a close redirection, got {other:?}"),
    }
}

#[test]
fn plan_carries_the_child_environment_and_cwd() {
    let probe = FakeProbe::with(&["/bin/echo"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin"), ("EDITOR", "helix")]);
    let registry = CommandRegistry::new();
    let plan = plan_with(
        "^echo",
        "/home/me",
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect("plan");

    assert_eq!(plan.cwd(), Path::new("/home/me"));
    assert_eq!(plan.environment().get("EDITOR"), Some(OsStr::new("helix")));
}

#[test]
fn missing_command_is_a_resolution_error_at_the_head_span() {
    let file = source("^missing arg");
    let pipeline = pipeline(&file);
    let probe = FakeProbe::with(&["/bin/echo"]);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);
    let registry = CommandRegistry::new();
    let error = plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut scope,
        &environment,
        &registry,
        &probe,
    )
    .expect_err("resolution should fail");

    let StageKind::Command(stage) = pipeline.stages()[0].kind() else {
        panic!("expected a command stage");
    };
    assert_eq!(error.span(), stage.head.span());
    match error.kind() {
        RuntimeErrorKind::CommandNotFound { name } => {
            assert_eq!(name.as_os_str(), OsStr::new("missing"));
        }
        other => panic!("expected CommandNotFound, got {other:?}"),
    }
}

#[test]
fn expression_stages_and_external_closure_arguments_are_unsupported_in_a_plan() {
    let probe = FakeProbe::with(&["/bin/map"]);

    // A pure expression stage is not a command plan.
    let expr_err = plan_with(
        "(1 + 2)",
        "/work",
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &probe,
    )
    .expect_err("expression stage");
    assert!(matches!(expr_err, RuntimeErrorKind::Unsupported { .. }));

    // A closure is typed runtime data and cannot become an external argv word.
    let closure_err = plan_with(
        "^map {|item| item}",
        "/work",
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &probe,
    )
    .expect_err("closure argument");
    assert!(matches!(closure_err, RuntimeErrorKind::Unsupported { .. }));
}

#[test]
fn an_internal_closure_argument_is_captured_without_entering_native_argv() {
    let file = source("which pwd | each {|item| $item.name}");
    let pipeline = pipeline(&file);
    let plan = plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &standard_registry(),
        &FakeProbe::with(&[]),
    )
    .expect("an internal closure argument should plan");

    let stage = &plan.stages()[1];
    assert_eq!(argv_values(&plan, 1), [OsString::from("each")]);
    let [PlannedArgument::Value { value, span }] = stage.arguments() else {
        panic!("each should retain one typed closure argument");
    };
    assert!(matches!(value, Value::Callable(_)));
    assert_eq!(
        file.slice(*span),
        Ok("{|item| $item.name}"),
        "the typed argument retains its exact source span"
    );
    assert!(
        plan.render().contains("[<closure at test.fsh:1:18>]"),
        "plan inspection includes typed arguments without calling them argv"
    );
}

/// Builds a plan, panicking on any planning error, for render assertions.
fn planned(
    text: &str,
    cwd: &str,
    environment: &Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
) -> ExecutionPlan {
    let file = source(text);
    let pipeline = pipeline(&file);
    plan_pipeline(
        &pipeline,
        cwd,
        &file,
        &mut ScopeStack::new(),
        environment,
        registry,
        probe,
    )
    .expect("planning should succeed")
}

#[test]
fn render_prints_an_external_byte_pipeline_without_executing() {
    let plan = planned(
        "^echo hi | ^cat",
        "/work",
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &FakeProbe::with(&["/bin/echo", "/bin/cat"]),
    );

    assert_eq!(
        plan.render(),
        "\
plan span 0..15
cwd [/work]
env
  [PATH]=[/bin]
pipefail false
capture-limit 8388608
process-group isolate
stage 0 span 0..8 external [/bin/echo]
  argv
    0 span 1..5 [echo]
    1 span 6..8 [hi]
  arguments
    0 word span 6..8 [hi]
  carriers in ByteStream out ByteStream
stage 1 span 11..15 external [/bin/cat]
  argv
    0 span 12..15 [cat]
  carriers in ByteStream out ByteStream
edge 0 span 9..10 | 1
"
    );
}

#[test]
fn render_shows_internal_resolution_and_carrier_contract() {
    let mut registry = CommandRegistry::new();
    registry.register(CommandSignature::new(
        "where",
        [Carrier::Value, Carrier::ValueStream],
        Carrier::ValueStream,
    ));
    let plan = planned(
        "where",
        "/work",
        &Environment::new(),
        &registry,
        &FakeProbe::with(&[]),
    );

    assert_eq!(
        plan.render(),
        "\
plan span 0..5
cwd [/work]
env
pipefail false
capture-limit 8388608
process-group isolate
stage 0 span 0..5 internal where
  argv
    0 span 0..5 [where]
  carriers in Value|ValueStream out ValueStream
"
    );
}

#[test]
fn render_prints_redirections_in_source_order() {
    let plan = planned(
        "^build > out.txt 2>> err.txt 1>&2 3>&-",
        "/work",
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &FakeProbe::with(&["/bin/build"]),
    );

    assert_eq!(
        plan.render(),
        "\
plan span 0..38
cwd [/work]
env
  [PATH]=[/bin]
pipefail false
capture-limit 8388608
process-group isolate
stage 0 span 0..38 external [/bin/build]
  argv
    0 span 1..6 [build]
  carriers in ByteStream out ByteStream
  redir span 7..16 1> operator-span 7..8 target-span 9..16 [out.txt]
  redir span 17..28 2>> operator-span 18..20 target-span 21..28 [err.txt]
  redir span 29..33 1>&2 operator-span 30..32 target-span 32..33
  redir span 34..38 3>&- operator-span 35..37 target-span 37..38
"
    );
}

#[cfg(unix)]
#[test]
fn render_escapes_native_non_utf8_without_collapsing_distinct_plans() {
    use std::os::unix::ffi::OsStringExt;

    let executable = OsString::from_vec(b"/bin/tool-\xff".to_vec());
    let argument = OsString::from_vec(b"argument-\xfe".to_vec());
    let target = OsString::from_vec(b"target-\xfd".to_vec());
    let cwd = OsString::from_vec(b"/work-\xfc".to_vec());
    let environment =
        Environment::from_snapshot([("VALUE", OsString::from_vec(b"native-\xfb".to_vec()))]);
    let mut scope = ScopeStack::new();
    for (name, value) in [
        ("tool", executable.clone()),
        ("argument", argument),
        ("target", target),
    ] {
        scope
            .declare(
                name,
                BindingMutability::Immutable,
                Value::Path(flash_runtime::NativePath::new(value)),
            )
            .unwrap();
    }
    let registry = CommandRegistry::new();
    let file = source("^$tool $argument > $target");
    let pipeline = pipeline(&file);
    let plan = plan_pipeline(
        &pipeline,
        PathBuf::from(cwd),
        &file,
        &mut scope,
        &environment,
        &registry,
        &FakeProbe {
            executables: vec![executable],
        },
    )
    .expect("native values should plan");

    let rendered = plan.render();
    for expected in [
        "[/work-\\xfc]",
        "[native-\\xfb]",
        "external [/bin/tool-\\xff]",
        "[argument-\\xfe]",
        "[target-\\xfd]",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected:?}: {rendered}"
        );
    }
    assert!(!rendered.contains('\u{fffd}'));
}

#[test]
fn a_descriptor_beyond_u32_is_a_plan_error() {
    let error = plan_with(
        "^build 5000000000> out.txt",
        "/work",
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &FakeProbe::with(&["/bin/build"]),
    )
    .expect_err("descriptor overflow");
    assert!(matches!(
        error,
        RuntimeErrorKind::RedirectionDescriptorOverflow
    ));
}
