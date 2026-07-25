#![forbid(unsafe_code)]

//! Acceptance coverage for `!`, `&&`, and `||` with short-circuit semantics
//! over the pure evaluator.

use flashshell_runtime::eval::{RuntimeError, RuntimeErrorKind, evaluate};
use flashshell_runtime::{BindingMutability, Duration, ScopeStack, Status, Value};
use flashshell_syntax::{ParseOutcome, SourceFile, SourceId, parse};

fn run(source: &str) -> Result<Value, RuntimeError> {
    let file = SourceFile::new(SourceId::new(1), "test.fsh", source);
    let script = match parse(&file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}\n{source}"),
    };
    let mut scope = ScopeStack::new();
    evaluate(&script, &file, &mut scope)
}

fn run_with_scope(source: &str, scope: &mut ScopeStack) -> Result<Value, RuntimeError> {
    let file = SourceFile::new(SourceId::new(1), "test.fsh", source);
    let script = match parse(&file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}\n{source}"),
    };
    evaluate(&script, &file, scope)
}

fn ok(source: &str) -> Value {
    run(source).unwrap_or_else(|error| panic!("evaluation failed: {error:?}\n{source}"))
}

fn err(source: &str) -> RuntimeErrorKind {
    run(source).expect_err(source).kind().clone()
}

fn boolean(value: bool) -> Value {
    Value::Bool(value)
}

#[test]
fn logical_not_negates_a_bool() {
    assert_eq!(ok("!true"), boolean(false));
    assert_eq!(ok("!false"), boolean(true));
    assert_eq!(ok("!!true"), boolean(true));
    // As an ordinary expression it can bind.
    assert_eq!(ok("let flip = !true\n$flip"), boolean(false));
}

#[test]
fn not_on_a_non_bool_is_a_logic_operand_error() {
    assert!(matches!(
        err("!5"),
        RuntimeErrorKind::LogicOperandNotBool { .. }
    ));
}

#[test]
fn and_returns_a_bool_and_short_circuits() {
    assert_eq!(ok("true && true"), boolean(true));
    assert_eq!(ok("true && false"), boolean(false));
    assert_eq!(ok("false && true"), boolean(false));

    // A `false` left operand skips the right entirely, so an unbound name there
    // never raises an unknown-binding error.
    assert_eq!(ok("false && $never"), boolean(false));
}

#[test]
fn or_returns_a_bool_and_short_circuits() {
    assert_eq!(ok("false || false"), boolean(false));
    assert_eq!(ok("false || true"), boolean(true));
    assert_eq!(ok("true || false"), boolean(true));

    // A `true` left operand skips the right entirely.
    assert_eq!(ok("true || $never"), boolean(true));
}

#[test]
fn a_non_bool_operand_that_is_evaluated_is_an_error() {
    // The left operand is always evaluated, so a non-bool left is an error.
    assert!(matches!(
        err("1 || true"),
        RuntimeErrorKind::ConditionalOperandNotBoolOrStatus { .. }
    ));
    // A reached right operand is likewise typed.
    assert!(matches!(
        err("true && 1"),
        RuntimeErrorKind::ConditionalOperandNotBoolOrStatus { .. }
    ));
}

#[test]
fn and_binds_tighter_than_or() {
    // `false || true && false` parses as `false || (true && false)` => false.
    assert_eq!(ok("false || true && false"), boolean(false));
    // `true || false && false` parses as `true || (false && false)` => true, and
    // the `&&` term is short-circuited away by the `true` on the left.
    assert_eq!(ok("true || false && false"), boolean(true));
}

#[test]
fn boolean_logic_drives_conditions_and_grouped_values() {
    // A full chain as an `if` condition.
    let source = "\
mut result = 'no'
if true && (false || true) {
    $result = 'yes'
}
$result";
    assert_eq!(ok(source), Value::string("yes"));

    // A grouped chain used as an expression value.
    assert_eq!(ok("let both = (true && true)\n$both"), boolean(true));
}

#[test]
fn status_values_drive_chains_and_conditions_without_losing_identity() {
    let failed = Status::exit(7, Duration::from_nanos(4)).unwrap();
    let succeeded = Status::exit(0, Duration::from_nanos(8)).unwrap();
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "failed",
            BindingMutability::Immutable,
            Value::from(failed.clone()),
        )
        .unwrap();
    scope
        .declare(
            "succeeded",
            BindingMutability::Immutable,
            Value::from(succeeded.clone()),
        )
        .unwrap();

    assert_eq!(
        run_with_scope("$failed || $succeeded", &mut scope).unwrap(),
        Value::from(succeeded)
    );
    assert_eq!(
        run_with_scope("$failed && $never", &mut scope).unwrap(),
        Value::from(failed)
    );
    assert_eq!(
        run_with_scope(
            "mut result = 'no'\nif $succeeded { $result = 'yes' }\n$result",
            &mut scope,
        )
        .unwrap(),
        Value::string("yes")
    );
}
