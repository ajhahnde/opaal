#![forbid(unsafe_code)]

//! Preflight validation of a built execution plan before any stage is spawned:
//! rejecting NUL bytes in argv and redirection targets, ambiguous
//! structured-to-byte pipeline edges, and duplication from a descriptor that is
//! not open in a stage's descriptor map.

use flashshell_runtime::command::{Carrier, CommandRegistry, CommandSignature};
use flashshell_runtime::eval::{CarrierBridge, RuntimeError, RuntimeErrorKind};
use flashshell_runtime::plan::{ExecutionPlan, plan_pipeline, preflight};
use flashshell_runtime::resolve::ExecutableProbe;
use flashshell_runtime::{Environment, ScopeStack};
use flashshell_syntax::{ParseOutcome, Pipeline, SourceFile, SourceId, StatementKind, parse};

use std::ffi::{OsStr, OsString};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(1), "test.fsh", text)
}

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

/// Builds a plan over a `/bin` `PATH` with the given registry and probe.
fn build(text: &str, registry: &CommandRegistry, probe: &dyn ExecutableProbe) -> ExecutionPlan {
    let file = source(text);
    let pipeline = pipeline(&file);
    let mut scope = ScopeStack::new();
    let environment = Environment::from_snapshot([("PATH", "/bin")]);
    plan_pipeline(
        &pipeline,
        "/work",
        &file,
        &mut scope,
        &environment,
        registry,
        probe,
    )
    .unwrap_or_else(|error| panic!("planning failed for {text:?}: {:?}", error.kind()))
}

/// Builds and preflights a plan, returning any preflight error.
fn check(
    text: &str,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
) -> Result<(), RuntimeError> {
    preflight(&build(text, registry, probe))
}

fn empty() -> CommandRegistry {
    CommandRegistry::new()
}

#[test]
fn a_clean_plan_passes_preflight() {
    let probe = FakeProbe::with(&["/bin/echo"]);
    assert!(check("^echo hello > out.txt", &empty(), &probe).is_ok());
}

#[test]
fn a_nul_byte_in_an_argument_is_rejected() {
    let probe = FakeProbe::with(&["/bin/echo"]);
    let error = check("^echo \"a\\0b\"", &empty(), &probe).expect_err("nul argument");
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::ArgumentContainsNul
    ));
}

#[test]
fn a_nul_byte_in_a_redirection_target_is_rejected() {
    let probe = FakeProbe::with(&["/bin/cat"]);
    let error = check("^cat < \"f\\0\"", &empty(), &probe).expect_err("nul target");
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::ArgumentContainsNul
    ));
}

#[test]
fn a_value_to_external_edge_is_a_carrier_mismatch() {
    let mut registry = empty();
    registry.register(CommandSignature::new(
        "gen",
        [Carrier::Empty],
        Carrier::ValueStream,
    ));
    let probe = FakeProbe::with(&["/bin/cat"]);
    let file = source("gen | ^cat");
    let pipeline = pipeline(&file);
    let plan = build("gen | ^cat", &registry, &probe);
    let error = preflight(&plan).expect_err("carrier mismatch");
    let RuntimeErrorKind::CarrierMismatch(mismatch) = error.kind() else {
        panic!("expected a carrier mismatch, found {:?}", error.kind());
    };
    // The diagnostic names both commands, what the producer emits, and what the
    // consumer accepts, so the reader can see the exact edge that is wrong.
    assert_eq!(mismatch.producer_command, "gen");
    assert_eq!(mismatch.produced, Carrier::ValueStream);
    assert_eq!(mismatch.consumer_command, "cat");
    assert_eq!(mismatch.accepted, [Carrier::ByteStream]);
    // A structured producer into a byte consumer is bridged by serializing.
    assert_eq!(mismatch.bridge, Some(CarrierBridge::StructuredToByte));
    // The mismatch anchors on the pipe operator between the stages.
    assert_eq!(error.span(), pipeline.operators()[0].span());
}

#[test]
fn an_external_byte_stream_to_a_structured_only_internal_is_a_mismatch() {
    let mut registry = empty();
    registry.register(CommandSignature::new(
        "where",
        [Carrier::ValueStream],
        Carrier::ValueStream,
    ));
    let probe = FakeProbe::with(&["/bin/cat"]);
    let error = check("^cat | where", &registry, &probe).expect_err("carrier mismatch");
    let RuntimeErrorKind::CarrierMismatch(mismatch) = error.kind() else {
        panic!("expected a carrier mismatch, found {:?}", error.kind());
    };
    assert_eq!(mismatch.producer_command, "cat");
    assert_eq!(mismatch.produced, Carrier::ByteStream);
    assert_eq!(mismatch.consumer_command, "where");
    assert_eq!(mismatch.accepted, [Carrier::ValueStream]);
    // A byte producer into a structured consumer is bridged by parsing.
    assert_eq!(mismatch.bridge, Some(CarrierBridge::ByteToStructured));
}

