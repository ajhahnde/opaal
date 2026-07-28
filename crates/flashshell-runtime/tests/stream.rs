#![forbid(unsafe_code)]

//! Acceptance coverage for the lazy `ValueStream` payload behind the
//! `Carrier::ValueStream` planning tag. The stream computes nothing until pulled,
//! yields per-item `Result<Value, RuntimeError>`, bounds staged memory through a
//! capacity-capped `BoundedQueue`, bounds materialization through
//! `collect_bounded`, and stops at its next pull boundary when a cooperative
//! cancellation token trips. The whole layer is span-independent and host-free:
//! no process, terminal, or clock participates.

use std::cell::Cell;
use std::rc::Rc;

use flashshell_runtime::Value;
use flashshell_runtime::eval::{CancelReason, CancellationToken};
use flashshell_runtime::stream::{
    BoundedQueue, BytePull, ByteStream, CollectOutcome, QueueFull, StreamPull, ValueStream,
};
use flashshell_syntax::{SourceFile, SourceId};

/// Pulls one item, panicking on any non-`Item` outcome.
fn expect_item(stream: &mut ValueStream) -> Value {
    match stream.pull() {
        StreamPull::Item(value) => value,
        other => panic!("expected an item, got {other:?}"),
    }
}

#[test]
fn once_yields_one_item_then_end() {
    let mut stream = ValueStream::once(Value::Int(7));
    assert_eq!(expect_item(&mut stream), Value::Int(7));
    assert!(matches!(stream.pull(), StreamPull::End));
    // Exhaustion is stable: a pull past the end stays `End`.
    assert!(matches!(stream.pull(), StreamPull::End));
}

