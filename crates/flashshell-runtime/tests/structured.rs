#![forbid(unsafe_code)]

//! Acceptance coverage for the closure-free terminal structured commands —
//! `first`, `last`, `collect`, and `length` — over a `ValueStream`. Each drains
//! (or bounded-drains) the stream and reshapes it: `first n` takes the leading
//! items without pulling the source more than `n` times, `last n` keeps the
//! trailing items, `collect` materializes a `List` value, and `length` counts.
//!
//! The layer is host-free and span-independent, matching `stream` and `convert`:
//! no process, terminal, or clock participates. Every terminal state a drain can
//! reach — exhaustion, a materialization bound, a producer failure, or a
//! cancellation — is a first-class `DrainOutcome` arm.

use std::cell::Cell;
use std::rc::Rc;

use flashshell_runtime::eval::{CancelReason, CancellationToken};
use flashshell_runtime::stream::ValueStream;
use flashshell_runtime::structured::{
    DrainOutcome, GetStep, LineStep, SelectStep, SortOutcome, collect, first, get, last, length,
    lines, select, sort,
};
use flashshell_runtime::{Record, Value};

/// A stream of the given integer values.
fn ints(values: &[i64]) -> ValueStream {
    ValueStream::from_values(values.iter().copied().map(Value::Int).collect())
}

#[test]
fn first_takes_the_leading_items_without_overdraining() {
    // The source counts every pull. `first 3` must advance it exactly three times,
    // so an unbounded producer is safe.
    let pulls = Rc::new(Cell::new(0_i64));
    let stream = ValueStream::from_fn({
        let pulls = Rc::clone(&pulls);
        move || {
            let n = pulls.get();
            pulls.set(n + 1);
            Some(Ok(Value::Int(n)))
        }
    });
    match first(stream, 3) {
        DrainOutcome::Done(items) => {
            assert_eq!(items, vec![Value::Int(0), Value::Int(1), Value::Int(2)]);
        }
        other => panic!("expected the leading items, got {other:?}"),
    }
    assert_eq!(pulls.get(), 3, "source advanced exactly `count` times");
}

#[test]
fn first_stops_at_end_when_the_source_is_shorter_than_count() {
    match first(ints(&[1, 2]), 5) {
        DrainOutcome::Done(items) => assert_eq!(items, vec![Value::Int(1), Value::Int(2)]),
        other => panic!("expected the whole short stream, got {other:?}"),
    }
}

#[test]
fn first_propagates_cancellation() {
    let stream = ints(&[1, 2, 3]).with_cancellation(CancellationToken::from_fn(|| true));
    assert!(matches!(
        first(stream, 2),
        DrainOutcome::Cancelled(CancelReason::Requested)
    ));
}

#[test]
fn last_keeps_the_trailing_items() {
    match last(ints(&[1, 2, 3, 4, 5]), 2, 1000) {
        DrainOutcome::Done(items) => assert_eq!(items, vec![Value::Int(4), Value::Int(5)]),
        other => panic!("expected the trailing items, got {other:?}"),
    }
}

#[test]
fn last_keeps_the_whole_stream_when_count_exceeds_length() {
    match last(ints(&[1, 2]), 5, 1000) {
        DrainOutcome::Done(items) => assert_eq!(items, vec![Value::Int(1), Value::Int(2)]),
        other => panic!("expected the whole stream, got {other:?}"),
    }
}

#[test]
fn last_reports_limit_exceeded_on_an_oversized_stream() {
    // `last` must read the whole stream to find its tail, so an oversized stream
    // is refused rather than drained without bound.
    assert!(matches!(
        last(ints(&[1, 2, 3, 4]), 2, 3),
        DrainOutcome::LimitExceeded { limit: 3 }
    ));
}

