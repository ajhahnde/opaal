#![forbid(unsafe_code)]

//! Acceptance coverage for the closure-driven structured commands — `each` (map)
//! and `where` (filter) — over a `ValueStream`.
//!
//! Unlike the span-independent `structured` and `convert` layers, these commands
//! thread a runtime closure and the pure evaluator through a lazy stream, so they
//! carry the closure's source and a command span. Each is lazy: it pulls the
//! source and applies the closure one item at a time, so `each`/`where` over an
//! unbounded producer stay bounded behind a bounded downstream. Every terminal
//! state — exhaustion, a producer failure, a cancellation (upstream or from the
//! closure), a closure runtime error, and (for `where`) a non-boolean predicate —
//! is a first-class step.

use std::cell::Cell;
use std::rc::Rc;

use flash_runtime::closure::{EachStep, UpdateStep, WhereStep, each, update, r#where};
use flash_runtime::eval::{
    CancelReason, CancellationToken, EvalLimits, ResourceBudget, RuntimeErrorKind, evaluate,
};
use flash_runtime::stream::ValueStream;
use flash_runtime::{Environment, Record, ScopeStack, Value};
use flash_syntax::{ParseOutcome, SourceFile, SourceId, Span, parse};

/// Evaluates `source` as a closure expression, returning the owned source file
/// (whose spans the closure body addresses) and the resulting callable value.
fn built(source: &str) -> (SourceFile, Value) {
    let file = SourceFile::new(SourceId::new(1), "closure.fsh", source);
    let script = match parse(&file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}\n{source}"),
    };
    let mut scope = ScopeStack::new();
    let value = evaluate(&script, &file, &mut scope).expect("closure evaluates");
    (file, value)
}

fn command_span(file: &SourceFile) -> Span {
    file.span(0..1).expect("a one-byte span exists")
}

fn ints(values: &[i64]) -> ValueStream {
    ValueStream::from_values(values.iter().copied().map(Value::Int).collect())
}

// --- `each` -------------------------------------------------------------------