#[test]
fn from_values_drains_in_order_then_end() {
    let mut stream = ValueStream::from_values(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    assert_eq!(expect_item(&mut stream), Value::Int(1));
    assert_eq!(expect_item(&mut stream), Value::Int(2));
    assert_eq!(expect_item(&mut stream), Value::Int(3));
    assert!(matches!(stream.pull(), StreamPull::End));
}

#[test]
fn from_fn_is_advanced_only_as_pulled() {
    // The producer is invoked exactly once per pull, so a lazy source is never
    // driven ahead of the consumer. An unbounded producer is fine because pull
    // sets the pace.
    let calls = Rc::new(Cell::new(0_i64));
    let mut stream = ValueStream::from_fn({
        let calls = Rc::clone(&calls);
        move || {
            let n = calls.get();
            calls.set(n + 1);
            Some(Ok(Value::Int(n)))
        }
    });

    assert_eq!(expect_item(&mut stream), Value::Int(0));
    assert_eq!(expect_item(&mut stream), Value::Int(1));
    assert_eq!(expect_item(&mut stream), Value::Int(2));
    assert_eq!(calls.get(), 3, "producer advanced exactly once per pull");
}

#[test]
fn from_fn_can_fail_mid_stream() {
    // A producer failure surfaces as `Failed` at the offending item, carrying the
    // runtime error the producer owns; `End` stays distinct from failure.
    let file = SourceFile::new(SourceId::new(1), "test.fsh", "x");
    let span = file.span(0..1).unwrap();
    let calls = Rc::new(Cell::new(0_i64));
    let mut stream = ValueStream::from_fn({
        let calls = Rc::clone(&calls);
        move || {
            let n = calls.get();
            calls.set(n + 1);
            if n == 0 {
                Some(Ok(Value::Int(0)))
            } else {
                Some(Err(flashshell_runtime::eval::RuntimeError::new(
                    flashshell_runtime::eval::RuntimeErrorKind::Unsupported {
                        feature: "producer",
                    },
                    span,
                )))
            }
        }
    });

    assert_eq!(expect_item(&mut stream), Value::Int(0));
    match stream.pull() {
        StreamPull::Failed(error) => {
            assert!(matches!(
                error.kind(),
                flashshell_runtime::eval::RuntimeErrorKind::Unsupported {
                    feature: "producer"
                }
            ));
            assert_eq!(error.span(), span);
        }
        other => panic!("expected a producer failure, got {other:?}"),
    }
}

#[test]
fn from_pull_fn_preserves_a_first_class_cancellation_state() {
    let mut step = 0;
    let mut stream = ValueStream::from_pull_fn(move || {
        step += 1;
        match step {
            1 => StreamPull::Item(Value::Int(7)),
            2 => StreamPull::Cancelled(CancelReason::Timeout),
            _ => StreamPull::End,
        }
    });

    assert!(matches!(stream.pull(), StreamPull::Item(Value::Int(7))));
    assert!(matches!(
        stream.pull(),
        StreamPull::Cancelled(CancelReason::Timeout)
    ));
}

#[test]
fn byte_stream_preserves_chunk_boundaries_and_order() {
    let mut stream =
        ByteStream::from_chunks(vec![b"one".to_vec(), vec![0, 0xff], b"three".to_vec()]);

    assert!(matches!(stream.pull(), BytePull::Chunk(chunk) if chunk == b"one"));
    assert!(matches!(stream.pull(), BytePull::Chunk(chunk) if chunk == [0, 0xff]));
    assert!(matches!(stream.pull(), BytePull::Chunk(chunk) if chunk == b"three"));
    assert!(matches!(stream.pull(), BytePull::End));
}

#[test]
fn byte_stream_puller_preserves_cancellation() {
    let mut step = 0;
    let mut stream = ByteStream::from_pull_fn(move || {
        step += 1;
        match step {
            1 => BytePull::Chunk(vec![7]),
            2 => BytePull::Cancelled(CancelReason::Timeout),
            _ => BytePull::End,
        }
    });

    assert!(matches!(stream.pull(), BytePull::Chunk(chunk) if chunk == [7]));
    assert!(matches!(
        stream.pull(),
        BytePull::Cancelled(CancelReason::Timeout)
    ));
}

#[test]
fn cancellation_stops_the_stream_without_advancing_the_source() {
    // An already-cancelled token trips before the source is advanced, so the
    // producer is never invoked and the pull reports the token's reason.
    let calls = Rc::new(Cell::new(0_i64));
    let mut stream = ValueStream::from_fn({
        let calls = Rc::clone(&calls);
        move || {
            calls.set(calls.get() + 1);
            Some(Ok(Value::Int(9)))
        }
    })
    .with_cancellation(CancellationToken::from_fn(|| true));

    match stream.pull() {
        StreamPull::Cancelled(reason) => assert_eq!(reason, CancelReason::Requested),
        other => panic!("expected cancellation, got {other:?}"),
    }
    assert_eq!(
        calls.get(),
        0,
        "a cancelled pull must not advance the source"
    );
}

#[test]
fn collect_bounded_collects_within_limit() {
    let mut stream = ValueStream::from_values(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    match stream.collect_bounded(10) {
        CollectOutcome::Collected(values) => {
            assert_eq!(values, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        }
        other => panic!("expected a full collection, got {other:?}"),
    }
}

#[test]
fn collect_bounded_caps_an_unbounded_producer() {
    // An infinite producer with a bounded collect never materializes fully; it
    // reports the limit it would have exceeded rather than looping forever.
    let counter = Rc::new(Cell::new(0_i64));
    let mut stream = ValueStream::from_fn({
        let counter = Rc::clone(&counter);
        move || {
            let n = counter.get();
            counter.set(n + 1);
            Some(Ok(Value::Int(n)))
        }
    });
    match stream.collect_bounded(5) {
        CollectOutcome::LimitExceeded { limit } => assert_eq!(limit, 5),
        other => panic!("expected the limit to be exceeded, got {other:?}"),
    }
}

#[test]
fn bounded_queue_refuses_push_when_full_and_frees_a_slot_on_pop() {
    // The bounded queue is the backpressure primitive: a producer that fills it is
    // refused further pushes, so staged memory never exceeds the capacity, and a
    // consumer `pop` frees exactly one slot.
    let mut queue = BoundedQueue::with_capacity(2);
    assert_eq!(queue.capacity(), 2);
    assert!(queue.try_push(Value::Int(1)).is_ok());
    assert!(queue.try_push(Value::Int(2)).is_ok());
    match queue.try_push(Value::Int(3)) {
        Err(QueueFull(value)) => assert_eq!(value, Value::Int(3)),
        Ok(()) => panic!("a full queue must refuse the push"),
    }
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.pop(), Some(Value::Int(1)));
    assert!(
        queue.try_push(Value::Int(3)).is_ok(),
        "a pop frees a slot for one more push"
    );
    assert_eq!(queue.len(), 2);
}

#[test]
fn a_backpressured_producer_retries_without_reordering_or_exceeding_capacity() {
    let mut queue = BoundedQueue::with_capacity(2);
    let mut next = 0_i64;
    let mut consumed = Vec::new();
    let mut refusals = 0;
    let mut maximum_staged = 0;

    while next < 6 {
        match queue.try_push(Value::Int(next)) {
            Ok(()) => next += 1,
            Err(QueueFull(value)) => {
                assert_eq!(
                    value,
                    Value::Int(next),
                    "the refused value is returned for an exact retry"
                );
                refusals += 1;
                consumed.push(queue.pop().expect("a full queue has a front item"));
            }
        }
        maximum_staged = maximum_staged.max(queue.len());
        assert!(queue.len() <= queue.capacity());
    }
    while let Some(value) = queue.pop() {
        consumed.push(value);
    }

    assert!(refusals > 0, "the interleaving must exercise backpressure");
    assert_eq!(maximum_staged, queue.capacity());
    assert_eq!(
        consumed,
        (0..6).map(Value::Int).collect::<Vec<_>>(),
        "retry and consumption preserve producer order"
    );
}

#[test]
fn from_queue_drains_the_queue_in_fifo_order() {
    let mut queue = BoundedQueue::with_capacity(4);
    queue.try_push(Value::Int(10)).unwrap();
    queue.try_push(Value::Int(20)).unwrap();
    let mut stream = ValueStream::from_queue(queue);
    assert_eq!(expect_item(&mut stream), Value::Int(10));
    assert_eq!(expect_item(&mut stream), Value::Int(20));
    assert!(matches!(stream.pull(), StreamPull::End));
}
