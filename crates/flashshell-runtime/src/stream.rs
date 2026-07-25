//! The lazy value stream carried by a structured pipeline edge.
//!
//! [`ValueStream`] is the concrete payload behind the `Carrier::ValueStream`
//! planning tag: a single-threaded, pull-driven sequence that computes nothing
//! until a consumer pulls it, so an unbounded producer imposes no cost ahead of
//! demand and backpressure is implicit. A pull returns a [`StreamPull`] whose
//! four arms mirror [`crate::eval::Completion`]: an item, exhaustion, a producer
//! error, or a cancellation. Per-item production is `Result<Value, RuntimeError>`
//! so a lazy producer fails at the offending item with the span it owns.
//!
//! [`BoundedQueue`] is the capacity-capped staging primitive a later
//! producer/consumer bridge pushes into: a full queue refuses further pushes, so
//! staged memory never exceeds the capacity. [`ValueStream::collect_bounded`]
//! bounds terminal materialization, so an infinite producer paired with a
//! bounded consumer never materializes fully.
//!
//! The whole layer is span-independent, matching [`crate::resolve`] and
//! [`crate::operation`]: a cancellation reports only its [`CancelReason`], and
//! the executor that later drives a stream inside a pipeline attaches source
//! spans at the pipeline boundary. Nothing here touches a process, terminal, or
//! clock.

use std::collections::VecDeque;

use crate::Value;
use crate::eval::{CancelReason, CancellationToken, RuntimeError};

/// The default staging capacity of a [`BoundedQueue`] built with [`BoundedQueue::new`].
pub const DEFAULT_CAPACITY: usize = 1024;

/// The outcome of pulling one item from a [`ValueStream`].
///
/// The arms are mutually exclusive terminal or intermediate states; `End` and
/// `Failed` are distinct so exhaustion is never confused with a producer error.
#[derive(Debug)]
pub enum StreamPull {
    /// The next value in the stream.
    Item(Value),
    /// The source is exhausted; further pulls stay `End`.
    End,
    /// The producer raised a runtime error carrying its own span.
    Failed(RuntimeError),
    /// The carried cancellation token tripped before the source was advanced.
    Cancelled(CancelReason),
}

/// The outcome of draining a [`ValueStream`] under a materialization bound.
///
/// It mirrors [`StreamPull`] with a `Collected` success arm, so every terminal
/// state a bounded drain can reach is explicit.
#[derive(Debug)]
pub enum CollectOutcome {
    /// The stream reached `End` within the limit; all items are collected.
    Collected(Vec<Value>),
    /// Collecting would have exceeded `limit` items; the stream is not drained
    /// further and nothing is materialized past the bound.
    LimitExceeded { limit: usize },
    /// The producer raised a runtime error mid-drain.
    Failed(RuntimeError),
    /// The carried cancellation token tripped mid-drain.
    Cancelled(CancelReason),
}

/// A capacity-capped FIFO of values, the backpressure primitive for a
/// producer/consumer bridge.
///
/// A producer offers values with [`try_push`](BoundedQueue::try_push); once the
/// queue holds `capacity` items the push is refused and the value handed back, so
/// staged memory is bounded. A consumer frees exactly one slot per
/// [`pop`](BoundedQueue::pop).
#[derive(Clone, Debug)]
pub struct BoundedQueue {
    items: VecDeque<Value>,
    capacity: usize,
}

/// A [`BoundedQueue::try_push`] refused because the queue is at capacity; it
/// returns the rejected value so a producer can retry it after a `pop`.
#[derive(Clone, Debug)]
pub struct QueueFull(pub Value);

impl BoundedQueue {
    /// A queue with [`DEFAULT_CAPACITY`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A queue that holds at most `capacity` items.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero: a stream that can never stage a value is a
    /// construction bug, not a runtime state.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(
            capacity >= 1,
            "a bounded queue needs a capacity of at least one"
        );
        Self {
            items: VecDeque::new(),
            capacity,
        }
    }

    /// The maximum number of items this queue stages.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of items currently staged.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no items are currently staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Offers `value` to the back of the queue, refusing it when the queue is
    /// full and handing it back through [`QueueFull`].
    pub fn try_push(&mut self, value: Value) -> Result<(), QueueFull> {
        if self.items.len() >= self.capacity {
            return Err(QueueFull(value));
        }
        self.items.push_back(value);
        Ok(())
    }

    /// Removes and returns the front value, or `None` when empty.
    pub fn pop(&mut self) -> Option<Value> {
        self.items.pop_front()
    }
}