#[test]
fn a_byte_passthrough_internal_connects_to_an_external_stage() {
    let mut registry = empty();
    registry.register(CommandSignature::new(
        "bytes",
        [Carrier::ByteStream],
        Carrier::ByteStream,
    ));
    let probe = FakeProbe::with(&["/bin/cat"]);
    assert!(check("^cat | bytes", &registry, &probe).is_ok());
    assert!(check("bytes | ^cat", &registry, &probe).is_ok());
}

#[test]
fn a_merged_stdout_stderr_edge_requires_a_byte_producer() {
    let mut registry = empty();
    registry.register(CommandSignature::new(
        "gen",
        [Carrier::Empty],
        Carrier::ValueStream,
    ));
    registry.register(CommandSignature::new(
        "sink",
        [Carrier::ValueStream],
        Carrier::ValueStream,
    ));
    // The accept check would pass (sink accepts ValueStream); `|&` still requires
    // a byte-stream producer, so the edge is rejected.
    let error = check("gen |& sink", &registry, &FakeProbe::with(&[])).expect_err("merge mismatch");
    let RuntimeErrorKind::MergedEdgeNotByteStream {
        producer_command,
        produced,
    } = error.kind()
    else {
        panic!("expected a merged-edge mismatch, found {:?}", error.kind());
    };
    assert_eq!(producer_command, "gen");
    assert_eq!(*produced, Carrier::ValueStream);
}

#[test]
fn a_structured_only_command_at_the_pipeline_head_is_rejected() {
    let mut registry = empty();
    // A command that consumes only a value stream cannot begin a pipeline: the
    // head input is an empty carrier, so it has nothing to consume.
    registry.register(CommandSignature::new(
        "each",
        [Carrier::ValueStream],
        Carrier::ValueStream,
    ));
    let file = source("each");
    let pipeline = pipeline(&file);
    let error = check("each", &registry, &FakeProbe::with(&[])).expect_err("head input");
    let RuntimeErrorKind::PipelineHeadInput { command, accepted } = error.kind() else {
        panic!("expected a head-input rejection, found {:?}", error.kind());
    };
    assert_eq!(command, "each");
    assert_eq!(accepted, &[Carrier::ValueStream]);
    // The rejection anchors on the offending head stage.
    assert_eq!(error.span(), pipeline.stages()[0].span());
}

#[test]
fn a_command_head_that_accepts_empty_input_is_a_valid_pipeline_start() {
    let mut registry = empty();
    registry.register(CommandSignature::new(
        "gen",
        [Carrier::Empty],
        Carrier::ValueStream,
    ));
    assert!(check("gen", &registry, &FakeProbe::with(&[])).is_ok());
}

#[test]
fn a_carrier_mismatch_message_names_both_commands_and_the_bridge() {
    let mut registry = empty();
    registry.register(CommandSignature::new(
        "gen",
        [Carrier::Empty],
        Carrier::ValueStream,
    ));
    let probe = FakeProbe::with(&["/bin/cat"]);
    let error = check("gen | ^cat", &registry, &probe).expect_err("carrier mismatch");
    let message = error.kind().to_string();
    assert!(
        message.contains("gen"),
        "message names the producer: {message}"
    );
    assert!(
        message.contains("cat"),
        "message names the consumer: {message}"
    );
    assert!(
        message.contains("encode") || message.contains("to"),
        "message suggests a serialization boundary: {message}"
    );
}

#[test]
fn duplicating_from_an_unopened_descriptor_is_rejected() {
    let probe = FakeProbe::with(&["/bin/build"]);
    let error = check("^build 3>&4", &empty(), &probe).expect_err("bad duplicate source");
    match error.kind() {
        RuntimeErrorKind::DescriptorNotOpen { descriptor } => assert_eq!(*descriptor, 4),
        other => panic!("expected DescriptorNotOpen, got {other:?}"),
    }
}

#[test]
fn duplicating_from_an_open_descriptor_passes() {
    let probe = FakeProbe::with(&["/bin/build"]);
    assert!(check("^build 2>&1 3>&2", &empty(), &probe).is_ok());
}

#[test]
fn closing_a_descriptor_then_duplicating_from_it_is_rejected() {
    let probe = FakeProbe::with(&["/bin/build"]);
    let error = check("^build 2>&- 1>&2", &empty(), &probe).expect_err("source was closed");
    match error.kind() {
        RuntimeErrorKind::DescriptorNotOpen { descriptor } => assert_eq!(*descriptor, 2),
        other => panic!("expected DescriptorNotOpen, got {other:?}"),
    }
}

#[test]
fn closing_an_absent_descriptor_is_a_successful_noop() {
    let probe = FakeProbe::with(&["/bin/build"]);
    assert!(check("^build 5>&-", &empty(), &probe).is_ok());
}
