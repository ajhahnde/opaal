#![forbid(unsafe_code)]

//! Acceptance coverage for source-spanned runtime errors and their innermost-first
//! call stack frames over the pure evaluator.

use flash_runtime::ScopeError;
use std::sync::Arc;

use flash_runtime::eval::{ErrorLabel, FrameCallee, RuntimeError, RuntimeErrorKind, evaluate};
use flash_runtime::operation;
use flash_runtime::{Duration, Record, ScopeStack, Status, Value};
use flash_syntax::{ParseOutcome, SourceFile, SourceId, parse};

/// Parses and evaluates `source`, returning the file so span text can be resolved.
fn run(source: &str) -> (SourceFile, Result<Value, RuntimeError>) {
    let file = SourceFile::new(SourceId::new(1), "test.fsh", source);
    let script = match parse(&file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}\n{source}"),
    };
    let mut scope = ScopeStack::new();
    let result = evaluate(&script, &file, &mut scope);
    (file, result)
}

fn error(source: &str) -> (SourceFile, RuntimeError) {
    let (file, result) = run(source);
    let error = result.expect_err(source);
    (file, error)
}

#[test]
fn top_level_error_has_no_frames() {
    // An unknown variable read at the top level never entered a call, so its
    // trace is empty and its primary span still points at the failing read.
    let (file, error) = error("$missing");
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding(name)) if name == "missing"
    ));
    assert_eq!(error.frames(), &[]);
    assert_eq!(file.slice(error.span()).unwrap(), "$missing");
}

#[test]
fn a_single_call_attaches_one_named_frame() {
    // The error is raised inside `boom`; the primary span stays on the failing
    // read while one frame names `boom` and points at the call site.
    let source = "\
def boom() {
    $missing
}
boom()";
    let (file, error) = error(source);
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding(name)) if name == "missing"
    ));
    assert_eq!(file.slice(error.span()).unwrap(), "$missing");

    let frames = error.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].callee(),
        &FrameCallee::Function("boom".to_owned())
    );
    assert_eq!(file.slice(frames[0].call_site()).unwrap(), "boom()");
}

#[test]
fn nested_calls_stack_innermost_first() {
    // `outer` calls `inner`, which fails. Frames read from the call nearest the
    // failure (inner) outward to the outermost call (outer).
    let source = "\
def inner() {
    $missing
}
def outer() {
    inner()
}
outer()";
    let (file, error) = error(source);
    let frames = error.frames();
    assert_eq!(frames.len(), 2);

    assert_eq!(
        frames[0].callee(),
        &FrameCallee::Function("inner".to_owned())
    );
    assert_eq!(file.slice(frames[0].call_site()).unwrap(), "inner()");

    assert_eq!(
        frames[1].callee(),
        &FrameCallee::Function("outer".to_owned())
    );
    assert_eq!(file.slice(frames[1].call_site()).unwrap(), "outer()");
}

#[test]
fn a_closure_call_attaches_an_anonymous_frame() {
    // A closure body failure attaches an anonymous frame carrying its call site.
    let source = "\
let boom = {|x| $missing}
$boom(1)";
    let (file, error) = error(source);
    let frames = error.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].callee(), &FrameCallee::Closure);
    assert_eq!(file.slice(frames[0].call_site()).unwrap(), "$boom(1)");
}

#[test]
fn errors_raised_before_body_entry_carry_no_frame() {
    // An arity mismatch is detected in the caller's context before the body is
    // entered, so no frame is attributed to the attempted call.
    let source = "\
def one(a) {
    $a
}
one(1, 2)";
    let (_file, arity_error) = error(source);
    assert!(matches!(
        arity_error.kind(),
        RuntimeErrorKind::ArityMismatch {
            expected: 1,
            actual: 2
        }
    ));
    assert_eq!(arity_error.frames(), &[]);

    // An error while evaluating an argument is likewise in the caller's context.
    let (_file, argument_error) = error("def id(x) {\n    $x\n}\nid($missing)");
    assert!(matches!(
        argument_error.kind(),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding(name)) if name == "missing"
    ));
    assert_eq!(argument_error.frames(), &[]);
}

#[test]
fn catch_exposes_structured_error_fields_and_call_frames() {
    let source = "\
mut observed = []
def boom() {
    $missing
}
try {
    boom()
} catch error {
    $observed = [
        $error.category,
        $error.message,
        $error.source.name,
        $error.frames[0].callee,
        $error.cause,
        $error.status,
    ]
}
$observed";
    assert_eq!(
        run(source).1.expect("runtime error should be caught"),
        Value::list(vec![
            Value::string("name"),
            Value::string("unknown binding \"missing\""),
            Value::string("test.fsh"),
            Value::string("boom"),
            Value::Null,
            Value::Null,
        ])
    );
}

