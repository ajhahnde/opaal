#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::Path;

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::{SessionState, standard_registry};
use opaal_runtime::eval::RuntimeErrorKind;
use opaal_runtime::internal::{
    InternalPayload, InternalPipelineOutcome, execute_internal_pipeline, execute_internal_suffix,
};
use opaal_runtime::plan::{ExecutionPlan, plan_pipeline, preflight};
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::stream::StreamPull;
use opaal_runtime::{AdmittedValue, Duration, Environment, ScopeStack, Status, Value};
use opaal_syntax::{ParseOutcome, SourceFile, SourceId, StatementKind, parse_opaal};

struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}

fn plan(text: &str) -> (SourceFile, ExecutionPlan) {
    let source = SourceFile::new(SourceId::new(1), "core-command-usage.opaal", text);
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("test source must parse");
    };
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("test source must contain one command job");
    };
    let pipeline = &job.chain.or_terms()[0].and_terms()[0];
    let registry = standard_registry();
    let plan = plan_pipeline(
        pipeline,
        Path::new("/core-command-usage"),
        &source,
        &mut ScopeStack::new(),
        &Environment::new(),
        &registry,
        &NoExecutables,
    )
    .expect("test source must plan");
    (source, plan)
}

#[test]
fn registry_invocations_report_every_accepted_argument_form() {
    let registry = standard_registry();

    for (name, invocation, minimum, maximum) in [
        ("from", "from FORMAT [MODE]", 1, Some(2)),
        ("fg", "fg [%JOB]", 0, Some(1)),
        ("bg", "bg [%JOB]", 0, Some(1)),
        ("wait", "wait [%JOB...]", 0, None),
        ("kill", "kill [SIGNAL] %JOB...", 1, None),
    ] {
        let signature = registry
            .lookup(name)
            .unwrap_or_else(|| panic!("{name} must be registered"));
        assert_eq!(signature.documentation().invocation(), invocation, "{name}");
        assert_eq!(signature.arguments().minimum(), minimum, "{name}");
        assert_eq!(signature.arguments().maximum(), maximum, "{name}");
    }
}

#[test]
fn from_optional_json_mode_reaches_the_internal_parser() {
    let (source, plan) = plan("help | from json array\n");
    let registry = standard_registry();
    let outcome = execute_internal_pipeline(
        &plan,
        &mut SessionState::new(Path::new("/core-command-usage"), Environment::new()),
        &registry,
        &NoExecutables,
        &FakePlatform::full(),
        &source,
    )
    .expect("the optional mode must be accepted before the lazy parser runs");
    let InternalPipelineOutcome::Completed {
        payload: InternalPayload::ValueStream(mut values),
        ..
    } = outcome
    else {
        panic!("from must produce a value stream");
    };

    assert!(matches!(
        values.pull(),
        StreamPull::Failed(error)
            if matches!(
                error.kind(),
                RuntimeErrorKind::StructuredCommand { command: "from", message }
                    if message.contains("malformed JSON")
            )
    ));
}

#[test]
fn internal_suffix_accepts_an_admitted_head_value() {
    let (source, plan) = plan("help\n");
    preflight(&plan).unwrap();
    let value = Value::Tuple([Value::Int(7), Value::string("ready")].into());
    let payload = InternalPayload::Value(AdmittedValue::new(&value).unwrap());
    let status = Status::exit(0, Duration::ZERO).unwrap();
    let outcome = execute_internal_suffix(
        &plan,
        1,
        payload,
        vec![status.clone()],
        &mut SessionState::new(Path::new("/core-command-usage"), Environment::new()),
        &standard_registry(),
        &NoExecutables,
        &FakePlatform::full(),
        &source,
    )
    .unwrap();
    assert!(matches!(
        outcome,
        InternalPipelineOutcome::Completed {
            payload: InternalPayload::Value(output),
            status: completed,
            ..
        } if output == value && completed == status
    ));
}