#[test]
fn collect_materializes_a_list_value_within_the_limit() {
    match collect(ints(&[1, 2, 3]), 1000) {
        DrainOutcome::Done(Value::List(items)) => {
            assert_eq!(&*items, &[Value::Int(1), Value::Int(2), Value::Int(3)]);
        }
        other => panic!("expected a list value, got {other:?}"),
    }
}

#[test]
fn collect_reports_limit_exceeded() {
    assert!(matches!(
        collect(ints(&[1, 2, 3, 4]), 3),
        DrainOutcome::LimitExceeded { limit: 3 }
    ));
}

#[test]
fn collect_within_exactly_the_limit_succeeds() {
    match collect(ints(&[1, 2, 3]), 3) {
        DrainOutcome::Done(Value::List(items)) => assert_eq!(items.len(), 3),
        other => panic!("expected a list value at the bound, got {other:?}"),
    }
}

#[test]
fn length_counts_the_items() {
    match length(ints(&[1, 2, 3, 4]), 1000) {
        DrainOutcome::Done(Value::Int(count)) => assert_eq!(count, 4),
        other => panic!("expected an int count, got {other:?}"),
    }
}

#[test]
fn length_reports_limit_exceeded() {
    assert!(matches!(
        length(ints(&[1, 2, 3, 4]), 3),
        DrainOutcome::LimitExceeded { limit: 3 }
    ));
}

#[test]
fn collect_propagates_a_producer_failure() {
    let mut pulls = 0_i64;
    let stream = ValueStream::from_fn(move || {
        pulls += 1;
        match pulls {
            1 => Some(Ok(Value::Int(1))),
            2 => Some(Err(flashshell_runtime::eval::RuntimeError::new(
                flashshell_runtime::eval::RuntimeErrorKind::ExecutionUnsupported,
                {
                    let file = flashshell_syntax::SourceFile::new(
                        flashshell_syntax::SourceId::new(1),
                        "t",
                        "x",
                    );
                    file.span(0..1).unwrap()
                },
            ))),
            _ => None,
        }
    });
    assert!(matches!(collect(stream, 1000), DrainOutcome::Failed(_)));
}

// --- `lines`: the lazy line splitter over a text value stream -----------------

/// A stream whose items are the given text chunks as `String` values.
fn text(chunks: &[&str]) -> ValueStream {
    ValueStream::from_values(chunks.iter().map(|c| Value::string(*c)).collect())
}

/// Pulls the splitter to exhaustion, asserting no non-`Line`/`End` step, and
/// returns the emitted lines as strings.
fn drain_lines(mut splitter: flashshell_runtime::structured::LineSplitter) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        match splitter.pull() {
            LineStep::Line(Value::String(text)) => out.push(text.to_string()),
            LineStep::Line(other) => panic!("a line must be a string, got {other:?}"),
            LineStep::End => return out,
            other => panic!("expected a line or end, got {other:?}"),
        }
    }
}

#[test]
fn lines_splits_a_chunk_on_newlines() {
    assert_eq!(drain_lines(lines(text(&["a\nb\nc"]))), ["a", "b", "c"]);
}

#[test]
fn lines_does_not_emit_a_trailing_empty_line_for_a_final_newline() {
    // "a\nb\n" is two lines, not three: a trailing terminator ends the last line
    // rather than starting an empty one.
    assert_eq!(drain_lines(lines(text(&["a\nb\n"]))), ["a", "b"]);
}

#[test]
fn lines_flushes_an_unterminated_final_line() {
    assert_eq!(drain_lines(lines(text(&["a\nb"]))), ["a", "b"]);
}

#[test]
fn lines_preserves_interior_blank_lines() {
    assert_eq!(drain_lines(lines(text(&["a\n\nb"]))), ["a", "", "b"]);
}

#[test]
fn lines_strips_a_carriage_return_before_each_newline() {
    assert_eq!(drain_lines(lines(text(&["a\r\nb\r\n"]))), ["a", "b"]);
}

