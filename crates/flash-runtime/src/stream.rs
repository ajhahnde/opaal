//! Lazy value and byte streams carried by structured pipeline edges.
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
//! bounded consumer never materializes fully. [`ByteStream`] provides the same
//! owned pull boundary for byte-preserving chunks and retains source failure and
//! cancellation without decoding.
//!
//! A value stream also carries one [`StreamSchema`]: its element type, declared
//! cardinality, and runtime-only [`StreamOwnerId`]. Checked v2 consumers enforce
//! that schema, latch the first terminal state, and close the owned resource at
//! most once. Legacy v1 consumers retain the original untyped [`StreamPull`]
//! boundary; they do not acquire an inferred v2 contract.
//!
//! The whole layer is span-independent, matching [`crate::resolve`] and
//! [`crate::operation`]: a cancellation reports only its [`CancelReason`], and
//! the executor that later drives a stream inside a pipeline attaches source
//! spans at the pipeline boundary. Nothing here touches a process, terminal, or
//! clock.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Value;
use crate::eval::{CancelReason, CancellationToken, RuntimeError};
use crate::module::ValueType;

static NEXT_STREAM_OWNER: AtomicU64 = AtomicU64::new(1);

/// Runtime-only identity of the resource that owns one stream.
///
/// The identity is deliberately opaque and has no serialization contract. It
/// exists so planners and fake schedules can prove that a single-consumer
/// stream is not routed to two consumers.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamOwnerId(u64);

impl StreamOwnerId {
    fn fresh() -> Self {
        let mut owner = NEXT_STREAM_OWNER.load(Ordering::Relaxed);
        loop {
            let next = owner
                .checked_add(1)
                .expect("stream owner identity space is exhausted");
            match NEXT_STREAM_OWNER.compare_exchange_weak(
                owner,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Self(owner),
                Err(observed) => owner = observed,
            }
        }
    }
}

/// Declared cardinality of a typed value stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamCardinality {
    /// The producer must yield exactly this many items.
    Exact(usize),
    /// The producer may yield no more than this many items.
    AtMost(usize),
    /// The producer has no statically known finite cardinality.
    Unknown,
}

/// Host-free type, cardinality, and ownership metadata for one value stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamSchema {
    element_type: ValueType,
    cardinality: StreamCardinality,
    owner: StreamOwnerId,
}

impl StreamSchema {
    /// Builds one stream schema with a fresh runtime-only owner.
    #[must_use]
    pub fn new(element_type: ValueType, cardinality: StreamCardinality) -> Self {
        Self {
            element_type,
            cardinality,
            owner: StreamOwnerId::fresh(),
        }
    }

    /// The exact item type promised by the producer.
    #[must_use]
    pub const fn element_type(&self) -> &ValueType {
        &self.element_type
    }

    /// The declared number of items, when bounded statically.
    #[must_use]
    pub const fn cardinality(&self) -> StreamCardinality {
        self.cardinality
    }

    /// The resource owner that may be claimed by exactly one consumer.
    #[must_use]
    pub const fn owner(&self) -> StreamOwnerId {
        self.owner
    }
}

/// A producer violated the type or cardinality declared in its stream schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamContractViolation {
    /// An item did not belong to the declared element type.
    ElementType {
        expected: ValueType,
        actual: &'static str,
    },
    /// A producer yielded more items than its exact or upper-bound cardinality.
    CardinalityExceeded {
        declared: StreamCardinality,
        observed: usize,
    },
    /// An exact-cardinality producer ended before yielding every promised item.
    CardinalityShortfall { expected: usize, observed: usize },
}

/// A deterministic failure reported while closing an owned stream resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamCleanupFailure(Arc<str>);

impl StreamCleanupFailure {
    /// Builds cleanup evidence owned by the stream adapter.
    #[must_use]
    pub fn new(message: impl Into<Arc<str>>) -> Self {
        Self(message.into())
    }

