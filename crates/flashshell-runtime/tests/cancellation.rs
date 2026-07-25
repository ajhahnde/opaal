#![forbid(unsafe_code)]

//! Acceptance coverage for cooperative cancellation checks at loop and call
//! boundaries over the pure evaluator. Cancellation is a distinct `Completion`,
//! never a `RuntimeError` and never a script value.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flashshell_runtime::eval::{
    CancelReason, CancellationToken, Completion, evaluate_with_cancellation,
};
use flashshell_runtime::{ScopeStack, Value};
use flashshell_syntax::{ParseOutcome, SourceFile, SourceId, parse};

/// Evaluates `source` under `token`, returning the file and the top-level outcome.
fn run(source: &str, token: &CancellationToken) -> (SourceFile, Completion) {
    let file = SourceFile::new(SourceId::new(1), "test.fsh", source);
    let script = match parse(&file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}\n{source}"),
    };
    let mut scope = ScopeStack::new();
    let completion = evaluate_with_cancellation(&script, &file, &mut scope, token)
        .unwrap_or_else(|error| panic!("evaluation errored: {error:?}\n{source}"));
    (file, completion)
}

#[test]
fn a_never_token_runs_to_a_value() {
    // Without cancellation a bounded loop completes and yields the final value.
    let source = "\
mut total = 0
for n in [1, 2, 3] {
    $total = $total + $n
}
$total";
    let (_file, completion) = run(source, &CancellationToken::never());
    match completion {
        Completion::Value(value) => assert_eq!(value, Value::Int(6)),
        Completion::Cancelled(cancellation) => {
            panic!("unexpected cancellation: {cancellation:?}")
        }
    }
}

#[test]
fn a_cancelled_token_stops_an_unbounded_loop() {
    // An already-cancelled token trips at the loop-condition boundary, so a loop
    // that would otherwise never terminate stops with a cancellation anchored on
    // the condition. Reaching the assertion at all proves it did not hang.
    let source = "while true {\n}";
    let (file, completion) = run(source, &CancellationToken::from_fn(|| true));
    match completion {
        Completion::Cancelled(cancellation) => {
            assert_eq!(cancellation.reason(), CancelReason::Requested);
            assert_eq!(file.slice(cancellation.span()).unwrap(), "true");
        }
        Completion::Value(value) => panic!("expected cancellation, got value: {value:?}"),
    }
}

#[test]
fn cancellation_is_polled_before_each_iteration() {
    // A token that trips on its fourth poll lets exactly three iterations run: the
    // boundary is checked once per loop turn before the condition.
    let polls = Arc::new(AtomicUsize::new(0));
    let token = CancellationToken::from_fn({
        let polls = Arc::clone(&polls);
        move || polls.fetch_add(1, Ordering::SeqCst) >= 3
    });

    let (_file, completion) = run("while true {\n}", &token);
    assert!(matches!(completion, Completion::Cancelled(_)));
    // Three passing polls (0, 1, 2) plus the fourth tripping poll (3).
    assert_eq!(polls.load(Ordering::SeqCst), 4);
}

#[test]
fn cancellation_is_polled_before_a_call() {
    // Entering a call polls the token, so a cancelled token stops at the call
    // boundary with the call expression as the cancellation span. The preceding
    // `def` is not a call and does not trip.
    let source = "\
def f() {
    1
}
f()";
    let (file, completion) = run(source, &CancellationToken::from_fn(|| true));
    match completion {
        Completion::Cancelled(cancellation) => {
            assert_eq!(cancellation.reason(), CancelReason::Requested);
            assert_eq!(file.slice(cancellation.span()).unwrap(), "f()");
        }
        Completion::Value(value) => panic!("expected cancellation, got value: {value:?}"),
    }
}
