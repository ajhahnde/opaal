#![forbid(unsafe_code)]

use flashshell_runtime::ScopeError;
use flashshell_runtime::eval::{ControlKind, RuntimeError, RuntimeErrorKind, evaluate};
use flashshell_runtime::operation::OperationError;
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
fn literals_and_operators_produce_the_last_expression_value() {
    assert_eq!(ok("1 + 2 * 3"), int(7));
    assert_eq!(ok("7 / 2"), int(3)); // floored integer division
    assert_eq!(ok("-5"), int(-5));
    assert_eq!(ok("0x10"), int(16));
    assert_eq!(ok("0o17"), int(15));
    assert_eq!(ok("0b101"), int(5));
    assert_eq!(
        ok("1.0 / 4.0"),
        Value::from(flashshell_runtime::FiniteFloat::new(0.25).unwrap())
    );
    assert_eq!(ok("true"), Value::Bool(true));
    assert_eq!(ok("null"), Value::Null);
    assert_eq!(ok("'exact'"), Value::string("exact"));
    assert_eq!(ok("2 in [1, 2, 3]"), Value::Bool(true));
    assert_eq!(
        ok("[10, 20, 30]"),
        Value::list(vec![int(10), int(20), int(30)])
    );
    // An empty program evaluates to null.
    assert_eq!(ok(""), Value::Null);
}

#[test]
fn bindings_and_assignment_follow_scope_rules() {
    assert_eq!(
        ok("let base = 10\nmut total = $base\n$total = $total + 5\n$total"),
        int(15)
    );

    assert!(matches!(
        err("let x = 1\n$x = 2"),
        RuntimeErrorKind::Scope(ScopeError::ImmutableBinding(_))
    ));
    assert!(matches!(
        err("$missing = 1"),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding(_))
    ));
    assert!(matches!(
        err("$missing"),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding(_))
    ));
}

#[test]
fn if_else_selects_the_matching_block() {
    assert_eq!(
        ok("mut r = 0\nif 1 < 2 { $r = 1 } else { $r = 2 }\n$r"),
        int(1)
    );
    assert_eq!(
        ok("mut r = 0\nif 5 < 2 { $r = 1 } else { $r = 2 }\n$r"),
        int(2)
    );
    assert_eq!(
        ok("mut r = 0\nif false { $r = 1 } else if 3 > 1 { $r = 2 } else { $r = 3 }\n$r"),
        int(2)
    );

    assert!(matches!(
        err("if 3 { }"),
        RuntimeErrorKind::ConditionNotBool { .. }
    ));
}

#[test]
fn while_loops_honor_break_and_continue() {
    let program = "\
mut i = 0
mut sum = 0
while $i < 100 {
    $i = $i + 1
    if $i == 3 { continue }
    if $i == 5 { break }
    $sum = $sum + $i
}
$sum";
    // i = 1,2,(3 skip),4,(5 break) -> 1 + 2 + 4
    assert_eq!(ok(program), int(7));
}

#[test]
fn for_loops_iterate_lists_and_ranges() {
    assert_eq!(
        ok("mut acc = 0\nfor n in [1, 2, 3, 4] {\n    $acc = $acc + $n\n}\n$acc"),
        int(10)
    );
    assert_eq!(
        ok("mut acc = 0\nfor i in 0..4 {\n    $acc = $acc + $i\n}\n$acc"),
        int(6)
    );
    assert_eq!(
        ok("mut acc = 0\nfor i in 0..=4 {\n    $acc = $acc + $i\n}\n$acc"),
        int(10)
    );

    assert!(matches!(
        err("for x in 5 { }"),
        RuntimeErrorKind::NotIterable { .. }
    ));
}

#[test]
fn blocks_open_child_scopes() {
    // Inner shadow does not leak out of the block.
    assert_eq!(ok("let x = 1\nif true { let x = 99 }\n$x"), int(1));
    // Assignment from inside a block reaches the outer mutable binding.
    assert_eq!(ok("mut x = 1\nif true { $x = 42 }\n$x"), int(42));
}

#[test]
fn collections_support_indexing_and_member_access() {
    assert_eq!(ok("let xs = [10, 20, 30]\n$xs[1]"), int(20));
    assert_eq!(ok("let r = {name: 'fsh', count: 2}\n$r.count"), int(2));
    assert_eq!(
        ok("let r = {name: 'fsh', count: 2}\n$r['name']"),
        Value::string("fsh")
    );

    assert!(matches!(
        err("let xs = [10, 20]\n$xs[5]"),
        RuntimeErrorKind::Operation(OperationError::IndexOutOfRange { .. })
    ));
    assert!(matches!(
        err("1 + true"),
        RuntimeErrorKind::Operation(OperationError::UnsupportedOperands { .. })
    ));
}

#[test]
fn deferred_forms_report_precise_errors_with_spans() {
    assert!(matches!(
        err("echo hi"),
        RuntimeErrorKind::ExecutionUnsupported
    ));
    assert!(matches!(
        err("let x = $(echo hi)"),
        RuntimeErrorKind::ExecutionUnsupported
    ));
    assert!(matches!(
        err("let x = \"hi\""),
        RuntimeErrorKind::Unsupported { .. }
    ));
    assert!(matches!(
        err("break"),
        RuntimeErrorKind::ControlOutsideLoop {
            control: ControlKind::Break
        }
    ));
    assert!(matches!(
        err("continue"),
        RuntimeErrorKind::ControlOutsideLoop {
            control: ControlKind::Continue
        }
    ));
    assert!(matches!(
        err("return 1"),
        RuntimeErrorKind::ReturnOutsideFunction
    ));

    // The failing node's span is attached: the non-bool condition `3`.
    let file = SourceFile::new(SourceId::new(7), "span.fsh", "if 3 { }");
    let ParseOutcome::Complete(script) = parse(&file) else {
        panic!("parse");
    };
    let mut scope = ScopeStack::new();
    let error = evaluate(&script, &file, &mut scope).unwrap_err();
    assert_eq!(file.slice(error.span()).unwrap(), "3");
}