#[test]
fn lines_strips_a_trailing_carriage_return_on_the_flushed_final_line() {
    assert_eq!(drain_lines(lines(text(&["a\r\nb\r"]))), ["a", "b"]);
}

#[test]
fn lines_keeps_a_lone_carriage_return_inside_a_line() {
    // A CR that does not immediately precede an LF is ordinary line content.
    assert_eq!(drain_lines(lines(text(&["a\rb"]))), ["a\rb"]);
}

#[test]
fn lines_joins_a_line_split_across_chunks() {
    assert_eq!(drain_lines(lines(text(&["ab", "cd\nef"]))), ["abcd", "ef"]);
}

#[test]
fn lines_yields_no_lines_for_an_empty_stream() {
    assert_eq!(
        drain_lines(lines(ValueStream::from_values(vec![]))),
        Vec::<String>::new()
    );
}

#[test]
fn lines_is_infinite_safe_when_bounded_downstream() {
    // Each source pull yields one complete line, so pulling the splitter three
    // times advances the unbounded source a bounded number of times.
    let pulls = Rc::new(Cell::new(0_i64));
    let source = ValueStream::from_fn({
        let pulls = Rc::clone(&pulls);
        move || {
            pulls.set(pulls.get() + 1);
            Some(Ok(Value::string("line\n")))
        }
    });
    let mut splitter = lines(source);
    for _ in 0..3 {
        assert!(matches!(splitter.pull(), LineStep::Line(Value::String(t)) if &*t == "line"));
    }
    assert!(
        pulls.get() <= 3,
        "the source advanced a bounded number of times"
    );
}

#[test]
fn lines_reports_a_non_text_item() {
    let mut splitter = lines(ValueStream::from_values(vec![Value::Int(1)]));
    assert!(matches!(
        splitter.pull(),
        LineStep::NotText { actual: "int" }
    ));
    // The step latches: a further pull is `End`, never a repeated report.
    assert!(matches!(splitter.pull(), LineStep::End));
}

#[test]
fn lines_passes_an_upstream_failure_through() {
    let file = flashshell_syntax::SourceFile::new(flashshell_syntax::SourceId::new(1), "t", "x");
    let boom = flashshell_runtime::eval::RuntimeError::new(
        flashshell_runtime::eval::RuntimeErrorKind::ExecutionUnsupported,
        file.span(0..1).unwrap(),
    );
    let expected = boom.clone();
    let mut produced = false;
    let source = ValueStream::from_fn(move || {
        if produced {
            None
        } else {
            produced = true;
            Some(Err(boom.clone()))
        }
    });
    match lines(source).pull() {
        LineStep::Failed(error) => assert_eq!(error, expected),
        other => panic!("expected the upstream failure, got {other:?}"),
    }
}

#[test]
fn lines_passes_an_upstream_cancellation_through() {
    let token = CancellationToken::from_fn(|| true);
    let source = ValueStream::from_values(vec![Value::string("later\n")]).with_cancellation(token);
    assert!(matches!(
        lines(source).pull(),
        LineStep::Cancelled(CancelReason::Requested)
    ));
}

// --- `select` and `get`: read-only record projection --------------------------

/// A record value from the given key/value pairs, in order.
fn record(pairs: &[(&str, Value)]) -> Value {
    let entries = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect();
    Value::Record(Record::new(entries).expect("unique keys"))
}

/// A stream of the given values.
fn values(items: Vec<Value>) -> ValueStream {
    ValueStream::from_values(items)
}

/// Pulls a select transformer to exhaustion, returning the narrowed records.
fn drain_select(mut selector: flashshell_runtime::structured::SelectStream) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        match selector.pull() {
            SelectStep::Record(value) => out.push(value),
            SelectStep::End => return out,
            other => panic!("expected a record or end, got {other:?}"),
        }
    }
}