#[test]
fn catch_preserves_nested_closure_and_function_frames() {
    let source = "\
mut observed = []
let fail = {|| $missing}
def outer(callable) {
    $callable()
}
try {
    outer($fail)
} catch error {
    $observed = [$error.frames[0].callee, $error.frames[1].callee]
}
$observed";
    assert_eq!(
        run(source)
            .1
            .expect("nested callable error should be caught"),
        Value::list(vec![Value::string("<closure>"), Value::string("outer")])
    );
}

#[test]
fn catch_rolls_back_language_state_to_the_pre_try_checkpoint() {
    let source = "\
mut value = 1
mut observed = []
export SAMPLE = \"before\"
try {
    $value = 2
    export SAMPLE = \"inside\"
    throw \"stop\"
} catch error {
    $observed = [$value, env(\"SAMPLE\"), $error.message]
}
$observed";
    assert_eq!(
        run(source).1.expect("thrown string should be caught"),
        Value::list(vec![
            Value::Int(1),
            Value::string("before"),
            Value::string("stop"),
        ])
    );
}

#[test]
fn rethrow_preserves_the_original_structured_error_and_source() {
    let source = "\
mut observed = []
try {
    try {
        throw \"preserved\"
    } catch inner {
        throw $inner
    }
} catch outer {
    $observed = [$outer.category, $outer.message, $outer.source.name, $outer == $outer]
}
$observed";
    assert_eq!(
        run(source).1.expect("rethrow should reach outer catch"),
        Value::list(vec![
            Value::string("user"),
            Value::string("preserved"),
            Value::string("test.fsh"),
            Value::Bool(true),
        ])
    );
}

#[test]
fn catch_binding_is_immutable_and_does_not_escape_its_block() {
    let (_file, immutable) = error("try { throw \"x\" } catch error { $error = null }");
    assert!(matches!(
        immutable.kind(),
        RuntimeErrorKind::Scope(ScopeError::ImmutableBinding(name)) if name == "error"
    ));

    let (_file, missing) = error("try { throw \"x\" } catch error { null }\n$error");
    assert!(matches!(
        missing.kind(),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding(name)) if name == "error"
    ));
}

#[test]
fn throw_rejects_non_string_non_error_values() {
    let (_file, error) = error("throw 42");
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::ThrowValueNotErrorOrString { actual: "int" }
    ));
}

#[test]
fn error_values_preserve_labels_causes_status_equality_and_rendering() {
    let (file, outer) = error("throw \"outer\"");
    let (_, cause) = error("throw \"cause\"");
    let label_source = Arc::new(SourceFile::new(
        SourceId::new(2),
        "library.fsh",
        "secondary",
    ));
    let label_span = label_source.span(0..9).expect("valid label span");
    let enriched = outer
        .with_label(ErrorLabel::new(
            Arc::clone(&label_source),
            label_span,
            "related source",
        ))
        .with_cause(Arc::new(cause));
    let value = Value::Error(Arc::new(enriched.clone()));

    assert_eq!(value, Value::Error(Arc::new(enriched)));
    assert_eq!(format!("{value}"), "outer");
    assert_eq!(format!("{value:?}"), "error(user: \"outer\")");
    assert_eq!(
        operation::field(&value, "source").expect("source field"),
        Value::Record(
            Record::new(vec![
                ("name".to_owned(), Value::string("test.fsh")),
                ("start".to_owned(), Value::Int(6)),
                ("end".to_owned(), Value::Int(13)),
            ])
            .expect("distinct source fields"),
        )
    );
    assert_eq!(
        operation::field(&value, "labels").expect("labels field"),
        Value::list(vec![Value::Record(
            Record::new(vec![
                ("name".to_owned(), Value::string("library.fsh")),
                ("start".to_owned(), Value::Int(0)),
                ("end".to_owned(), Value::Int(9)),
                ("message".to_owned(), Value::string("related source")),
            ])
            .expect("distinct label fields"),
        )])
    );
    assert_eq!(
        operation::field(&value, "cause")
            .expect("nested cause field")
            .family_name(),
        "error"
    );

    let status = Status::exit(7, Duration::from_nanos(3)).expect("valid status");
    let status_error = Value::Error(Arc::new(RuntimeError::new(
        RuntimeErrorKind::UnsuccessfulStatus {
            status: Box::new(status.clone()),
        },
        file.span(0..1).expect("valid status error span"),
    )));
    assert_eq!(
        operation::field(&status_error, "status").expect("status field"),
        Value::Status(status)
    );
}
