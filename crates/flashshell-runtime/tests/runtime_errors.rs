#![forbid(unsafe_code)]

//! Acceptance coverage for source-spanned runtime errors and their innermost-first
//! call stack frames over the pure evaluator.

use flashshell_runtime::ScopeError;
use flashshell_runtime::eval::{FrameCallee, RuntimeError, RuntimeErrorKind, evaluate};
use flashshell_runtime::{ScopeStack, Value};
use flashshell_syntax::{ParseOutcome, SourceFile, SourceId, parse};

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
