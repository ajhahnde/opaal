#![forbid(unsafe_code)]

//! Acceptance coverage for the deterministic fake clock, deadline-driven timeout
//! cancellation, and the evaluation resource budget over the pure evaluator.

use flashshell_runtime::eval::{
    CancelReason, CancellationToken, Clock, Completion, EvalLimits, FakeClock, Instant,
    ResourceBudget, RuntimeErrorKind, evaluate_with_limits,
};
use flashshell_runtime::{ScopeStack, Value};
use flashshell_syntax::{ParseOutcome, SourceFile, SourceId, parse};

fn parse_source(source: &str) -> (SourceFile, flashshell_syntax::Script) {
    let file = SourceFile::new(SourceId::new(1), "test.fsh", source);
    let script = match parse(&file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}\n{source}"),
    };
    (file, script)
}

fn run(
    source: &str,
    limits: &EvalLimits,
) -> Result<Completion, flashshell_runtime::eval::RuntimeError> {
    let (file, script) = parse_source(source);
    let mut scope = ScopeStack::new();
    evaluate_with_limits(&script, &file, &mut scope, limits)
}

#[test]
fn a_fake_clock_advances_deterministically() {
    let clock = FakeClock::new();
    assert_eq!(clock.now(), Instant::from_nanos(0));
    clock.advance(50);
    assert_eq!(clock.now(), Instant::from_nanos(50));

    // A clone shares the same underlying time.
    let mirror = clock.clone();
    clock.advance(25);
    assert_eq!(mirror.now(), Instant::from_nanos(75));
}

#[test]
fn a_deadline_token_trips_when_the_clock_passes_it() {
    let clock = FakeClock::new();
    let token = CancellationToken::deadline(clock.clone(), Instant::from_nanos(100));
    assert!(!token.is_cancelled());
    assert_eq!(token.reason(), CancelReason::Timeout);

    clock.advance(100);
    assert!(token.is_cancelled());
}

#[test]
fn a_passed_deadline_cancels_an_unbounded_loop_with_timeout() {
    // The clock already sits past the deadline, so the loop stops at its first
    // condition boundary with a timeout cancellation rather than running forever.
    let clock = FakeClock::at(1_000);
    let token = CancellationToken::deadline(clock, Instant::from_nanos(100));
    let limits = EvalLimits::new(token, ResourceBudget::unlimited());

    match run("while true {\n}", &limits).unwrap() {
        Completion::Cancelled(cancellation) => {
            assert_eq!(cancellation.reason(), CancelReason::Timeout);
        }
        Completion::Value(value) => panic!("expected timeout cancellation, got {value:?}"),
    }
}

#[test]
fn an_unlimited_budget_runs_to_a_value() {
    let limits = EvalLimits::new(CancellationToken::never(), ResourceBudget::unlimited());
    match run("1 + 2 + 3", &limits).unwrap() {
        Completion::Value(value) => assert_eq!(value, Value::Int(6)),
        Completion::Cancelled(cancellation) => panic!("unexpected cancellation: {cancellation:?}"),
    }
}

#[test]
fn an_exhausted_budget_is_a_runtime_error_not_a_cancellation() {
    // A tight step budget cannot finish this loop; exhaustion is a spanned runtime
    // error, distinct from a cancellation.
    let source = "\
mut total = 0
for n in [1, 2, 3, 4, 5] {
    $total = $total + $n
}
$total";
    let limits = EvalLimits::new(CancellationToken::never(), ResourceBudget::steps(4));
    let error = run(source, &limits).expect_err("a tight budget must fail");
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::ResourceBudgetExceeded
    ));

    // The same script under a generous budget completes deterministically.
    let generous = EvalLimits::new(CancellationToken::never(), ResourceBudget::steps(10_000));
    assert!(matches!(
        run(source, &generous).unwrap(),
        Completion::Value(Value::Int(15))
    ));
}