    /// Stable adapter-supplied failure detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StreamCleanupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for StreamCleanupFailure {}

/// A schema-checking pull from a typed [`ValueStream`].
#[derive(Debug)]
pub enum CheckedStreamPull {
    Item(Value),
    End,
    Failed(RuntimeError),
    Cancelled(CancelReason),
    ContractViolation(StreamContractViolation),
}

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

/// The outcome of pulling one chunk from a [`ByteStream`].
#[derive(Debug)]
pub enum BytePull {
    /// The next byte-preserving chunk.
    Chunk(Vec<u8>),
    /// The source is exhausted; further pulls stay `End`.
    End,
    /// The producer raised a source-spanned runtime error.
    Failed(RuntimeError),
    /// The carried cancellation token tripped before the source advanced.
    Cancelled(CancelReason),
}

enum ByteSource {
    Chunks { chunks: Vec<Vec<u8>>, cursor: usize },
    Puller(Box<dyn FnMut() -> BytePull>),
}

impl ByteSource {
    fn advance(&mut self) -> BytePull {
        match self {
            Self::Chunks { chunks, cursor } => match chunks.get(*cursor) {
                Some(chunk) => {
                    *cursor += 1;
                    BytePull::Chunk(chunk.clone())
                }
                None => BytePull::End,
            },
            Self::Puller(puller) => puller(),
        }
    }
}

/// A lazy, pull-driven sequence of byte-preserving chunks.
pub struct ByteStream {
    source: ByteSource,
    state: Box<ByteStreamState>,
}

struct ByteStreamState {
    cancel: CancellationToken,
    owner: StreamOwnerId,
    terminal: bool,
}

impl ByteStream {
    /// Build a stream that drains an eager chunk sequence in order.
    #[must_use]
    pub fn from_chunks(chunks: Vec<Vec<u8>>) -> Self {
        Self::with_source(ByteSource::Chunks { chunks, cursor: 0 })
    }

    /// Build a lazy stream that preserves every first-class terminal state.
    #[must_use]
    pub fn from_pull_fn(producer: impl FnMut() -> BytePull + 'static) -> Self {
        Self::with_source(ByteSource::Puller(Box::new(producer)))
    }

    fn with_source(source: ByteSource) -> Self {
        Self {
            source,
            state: Box::new(ByteStreamState {
                cancel: CancellationToken::never(),
                owner: StreamOwnerId::fresh(),
                terminal: false,
            }),
        }
    }

    /// The runtime-only owner of this byte-preserving stream.
    #[must_use]
    pub const fn owner(&self) -> StreamOwnerId {
        self.state.owner
    }

    /// Attach a cooperative cancellation token, polled before each pull.
    #[must_use]
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.state.cancel = token;
        self
    }

    /// Pull the next chunk or terminal state.
    pub fn pull(&mut self) -> BytePull {
        if self.state.terminal {
            return BytePull::End;
        }
        if self.state.cancel.is_cancelled() {
            self.state.terminal = true;
            return BytePull::Cancelled(self.state.cancel.reason());
        }
        let pulled = self.source.advance();
        if !matches!(pulled, BytePull::Chunk(_)) {
            self.state.terminal = true;
        }
        pulled
    }
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
    /// A lazy transformer that preserves every first-class stream terminal
    /// state instead of folding cancellation into an error.
    Puller(Box<dyn FnMut() -> StreamPull>),
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
            Self::Puller(puller) => puller(),
        }
    }
}

/// A lazy, pull-driven, single-consumer sequence behind one pipeline edge.
///
/// The stream is moved into its consumer. Its schema makes the element,
/// cardinality, and owner contract inspectable without advancing the producer.
pub struct ValueStream {
    source: Source,
    state: Box<ValueStreamState>,
}

struct ValueStreamState {
    cancel: CancellationToken,
    schema: StreamSchema,
    checked_items: usize,
    terminal: bool,
    cleanup: Option<Box<dyn FnMut() -> Result<(), StreamCleanupFailure>>>,
    closed: bool,
}

impl ValueStream {
    /// A stream of exactly one value.
    #[must_use]
    pub fn once(value: Value) -> Self {
        Self::with_source(Source::Once(Some(value)), StreamCardinality::Exact(1))
    }

    /// A stream that drains an eager backing sequence in order.
    #[must_use]
    pub fn from_values(values: Vec<Value>) -> Self {
        let cardinality = StreamCardinality::Exact(values.len());
        Self::with_source(Source::Values { values, cursor: 0 }, cardinality)
    }

    /// A stream that drains the current contents of `queue` in FIFO order.
    #[must_use]
    pub fn from_queue(queue: BoundedQueue) -> Self {
        let cardinality = StreamCardinality::Exact(queue.len());
        Self::with_source(Source::Queue(queue), cardinality)
    }

    /// A stream advanced by a lazy producer, once per pull. `None` is exhaustion.
    #[must_use]
    pub fn from_fn(
        producer: impl FnMut() -> Option<Result<Value, RuntimeError>> + 'static,
    ) -> Self {
        Self::with_source(
            Source::Producer(Box::new(producer)),
            StreamCardinality::Unknown,
        )
    }

