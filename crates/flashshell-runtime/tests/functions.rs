#![forbid(unsafe_code)]

//! Acceptance coverage for functions, closures, calls, and `return` over the
//! pure evaluator.

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

fn int(value: i64) -> Value {
    Value::Int(value)
}

#[test]
fn named_functions_call_and_recurse() {
    // Explicit `return` in a branch plus an implicit final-expression result.
    let factorial = "\
def fac(n) {
    if $n <= 1 {
        return 1
    }
    $n * fac($n - 1)
}
fac(5)";
    assert_eq!(ok(factorial), int(120));

    // A function with no explicit return yields its last expression value.
    assert_eq!(ok("def double(x) {\n    $x + $x\n}\ndouble(21)"), int(42));

    // Multiple parameters bind in order.
    assert_eq!(ok("def add(a, b) {\n    $a + $b\n}\nadd(3, 4)"), int(7));

    // A function ending in a control statement yields null.
    assert_eq!(
        ok("def noop(x) {\n    if $x > 0 {\n        return 1\n    }\n}\nnoop(0)"),
        Value::Null
    );
}

#[test]
fn closures_are_values_that_capture_and_call() {
    // A closure stored in a binding is called through a `$` variable callee.
    assert_eq!(ok("let add = {|x| $x + 1}\n$add(4)"), int(5));

    // A closure captures a by-value snapshot; a later reassignment is invisible.
    let snapshot = "\
mut base = 10
let plus = {|x| $x + $base}
$base = 100
$plus(5)";
    assert_eq!(ok(snapshot), int(15));
}

#[test]
fn captured_bindings_are_immutable_inside_a_function() {
    let assign_capture = "\
mut base = 10
def bump() {
    $base = 20
}
bump()";
    assert!(matches!(
        err(assign_capture),
        RuntimeErrorKind::Scope(ScopeError::ImmutableBinding(_))
    ));
}

#[test]
fn callables_use_runtime_identity() {
    // The same captured value compares equal to itself.
    assert_eq!(
        ok("let f = {|| 1}\nlet g = $f\n$f == $g"),
        Value::Bool(true)
    );
    // Two separately created closures are distinct even with identical bodies.
    assert_eq!(
        ok("let a = {|| 1}\nlet b = {|| 1}\n$a == $b"),
        Value::Bool(false)
    );
}

#[test]
fn call_and_return_errors_are_precise() {
    // Wrong argument count.
    assert!(matches!(
        err("def f(a, b) {\n    $a + $b\n}\nf(1)"),
        RuntimeErrorKind::ArityMismatch {
            expected: 2,
            actual: 1
        }
    ));

    // A non-callable callee.
    assert!(matches!(
        err("let x = 5\n$x(1)"),
        RuntimeErrorKind::NotCallable { actual: "int" }
    ));

    // An unknown function name.
    assert!(matches!(
        err("missing(1)"),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding(_))
    ));

    // Duplicate parameter names are rejected when the callable is created.
    assert!(matches!(
        err("def f(a, a) {\n    $a\n}\nf(1, 2)"),
        RuntimeErrorKind::DuplicateParameter { .. }
    ));

    // `return` at script top level is an error.
    assert!(matches!(
        err("return 1"),
        RuntimeErrorKind::ReturnOutsideFunction
    ));
}

#[test]
fn callable_display_reveals_kind_and_origin() {
    // A closure produces a callable value directly, exercising the display form.
    assert_eq!(format!("{}", ok("{|x| $x}")), "<closure at test.fsh:1:1>");
    assert_eq!(format!("{:?}", ok("{|x| $x}")), "<closure at test.fsh:1:1>");
}