#[test]
fn each_maps_every_item() {
    let (file, closure) = built("{|x| $x * 2}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let mut mapper = each(
        ints(&[1, 2, 3]),
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    assert!(matches!(mapper.pull(), EachStep::Item(Value::Int(2))));
    assert!(matches!(mapper.pull(), EachStep::Item(Value::Int(4))));
    assert!(matches!(mapper.pull(), EachStep::Item(Value::Int(6))));
    assert!(matches!(mapper.pull(), EachStep::End));
}

#[test]
fn each_is_lazy_over_an_unbounded_source() {
    let (file, closure) = built("{|x| $x + 1}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let pulls = Rc::new(Cell::new(0_i64));
    let source = ValueStream::from_fn({
        let pulls = Rc::clone(&pulls);
        move || {
            let n = pulls.get();
            pulls.set(n + 1);
            Some(Ok(Value::Int(n)))
        }
    });
    let mut mapper = each(
        source,
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    assert!(matches!(mapper.pull(), EachStep::Item(Value::Int(1))));
    assert!(matches!(mapper.pull(), EachStep::Item(Value::Int(2))));
    assert!(matches!(mapper.pull(), EachStep::Item(Value::Int(3))));
    assert_eq!(pulls.get(), 3, "the source advanced once per emitted item");
}

#[test]
fn each_surfaces_a_closure_runtime_error() {
    let (file, closure) = built("{|x| $missing}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let mut mapper = each(
        ints(&[1]),
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    match mapper.pull() {
        EachStep::Failed(error) => {
            assert!(matches!(error.kind(), RuntimeErrorKind::Scope(_)));
        }
        other => panic!("expected the closure error, got {other:?}"),
    }
    // The step latches.
    assert!(matches!(mapper.pull(), EachStep::End));
}

#[test]
fn each_reports_a_non_callable_closure() {
    let file = SourceFile::new(SourceId::new(2), "c.fsh", "1");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let mut mapper = each(
        ints(&[1]),
        Value::Int(7),
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    match mapper.pull() {
        EachStep::Failed(error) => {
            assert!(matches!(
                error.kind(),
                RuntimeErrorKind::NotCallable { actual: "int" }
            ));
        }
        other => panic!("expected a not-callable error, got {other:?}"),
    }
}

#[test]
fn each_respects_the_resource_budget() {
    // A tiny budget is exhausted inside the closure body, so applying it fails.
    let (file, closure) = built("{|x| $x + $x + $x}");
    let limits = EvalLimits::new(CancellationToken::never(), ResourceBudget::steps(1));
    let mut env = Environment::new();
    let mut mapper = each(
        ints(&[1]),
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    match mapper.pull() {
        EachStep::Failed(error) => {
            assert!(matches!(
                error.kind(),
                RuntimeErrorKind::ResourceBudgetExceeded
            ));
        }
        other => panic!("expected a budget error, got {other:?}"),
    }
}

#[test]
fn each_passes_an_upstream_failure_through() {
    let (file, closure) = built("{|x| $x}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let mut produced = false;
    let boom_file = SourceFile::new(SourceId::new(3), "b.fsh", "x");
    let boom = flash_runtime::eval::RuntimeError::new(
        RuntimeErrorKind::ExecutionUnsupported,
        boom_file.span(0..1).unwrap(),
    );
    let expected = boom.clone();
    let source = ValueStream::from_fn(move || {
        if produced {
            None
        } else {
            produced = true;
            Some(Err(boom.clone()))
        }
    });
    match each(
        source,
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    )
    .pull()
    {
        EachStep::Failed(error) => assert_eq!(error, expected),
        other => panic!("expected the upstream failure, got {other:?}"),
    }
}

#[test]
fn each_passes_an_upstream_cancellation_through() {
    let (file, closure) = built("{|x| $x}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let source = ints(&[1, 2]).with_cancellation(CancellationToken::from_fn(|| true));
    assert!(matches!(
        each(
            source,
            closure,
            &file,
            command_span(&file),
            &mut env,
            &limits
        )
        .pull(),
        EachStep::Cancelled(CancelReason::Requested)
    ));
}

#[test]
fn each_reports_a_cancelled_closure() {
    // A tripped token in the limits cancels the closure application itself.
    let (file, closure) = built("{|x| $x}");
    let limits = EvalLimits::new(
        CancellationToken::from_fn(|| true),
        ResourceBudget::unlimited(),
    );
    let mut env = Environment::new();
    assert!(matches!(
        each(
            ints(&[1]),
            closure,
            &file,
            command_span(&file),
            &mut env,
            &limits
        )
        .pull(),
        EachStep::Cancelled(CancelReason::Requested)
    ));
}

// --- `where` ------------------------------------------------------------------

#[test]
fn where_keeps_only_matching_items() {
    let (file, closure) = built("{|x| $x > 2}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let mut filter = r#where(
        ints(&[1, 2, 3, 4]),
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    assert!(matches!(filter.pull(), WhereStep::Item(Value::Int(3))));
    assert!(matches!(filter.pull(), WhereStep::Item(Value::Int(4))));
    assert!(matches!(filter.pull(), WhereStep::End));
}

#[test]
fn where_can_filter_everything_out() {
    let (file, closure) = built("{|x| false}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let mut filter = r#where(
        ints(&[1, 2, 3]),
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    assert!(matches!(filter.pull(), WhereStep::End));
}

#[test]
fn where_reports_a_non_boolean_predicate() {
    let (file, closure) = built("{|x| $x + 1}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let mut filter = r#where(
        ints(&[1]),
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    assert!(matches!(
        filter.pull(),
        WhereStep::PredicateNotBool { actual: "int" }
    ));
    // The step latches.
    assert!(matches!(filter.pull(), WhereStep::End));
}

#[test]
fn where_surfaces_a_closure_runtime_error() {
    let (file, closure) = built("{|x| $missing}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    match r#where(
        ints(&[1]),
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    )
    .pull()
    {
        WhereStep::Failed(error) => assert!(matches!(error.kind(), RuntimeErrorKind::Scope(_))),
        other => panic!("expected the closure error, got {other:?}"),
    }
}

#[test]
fn where_passes_an_upstream_cancellation_through() {
    let (file, closure) = built("{|x| true}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let source = ints(&[1, 2]).with_cancellation(CancellationToken::from_fn(|| true));
    assert!(matches!(
        r#where(
            source,
            closure,
            &file,
            command_span(&file),
            &mut env,
            &limits
        )
        .pull(),
        WhereStep::Cancelled(CancelReason::Requested)
    ));
}

// --- `update` -----------------------------------------------------------------

/// A record value from the given key/value pairs, in order.
fn record(pairs: &[(&str, Value)]) -> Value {
    let entries = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect();
    Value::Record(Record::new(entries).expect("unique keys"))
}

/// A plain source file for the static-replacement path (no closure is applied).
fn plain_file() -> SourceFile {
    SourceFile::new(SourceId::new(9), "u.fsh", "0")
}

#[test]
fn update_replaces_a_field_with_a_static_value_preserving_order() {
    let file = plain_file();
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let stream = ValueStream::from_values(vec![record(&[
        ("name", Value::string("a")),
        ("size", Value::Int(1)),
    ])]);
    let mut updater = update(
        stream,
        "size".to_owned(),
        Value::Int(99),
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    assert_eq!(
        match updater.pull() {
            UpdateStep::Record(value) => value,
            other => panic!("expected a record, got {other:?}"),
        },
        record(&[("name", Value::string("a")), ("size", Value::Int(99))])
    );
    assert!(matches!(updater.pull(), UpdateStep::End));
}

#[test]
fn update_applies_a_closure_to_the_current_value() {
    let (file, closure) = built("{|x| $x * 10}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let stream = ValueStream::from_values(vec![record(&[("size", Value::Int(2))])]);
    let mut updater = update(
        stream,
        "size".to_owned(),
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    assert_eq!(
        match updater.pull() {
            UpdateStep::Record(value) => value,
            other => panic!("expected a record, got {other:?}"),
        },
        record(&[("size", Value::Int(20))])
    );
}

#[test]
fn update_reports_a_missing_key() {
    let file = plain_file();
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let stream = ValueStream::from_values(vec![record(&[("name", Value::string("a"))])]);
    let mut updater = update(
        stream,
        "size".to_owned(),
        Value::Int(0),
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    assert!(matches!(
        updater.pull(),
        UpdateStep::MissingKey { ref key } if key == "size"
    ));
    assert!(matches!(updater.pull(), UpdateStep::End));
}

#[test]
fn update_reports_a_non_record_item() {
    let file = plain_file();
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let stream = ValueStream::from_values(vec![Value::Int(5)]);
    let mut updater = update(
        stream,
        "size".to_owned(),
        Value::Int(0),
        &file,
        command_span(&file),
        &mut env,
        &limits,
    );
    assert!(matches!(
        updater.pull(),
        UpdateStep::NotRecord { actual: "int" }
    ));
}

#[test]
fn update_surfaces_a_closure_runtime_error() {
    let (file, closure) = built("{|x| $missing}");
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let stream = ValueStream::from_values(vec![record(&[("size", Value::Int(2))])]);
    match update(
        stream,
        "size".to_owned(),
        closure,
        &file,
        command_span(&file),
        &mut env,
        &limits,
    )
    .pull()
    {
        UpdateStep::Failed(error) => assert!(matches!(error.kind(), RuntimeErrorKind::Scope(_))),
        other => panic!("expected the closure error, got {other:?}"),
    }
}

#[test]
fn update_passes_an_upstream_cancellation_through() {
    let file = plain_file();
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let stream = ValueStream::from_values(vec![record(&[("size", Value::Int(1))])])
        .with_cancellation(CancellationToken::from_fn(|| true));
    assert!(matches!(
        update(
            stream,
            "size".to_owned(),
            Value::Int(0),
            &file,
            command_span(&file),
            &mut env,
            &limits
        )
        .pull(),
        UpdateStep::Cancelled(CancelReason::Requested)
    ));
}