impl Default for BoundedQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// The pull source backing a [`ValueStream`].
enum Source {
    /// A single value not yet pulled.
    Once(Option<Value>),
    /// An eager backing sequence drained by cursor.
    Values { values: Vec<Value>, cursor: usize },
    /// A drained staging queue.
    Queue(BoundedQueue),
    /// A lazy producer advanced once per pull; `None` is exhaustion.
    Producer(Box<dyn FnMut() -> Option<Result<Value, RuntimeError>>>),
}

impl Source {
    /// Advances the source by one item, independent of cancellation.
    fn advance(&mut self) -> StreamPull {
        match self {
            Self::Once(slot) => match slot.take() {
                Some(value) => StreamPull::Item(value),
                None => StreamPull::End,
            },
            Self::Values { values, cursor } => match values.get(*cursor) {
                Some(value) => {
                    *cursor += 1;
                    StreamPull::Item(value.clone())
                }
                None => StreamPull::End,
            },
            Self::Queue(queue) => match queue.pop() {
                Some(value) => StreamPull::Item(value),
                None => StreamPull::End,
            },
            Self::Producer(producer) => match producer() {
                Some(Ok(value)) => StreamPull::Item(value),
                Some(Err(error)) => StreamPull::Failed(error),
                None => StreamPull::End,
            },
        }
    }
}

/// A lazy, pull-driven sequence of values behind one structured pipeline edge.
pub struct ValueStream {
    source: Source,
    cancel: CancellationToken,
}

impl ValueStream {
    /// A stream of exactly one value.
    #[must_use]
    pub fn once(value: Value) -> Self {
        Self::with_source(Source::Once(Some(value)))
    }

    /// A stream that drains an eager backing sequence in order.
    #[must_use]
    pub fn from_values(values: Vec<Value>) -> Self {
        Self::with_source(Source::Values { values, cursor: 0 })
    }

    /// A stream that drains the current contents of `queue` in FIFO order.
    #[must_use]
    pub fn from_queue(queue: BoundedQueue) -> Self {
        Self::with_source(Source::Queue(queue))
    }

    /// A stream advanced by a lazy producer, once per pull. `None` is exhaustion.
    #[must_use]
    pub fn from_fn(
        producer: impl FnMut() -> Option<Result<Value, RuntimeError>> + 'static,
    ) -> Self {
        Self::with_source(Source::Producer(Box::new(producer)))
    }

    fn with_source(source: Source) -> Self {
        Self {
            source,
            cancel: CancellationToken::never(),
        }
    }

    /// Attaches a cooperative cancellation token, polled before each pull.
    #[must_use]
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    /// Pulls the next item.
    ///
    /// The cancellation token is polled first, so a tripped token yields
    /// `Cancelled` without advancing the source. Otherwise the source produces one
    /// item, exhaustion, or a producer failure. This is deliberately not
    /// `Iterator::next`: the outcome is a four-arm [`StreamPull`], not an
    /// `Option`, so cancellation and failure stay first-class terminal states.
    pub fn pull(&mut self) -> StreamPull {
        if self.cancel.is_cancelled() {
            return StreamPull::Cancelled(self.cancel.reason());
        }
        self.source.advance()
    }

    /// Drains the stream into a vector, refusing to materialize more than `limit`
    /// items.
    ///
    /// A stream that reaches `End` within the bound is `Collected`; one that would
    /// exceed it is `LimitExceeded` and is not drained further. A producer error or
    /// a cancellation observed mid-drain is reported as-is.
    pub fn collect_bounded(&mut self, limit: usize) -> CollectOutcome {
        let mut collected = Vec::new();
        loop {
            if collected.len() == limit {
                // The next pull would push the vector past the bound; stop before
                // materializing it so an unbounded producer never runs away.
                return match self.pull() {
                    StreamPull::End => CollectOutcome::Collected(collected),
                    StreamPull::Item(_) => CollectOutcome::LimitExceeded { limit },
                    StreamPull::Failed(error) => CollectOutcome::Failed(error),
                    StreamPull::Cancelled(reason) => CollectOutcome::Cancelled(reason),
                };
            }
            match self.pull() {
                StreamPull::Item(value) => collected.push(value),
                StreamPull::End => return CollectOutcome::Collected(collected),
                StreamPull::Failed(error) => return CollectOutcome::Failed(error),
                StreamPull::Cancelled(reason) => return CollectOutcome::Cancelled(reason),
            }
        }
    }
}
