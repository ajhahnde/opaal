#![forbid(unsafe_code)]

use flashshell_runtime::ScopeError;
use flashshell_runtime::eval::{RuntimeError, RuntimeErrorKind, evaluate};
use flashshell_runtime::{ScopeStack, Value};
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

fn ok(source: &str) -> Value {
    run(source).unwrap_or_else(|error| panic!("evaluation failed: {error:?}\n{source}"))
}

fn err(source: &str) -> RuntimeErrorKind {
    run(source).expect_err(source).kind().clone()
}

#[test]
fn arms_are_tried_in_source_order_with_literal_and_wildcard() {
    // The first literal arm equal to the scrutinee wins; later arms are skipped.
    let source = "\
mut r = 'none'
match 2 {
    1 => { $r = 'one' }
    2 => { $r = 'two' }
    2 => { $r = 'unreached' }
    _ => { $r = 'other' }
}
$r";
    assert_eq!(ok(source), Value::string("two"));

    // No literal arm matches, so the wildcard runs.
    let fallthrough = "\
mut r = 'none'
match 9 {
    1 => { $r = 'one' }
    _ => { $r = 'other' }
}
$r";
    assert_eq!(ok(fallthrough), Value::string("other"));
}

#[test]
fn an_identifier_pattern_binds_the_scrutinee_for_guard_and_body() {
    // The binding is visible to both the guard and the arm body.
    let source = "\
mut r = 0
match 5 {
    n if $n > 10 => { $r = 100 }
    n => { $r = $n + 1 }
}
$r";
    assert_eq!(ok(source), Value::Int(6));
}

#[test]
fn a_binding_does_not_escape_the_arm_frame() {
    // Each arm opens a fresh frame, so the bound name is gone after the match.
    let source = "\
match 3 {
    n => { $n }
}
$n";
    assert_eq!(
        err(source),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding("n".to_owned()))
    );
}

#[test]
fn no_matching_arm_is_a_runtime_error() {
    let source = "\
match 9 {
    1 => { }
    2 => { }
}";
    assert_eq!(err(source), RuntimeErrorKind::NoMatchingArm);
}

#[test]
fn a_non_bool_guard_is_a_condition_type_error() {
    let source = "\
match 1 {
    n if $n => { }
    _ => { }
}";
    assert_eq!(
        err(source),
        RuntimeErrorKind::ConditionNotBool { actual: "int" }
    );
}

#[test]
fn a_loop_transfer_inside_an_arm_propagates_to_the_enclosing_loop() {
    let source = "\
mut r = 0
for i in [1, 2, 3] {
    match $i {
        2 => { break }
        _ => { $r = $r + 1 }
    }
}
$r";
    assert_eq!(ok(source), Value::Int(1));
}

#[test]
fn return_inside_an_arm_leaves_the_function() {
    let source = "\
def classify(x) {
    match $x {
        0 => { return 'zero' }
        _ => { return 'nonzero' }
    }
}
classify(0)";
    assert_eq!(ok(source), Value::string("zero"));
}