/// Pulls a get transformer to exhaustion, returning the extracted values.
fn drain_get(mut getter: flashshell_runtime::structured::GetStream) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        match getter.pull() {
            GetStep::Value(value) => out.push(value),
            GetStep::End => return out,
            other => panic!("expected a value or end, got {other:?}"),
        }
    }
}

#[test]
fn select_narrows_each_record_to_the_named_columns_in_order() {
    let stream = values(vec![
        record(&[
            ("name", Value::string("a")),
            ("age", Value::Int(1)),
            ("city", Value::string("x")),
        ]),
        record(&[
            ("name", Value::string("b")),
            ("age", Value::Int(2)),
            ("city", Value::string("y")),
        ]),
    ]);
    let selected = drain_select(select(stream, ["city".to_owned(), "name".to_owned()]));
    assert_eq!(
        selected,
        vec![
            record(&[("city", Value::string("x")), ("name", Value::string("a"))]),
            record(&[("city", Value::string("y")), ("name", Value::string("b"))]),
        ]
    );
}

#[test]
fn select_deduplicates_repeated_requested_columns() {
    let stream = values(vec![record(&[
        ("name", Value::string("a")),
        ("age", Value::Int(1)),
    ])]);
    let selected = drain_select(select(stream, ["name".to_owned(), "name".to_owned()]));
    assert_eq!(selected, vec![record(&[("name", Value::string("a"))])]);
}

#[test]
fn select_reports_a_missing_column() {
    let stream = values(vec![record(&[("name", Value::string("a"))])]);
    let mut selector = select(stream, ["age".to_owned()]);
    assert!(matches!(
        selector.pull(),
        SelectStep::MissingColumn { ref column } if column == "age"
    ));
    // The step latches.
    assert!(matches!(selector.pull(), SelectStep::End));
}

#[test]
fn select_reports_a_non_record_item() {
    let stream = values(vec![Value::Int(5)]);
    let mut selector = select(stream, ["name".to_owned()]);
    assert!(matches!(
        selector.pull(),
        SelectStep::NotRecord { actual: "int" }
    ));
}

#[test]
fn select_passes_upstream_failure_and_cancellation_through() {
    let cancelled = values(vec![record(&[("name", Value::string("a"))])])
        .with_cancellation(CancellationToken::from_fn(|| true));
    assert!(matches!(
        select(cancelled, ["name".to_owned()]).pull(),
        SelectStep::Cancelled(CancelReason::Requested)
    ));
}

#[test]
fn get_extracts_one_field_from_each_record() {
    let stream = values(vec![
        record(&[("name", Value::string("a")), ("age", Value::Int(1))]),
        record(&[("name", Value::string("b")), ("age", Value::Int(2))]),
    ]);
    let got = drain_get(get(stream, "age".to_owned()));
    assert_eq!(got, vec![Value::Int(1), Value::Int(2)]);
}

#[test]
fn get_reports_a_missing_key() {
    let stream = values(vec![record(&[("name", Value::string("a"))])]);
    let mut getter = get(stream, "age".to_owned());
    assert!(matches!(
        getter.pull(),
        GetStep::MissingKey { ref key } if key == "age"
    ));
    assert!(matches!(getter.pull(), GetStep::End));
}

#[test]
fn get_reports_a_non_record_item() {
    let stream = values(vec![Value::string("plain")]);
    let mut getter = get(stream, "name".to_owned());
    assert!(matches!(
        getter.pull(),
        GetStep::NotRecord { actual: "string" }
    ));
}

#[test]
fn get_passes_upstream_cancellation_through() {
    let cancelled = values(vec![record(&[("name", Value::string("a"))])])
        .with_cancellation(CancellationToken::from_fn(|| true));
    assert!(matches!(
        get(cancelled, "name".to_owned()).pull(),
        GetStep::Cancelled(CancelReason::Requested)
    ));
}

// --- `sort`: materializing order over the ordering contract --------------------

