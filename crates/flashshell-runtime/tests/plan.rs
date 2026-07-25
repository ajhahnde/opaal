#![forbid(unsafe_code)]

//! Turning one parsed command pipeline into an inspectable execution plan:
//! argv, resolved command, cwd, child environment, pipeline edges, and ordered
//! redirections, all retaining source spans, without spawning any process.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use flashshell_runtime::command::{Carrier, CommandRegistry, CommandSignature};
use flashshell_runtime::eval::RuntimeErrorKind;
use flashshell_runtime::plan::{
    ExecutionPlan, PlannedResolution, RedirectionAction, plan_pipeline,
};
use flashshell_runtime::resolve::ExecutableProbe;
use flashshell_runtime::{BindingMutability, Environment, ScopeStack, Value};
use flashshell_syntax::{
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
            name: "cd".to_owned(),
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
        flashshell_syntax::PipeOperator::StdoutAndStderr
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
fn expression_and_closure_stages_are_unsupported_in_a_plan() {
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

    // A closure command argument is deferred to structured pipelines.
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
cwd /work
env
  PATH=/bin
stage 0 external /bin/echo
  argv [echo] [hi]
  carriers in ByteStream out ByteStream
stage 1 external /bin/cat
  argv [cat]
  carriers in ByteStream out ByteStream
edge 0 | 1
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
cwd /work
env
stage 0 internal where
  argv [where]
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
cwd /work
env
  PATH=/bin
stage 0 external /bin/build
  argv [build]
  carriers in ByteStream out ByteStream
  redir 1> [out.txt]
  redir 2>> [err.txt]
  redir 1>&2
  redir 3>&-
"
    );
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