    /// A stream advanced by a lazy producer that returns the complete
    /// [`StreamPull`] state.
    ///
    /// This is the adapter for internal pipeline transformers whose upstream can
    /// end, fail, or cancel independently. The stream's own cancellation token
    /// is still polled before the producer is advanced.
    #[must_use]
    pub fn from_pull_fn(producer: impl FnMut() -> StreamPull + 'static) -> Self {
        Self::with_source(
            Source::Puller(Box::new(producer)),
            StreamCardinality::Unknown,
        )
    }

    fn with_source(source: Source, cardinality: StreamCardinality) -> Self {
        Self {
            source,
            state: Box::new(ValueStreamState {
                cancel: CancellationToken::never(),
                schema: StreamSchema::new(ValueType::Any, cardinality),
                checked_items: 0,
                terminal: false,
                cleanup: None,
                closed: false,
            }),
        }
    }

    /// Declares the element type and cardinality enforced by checked consumers.
    #[must_use]
    pub fn with_contract(
        mut self,
        element_type: ValueType,
        cardinality: StreamCardinality,
    ) -> Self {
        self.state.schema.element_type = element_type;
        self.state.schema.cardinality = cardinality;
        self
    }

    /// Attaches the owned-resource cleanup action run at most once.
    #[must_use]
    pub fn with_cleanup(
        mut self,
        cleanup: impl FnMut() -> Result<(), StreamCleanupFailure> + 'static,
    ) -> Self {
        self.state.cleanup = Some(Box::new(cleanup));
        self
    }

    /// The type, cardinality, and owner contract carried with this stream.
    #[must_use]
    pub const fn schema(&self) -> &StreamSchema {
        &self.state.schema
    }

    /// Attaches a cooperative cancellation token, polled before each pull.
    #[must_use]
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.state.cancel = token;
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
        if self.state.terminal {
            return StreamPull::End;
        }
        if self.state.cancel.is_cancelled() {
            self.state.terminal = true;
            return StreamPull::Cancelled(self.state.cancel.reason());
        }
        let pulled = self.source.advance();
        if !matches!(pulled, StreamPull::Item(_)) {
            self.state.terminal = true;
        }
        pulled
    }

    /// Pulls one item while enforcing the declared element type and cardinality.
    pub fn pull_checked(&mut self) -> CheckedStreamPull {
        // `pull` normalizes every already-observed terminal state to `End` so
        // legacy consumers never advance a terminated producer. Do not run
        // exact-cardinality validation over that synthetic `End`: the first
        // checked terminal (including a contract violation) already owns the
        // outcome, and a later pull must not replace it with a shortfall.
        if self.state.terminal {
            return CheckedStreamPull::End;
        }
        match self.pull() {
            StreamPull::Item(value) => {
                let observed = self.state.checked_items + 1;
                let cardinality_exceeded = match self.state.schema.cardinality {
                    StreamCardinality::Exact(limit) | StreamCardinality::AtMost(limit) => {
                        observed > limit
                    }
                    StreamCardinality::Unknown => false,
                };
                if cardinality_exceeded {
                    self.state.terminal = true;
                    return CheckedStreamPull::ContractViolation(
                        StreamContractViolation::CardinalityExceeded {
                            declared: self.state.schema.cardinality,
                            observed,
                        },
                    );
                }
                if !self.state.schema.element_type.accepts(&value) {
                    self.state.terminal = true;
                    return CheckedStreamPull::ContractViolation(
                        StreamContractViolation::ElementType {
                            expected: self.state.schema.element_type.clone(),
                            actual: value.family_name(),
                        },
                    );
                }
                self.state.checked_items = observed;
                CheckedStreamPull::Item(value)
            }
            StreamPull::End => {
                if let StreamCardinality::Exact(expected) = self.state.schema.cardinality
                    && self.state.checked_items < expected
                {
                    return CheckedStreamPull::ContractViolation(
                        StreamContractViolation::CardinalityShortfall {
                            expected,
                            observed: self.state.checked_items,
                        },
                    );
                }
                CheckedStreamPull::End
            }
            StreamPull::Failed(error) => CheckedStreamPull::Failed(error),
            StreamPull::Cancelled(reason) => CheckedStreamPull::Cancelled(reason),
        }
    }

    /// Runs owned-resource cleanup at most once and returns its exact evidence.
    pub fn close(&mut self) -> Result<(), StreamCleanupFailure> {
        if self.state.closed {
            return Ok(());
        }
        self.state.closed = true;
        match self.state.cleanup.as_mut() {
            Some(cleanup) => cleanup(),
            None => Ok(()),
        }
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

impl Drop for ValueStream {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