fn strings(items: &[&str]) -> ValueStream {
    ValueStream::from_values(items.iter().map(|s| Value::string(*s)).collect())
}

#[test]
fn sort_orders_values_by_natural_order() {
    match sort(ints(&[3, 1, 2]), None, 1000) {
        SortOutcome::Sorted(items) => {
            assert_eq!(items, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        }
        other => panic!("expected a sorted list, got {other:?}"),
    }
}

#[test]
fn sort_orders_strings_lexicographically() {
    match sort(strings(&["c", "a", "b"]), None, 1000) {
        SortOutcome::Sorted(items) => {
            assert_eq!(
                items,
                vec![Value::string("a"), Value::string("b"), Value::string("c")]
            );
        }
        other => panic!("expected a sorted list, got {other:?}"),
    }
}

#[test]
fn sort_by_key_orders_records() {
    let stream = values(vec![
        record(&[("name", Value::string("b")), ("age", Value::Int(2))]),
        record(&[("name", Value::string("a")), ("age", Value::Int(1))]),
    ]);
    match sort(stream, Some("age".to_owned()), 1000) {
        SortOutcome::Sorted(items) => {
            assert_eq!(
                items,
                vec![
                    record(&[("name", Value::string("a")), ("age", Value::Int(1))]),
                    record(&[("name", Value::string("b")), ("age", Value::Int(2))]),
                ]
            );
        }
        other => panic!("expected sorted records, got {other:?}"),
    }
}

#[test]
fn sort_by_key_is_stable_for_equal_keys() {
    // Two records share a key; their input order is preserved.
    let stream = values(vec![
        record(&[("id", Value::Int(1)), ("k", Value::Int(5))]),
        record(&[("id", Value::Int(2)), ("k", Value::Int(5))]),
    ]);
    match sort(stream, Some("k".to_owned()), 1000) {
        SortOutcome::Sorted(items) => {
            assert_eq!(
                items[0],
                record(&[("id", Value::Int(1)), ("k", Value::Int(5))])
            );
            assert_eq!(
                items[1],
                record(&[("id", Value::Int(2)), ("k", Value::Int(5))])
            );
        }
        other => panic!("expected stable sorted records, got {other:?}"),
    }
}

#[test]
fn sort_reports_an_incomparable_pair() {
    let stream = values(vec![Value::Int(1), Value::string("x")]);
    assert!(matches!(
        sort(stream, None, 1000),
        SortOutcome::Incomparable { .. }
    ));
}

#[test]
fn sort_by_key_reports_a_missing_key() {
    let stream = values(vec![record(&[("name", Value::string("a"))])]);
    assert!(matches!(
        sort(stream, Some("age".to_owned()), 1000),
        SortOutcome::MissingKey { ref key } if key == "age"
    ));
}

#[test]
fn sort_by_key_reports_a_non_record_item() {
    let stream = values(vec![Value::Int(5)]);
    assert!(matches!(
        sort(stream, Some("age".to_owned()), 1000),
        SortOutcome::NotRecord { actual: "int" }
    ));
}

#[test]
fn sort_reports_limit_exceeded_on_an_oversized_stream() {
    assert!(matches!(
        sort(ints(&[1, 2, 3, 4]), None, 3),
        SortOutcome::LimitExceeded { limit: 3 }
    ));
}

#[test]
fn sort_of_an_empty_stream_is_empty() {
    match sort(ValueStream::from_values(vec![]), None, 1000) {
        SortOutcome::Sorted(items) => assert!(items.is_empty()),
        other => panic!("expected an empty sorted list, got {other:?}"),
    }
}

#[test]
fn sort_passes_upstream_cancellation_through() {
    let cancelled = ints(&[3, 1, 2]).with_cancellation(CancellationToken::from_fn(|| true));
    assert!(matches!(
        sort(cancelled, None, 1000),
        SortOutcome::Cancelled(CancelReason::Requested)
    ));
}
