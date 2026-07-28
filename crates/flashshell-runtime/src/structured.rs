//! Closure-free structured commands over a value stream.
//!
//! `first`, `last`, `collect`, and `length` are the terminal commands that
//! consume a [`ValueStream`] without evaluating a closure. They are the first real
//! runtime users of the stream payload: `first n` takes the leading items while
//! pulling the source no more than `n` times (so an unbounded producer is safe),
//! `last n` keeps the trailing items, `collect` materializes a `List` value, and
//! `length` counts the items.
//!
//! Every terminal state a drain can reach is a first-class [`DrainOutcome`] arm,
//! so exhaustion, a materialization bound, a producer failure, and a cancellation
//! never fold into one another. `last`, `collect`, and `length` must read the
//! whole stream, so each takes a `limit` and refuses an oversized source rather
//! than draining without bound; `first` is bounded by its own `count` and needs
//! no limit.
//!
//! `lines` is the one reshaping command here that is not terminal: it is a lazy
//! [`ValueStream`]→[`ValueStream`] transformer, pull-driven like
//! [`crate::convert`]'s decoder, so `… | lines | first n` over an unbounded text
//! source stays bounded. It splits the text carried by upstream `String` values on
//! `\n`, dropping a single `\r` before each terminator, and does not decode bytes:
//! a byte stream reaches text through an explicit `decode` boundary first.
//!
//! `select` and `get` are the read-only record projections: both are lazy 1:1
//! transformers over a stream of records. `select` narrows each record to the
//! named columns in the requested order, and `get` extracts one field's value
//! from each record. A missing column or a non-record item is a first-class step,
//! matching the record model — records carry no implicit `null` fields.
//!
//! `sort` materializes the stream and orders it under the ratified ordering
//! contract ([`crate::operation::order`]), so it is bounded by a `limit` like
//! `collect`. It sorts values directly, or records by a named key, keeps the sort
//! stable, and reports the first incomparable pair rather than inventing a
//! ranking.
//!
//! The layer is host-free and span-independent, matching [`crate::stream`] and
//! [`crate::convert`]: nothing here touches a process, terminal, or clock. The
//! closure-driven record command `update` lives in [`crate::closure`] beside
//! `each` and `where`.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::Arc;

use crate::eval::{CancelReason, RuntimeError};
use crate::operation;
use crate::stream::{StreamPull, ValueStream};
use crate::{Record, Value};

/// The terminal outcome of draining a value stream, generic over the produced
/// result.
///
/// It mirrors [`crate::stream::StreamPull`] with a `Done` success arm and a
/// `LimitExceeded` bound arm, so a terminal drain reports every state it can
/// reach.
#[derive(Debug)]
pub enum DrainOutcome<T> {
    /// The drain finished within any bound; `T` is the produced result.
    Done(T),
    /// Reading the stream would have exceeded `limit` items; it is not drained
    /// further and nothing past the bound is materialized.
    LimitExceeded {
        /// The item bound that was reached.
        limit: usize,
    },
    /// The producer raised a runtime error mid-drain, carrying its own span.
    Failed(RuntimeError),
    /// The carried cancellation token tripped mid-drain.
    Cancelled(CancelReason),
}

/// `first n`: the leading `count` items.
///
/// The source is pulled at most `count` times, so a `first` of a small count over
/// an unbounded producer returns promptly. A shorter stream yields all of it.
#[must_use]
pub fn first(mut stream: ValueStream, count: usize) -> DrainOutcome<Vec<Value>> {
    let mut items = Vec::new();
    while items.len() < count {
        match stream.pull() {
            StreamPull::Item(value) => items.push(value),
            StreamPull::End => break,
            StreamPull::Failed(error) => return DrainOutcome::Failed(error),
            StreamPull::Cancelled(reason) => return DrainOutcome::Cancelled(reason),
        }
    }
    DrainOutcome::Done(items)
}

/// `last n`: the trailing `count` items.
///
/// Finding the tail requires reading the whole stream, so `limit` caps the total
/// items read; a stream longer than `limit` is refused. A ring buffer keeps only
/// the last `count` items, so memory is bounded by `count`, not by the stream.
#[must_use]
pub fn last(mut stream: ValueStream, count: usize, limit: usize) -> DrainOutcome<Vec<Value>> {
    let mut tail: VecDeque<Value> = VecDeque::new();
    let mut seen = 0;
    loop {
        match stream.pull() {
            StreamPull::Item(value) => {
                seen += 1;
                if seen > limit {
                    return DrainOutcome::LimitExceeded { limit };
                }
                if count > 0 {
                    if tail.len() == count {
                        tail.pop_front();
                    }
                    tail.push_back(value);
                }
            }
            StreamPull::End => return DrainOutcome::Done(tail.into()),
            StreamPull::Failed(error) => return DrainOutcome::Failed(error),
            StreamPull::Cancelled(reason) => return DrainOutcome::Cancelled(reason),
        }
    }
}

/// `collect`: the whole stream materialized as one `List` value.
///
/// Bounded by `limit`: a stream of more than `limit` items is refused rather than
/// materialized without bound. A stream of exactly `limit` items succeeds.
#[must_use]
pub fn collect(mut stream: ValueStream, limit: usize) -> DrainOutcome<Value> {
    let mut items: Vec<Value> = Vec::new();
    loop {
        match stream.pull() {
            StreamPull::Item(value) => {
                if items.len() == limit {
                    return DrainOutcome::LimitExceeded { limit };
                }
                items.push(value);
            }
            StreamPull::End => return DrainOutcome::Done(Value::List(Arc::from(items))),
            StreamPull::Failed(error) => return DrainOutcome::Failed(error),
            StreamPull::Cancelled(reason) => return DrainOutcome::Cancelled(reason),
        }
    }
}

/// `length`: the number of items as an `Int` value.
///
/// Counting requires reading the whole stream, so `limit` caps the total items
/// read; a stream longer than `limit` is refused.
#[must_use]
pub fn length(mut stream: ValueStream, limit: usize) -> DrainOutcome<Value> {
    let mut count = 0_i64;
    loop {
        match stream.pull() {
            StreamPull::Item(_) => {
                count += 1;
                if count as usize > limit {
                    return DrainOutcome::LimitExceeded { limit };
                }
            }
            StreamPull::End => return DrainOutcome::Done(Value::Int(count)),
            StreamPull::Failed(error) => return DrainOutcome::Failed(error),
            StreamPull::Cancelled(reason) => return DrainOutcome::Cancelled(reason),
        }
    }
}

/// One step of splitting a text stream into lines.
#[derive(Debug)]
pub enum LineStep {
    /// The next line as a `String` value, with its terminator and a single
    /// preceding `\r` removed.
    Line(Value),
    /// The upstream stream is exhausted and no partial line remains; further steps
    /// stay `End`.
    End,
    /// An upstream item was not text. `lines` reshapes text and does not decode:
    /// carries the offending value's family name.
    NotText {
        /// The family name of the non-text value.
        actual: &'static str,
    },
    /// The upstream producer raised a runtime error, passed through unchanged.
    Failed(RuntimeError),
    /// The upstream stream was cancelled, passed through unchanged.
    Cancelled(CancelReason),
}

/// A pull-driven line splitter produced by [`lines`].
pub struct LineSplitter {
    input: ValueStream,
    /// Text pulled but not yet emitted as a line.
    carry: LineCarry,
    /// The upstream stream reached `End`; only a trailing partial line remains to
    /// flush.
    input_ended: bool,
    /// Latched once `End`, `NotText`, `Failed`, or `Cancelled` was returned.
    done: bool,
}

/// `lines`: split the text carried by `input`'s `String` values into one line per
/// step.
///
/// The transformer is lazy — it pulls `input` only until the next terminator — so
/// a bounded downstream (`lines | first n`) stays bounded over an unbounded text
/// source.
#[must_use]
pub fn lines(input: ValueStream) -> LineSplitter {
    LineSplitter {
        input,
        carry: LineCarry::default(),
        input_ended: false,
        done: false,
    }
}

impl LineSplitter {
    /// Pulls the next line, exhaustion, a non-text report, or a passed-through
    /// upstream failure or cancellation.
    ///
    /// Deliberately not `Iterator::next`, mirroring [`crate::stream::ValueStream::pull`]:
    /// the non-`Line` steps stay first-class terminal states.
    pub fn pull(&mut self) -> LineStep {
        if self.done {
            return LineStep::End;
        }
        loop {
            if let Some(line) = self.carry.next_line() {
                return LineStep::Line(Value::string(line));
            }
            if self.input_ended {
                return self.flush_remainder();
            }
            match self.input.pull() {
                StreamPull::Item(Value::String(text)) => self.carry.push(&text),
                StreamPull::Item(other) => {
                    self.done = true;
                    return LineStep::NotText {
                        actual: other.family_name(),
                    };
                }
                StreamPull::End => self.input_ended = true,
                StreamPull::Failed(error) => {
                    self.done = true;
                    return LineStep::Failed(error);
                }
                StreamPull::Cancelled(reason) => {
                    self.done = true;
                    return LineStep::Cancelled(reason);
                }
            }
        }
    }

    /// Emits the trailing partial line, if any, then latches `End`.
    fn flush_remainder(&mut self) -> LineStep {
        self.done = true;
        match self.carry.flush() {
            Some(line) => LineStep::Line(Value::string(line)),
            None => LineStep::End,
        }
    }
}

/// Text pulled but not yet emitted as a line.
///
/// This is the single implementation of FlashShell's line-splitting rule, shared
/// by `lines` here and by the line-oriented format boundary in
/// [`crate::format`], so the two can never drift apart. A `\r\n` terminator and
/// a trailing `\r` on a flushed final line both leave no carriage return, a lone
/// interior `\r` is ordinary content, a trailing terminator emits no empty final
/// line, and interior blank lines are preserved as empty strings.
#[derive(Default)]
pub(crate) struct LineCarry {
    /// Never contains a `\n`: every terminator is consumed as it appears.
    text: String,
}

impl LineCarry {
    /// Appends freshly decoded text.
    pub(crate) fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// The number of bytes held but not yet emitted.
    pub(crate) fn len(&self) -> usize {
        self.text.len()
    }

    /// Removes and returns the next complete line, if one is present.
    pub(crate) fn next_line(&mut self) -> Option<String> {
        let newline = self.text.find('\n')?;
        let mut line: String = self.text.drain(..newline).collect();
        self.text.remove(0); // Drop the `\n`.
        strip_carriage_return(&mut line);
        Some(line)
    }

    /// Removes and returns the trailing partial line, if any. A trailing
    /// terminator leaves nothing, so no empty final line is emitted.
    pub(crate) fn flush(&mut self) -> Option<String> {
        if self.text.is_empty() {
            return None;
        }
        let mut line = std::mem::take(&mut self.text);
        strip_carriage_return(&mut line);
        Some(line)
    }
}

/// Removes a single trailing `\r`, so a `\r\n` terminator leaves no carriage
/// return on the line.
fn strip_carriage_return(line: &mut String) {
    if line.ends_with('\r') {
        line.pop();
    }
}

/// One step of narrowing a stream of records to selected columns.
#[derive(Debug)]
pub enum SelectStep {
    /// The next narrowed record, holding only the requested columns in order.
    Record(Value),
    /// The upstream stream is exhausted; further steps stay `End`.
    End,
    /// A record did not contain a requested column. Carries the column name; the
    /// executor attaches the command's source span at the pipeline boundary.
    MissingColumn {
        /// The requested column absent from the record.
        column: String,
    },
    /// An upstream item was not a record. Carries the value's family name.
    NotRecord {
        /// The family name of the non-record value.
        actual: &'static str,
    },
    /// The upstream producer raised a runtime error, passed through unchanged.
    Failed(RuntimeError),
    /// The upstream stream was cancelled, passed through unchanged.
    Cancelled(CancelReason),
}

/// A pull-driven `select` transformer produced by [`select`].
pub struct SelectStream {
    input: ValueStream,
    columns: Vec<String>,
    done: bool,
}

/// `select`: narrow each record to `columns`, in the requested order.
///
/// Repeated requested columns are deduplicated (first occurrence wins), so the
/// narrowed record always has unique keys. The transformer is lazy and 1:1: it
/// pulls one record and emits one narrowed record per [`SelectStream::pull`].
pub fn select(input: ValueStream, columns: impl IntoIterator<Item = String>) -> SelectStream {
    let mut deduplicated: Vec<String> = Vec::new();
    for column in columns {
        if !deduplicated.contains(&column) {
            deduplicated.push(column);
        }
    }
    SelectStream {
        input,
        columns: deduplicated,
        done: false,
    }
}

impl SelectStream {
    /// Pulls the next narrowed record, exhaustion, a missing column, a non-record
    /// item, or a passed-through upstream failure or cancellation.
    ///
    /// Deliberately not `Iterator::next`, mirroring [`crate::stream::ValueStream::pull`].
    pub fn pull(&mut self) -> SelectStep {
        if self.done {
            return SelectStep::End;
        }
        match self.input.pull() {
            StreamPull::Item(Value::Record(record)) => {
                let mut entries: Vec<(String, Value)> = Vec::with_capacity(self.columns.len());
                for column in &self.columns {
                    match record.get(column) {
                        Some(value) => entries.push((column.clone(), value.clone())),
                        None => {
                            self.done = true;
                            return SelectStep::MissingColumn {
                                column: column.clone(),
                            };
                        }
                    }
                }
                let narrowed = Record::new(entries).expect("deduplicated columns are unique");
                SelectStep::Record(Value::Record(narrowed))
            }
            StreamPull::Item(other) => {
                self.done = true;
                SelectStep::NotRecord {
                    actual: other.family_name(),
                }
            }
            StreamPull::End => {
                self.done = true;
                SelectStep::End
            }
            StreamPull::Failed(error) => {
                self.done = true;
                SelectStep::Failed(error)
            }
            StreamPull::Cancelled(reason) => {
                self.done = true;
                SelectStep::Cancelled(reason)
            }
        }
    }
}

/// One step of extracting a field from a stream of records.
#[derive(Debug)]
pub enum GetStep {
    /// The next extracted value: the field at the requested key.
    Value(Value),
    /// The upstream stream is exhausted; further steps stay `End`.
    End,
    /// A record did not contain the requested key. Carries the key; the executor
    /// attaches the command's source span at the pipeline boundary.
    MissingKey {
        /// The requested key absent from the record.
        key: String,
    },
    /// An upstream item was not a record. Carries the value's family name.
    NotRecord {
        /// The family name of the non-record value.
        actual: &'static str,
    },
    /// The upstream producer raised a runtime error, passed through unchanged.
    Failed(RuntimeError),
    /// The upstream stream was cancelled, passed through unchanged.
    Cancelled(CancelReason),
}

/// A pull-driven `get` transformer produced by [`get`].
pub struct GetStream {
    input: ValueStream,
    key: String,
    done: bool,
}

/// `get`: extract the value at `key` from each record.
///
/// The transformer is lazy and 1:1: it pulls one record and emits its field value
/// per [`GetStream::pull`].
pub fn get(input: ValueStream, key: String) -> GetStream {
    GetStream {
        input,
        key,
        done: false,
    }
}

impl GetStream {
    /// Pulls the next field value, exhaustion, a missing key, a non-record item, or
    /// a passed-through upstream failure or cancellation.
    ///
    /// Deliberately not `Iterator::next`, mirroring [`crate::stream::ValueStream::pull`].
    pub fn pull(&mut self) -> GetStep {
        if self.done {
            return GetStep::End;
        }
        match self.input.pull() {
            StreamPull::Item(Value::Record(record)) => match record.get(&self.key) {
                Some(value) => GetStep::Value(value.clone()),
                None => {
                    self.done = true;
                    GetStep::MissingKey {
                        key: self.key.clone(),
                    }
                }
            },
            StreamPull::Item(other) => {
                self.done = true;
                GetStep::NotRecord {
                    actual: other.family_name(),
                }
            }
            StreamPull::End => {
                self.done = true;
                GetStep::End
            }
            StreamPull::Failed(error) => {
                self.done = true;
                GetStep::Failed(error)
            }
            StreamPull::Cancelled(reason) => {
                self.done = true;
                GetStep::Cancelled(reason)
            }
        }
    }
}

/// The outcome of `sort`, which materializes the stream to order it.
#[derive(Debug)]
pub enum SortOutcome {
    /// The stream was drained within the limit and ordered.
    Sorted(Vec<Value>),
    /// Draining the stream would have exceeded `limit` items; it is refused rather
    /// than materialized without bound.
    LimitExceeded {
        /// The item bound that was reached.
        limit: usize,
    },
    /// Two values could not be ordered under the ratified ordering contract.
    /// Carries the family names of the first incomparable pair encountered; the
    /// executor attaches the command's source span at the pipeline boundary.
    Incomparable {
        /// The left value's family name.
        left: &'static str,
        /// The right value's family name.
        right: &'static str,
    },
    /// Sorting by a key met a record without that key. Carries the key.
    MissingKey {
        /// The sort key absent from a record.
        key: String,
    },
    /// Sorting by a key met a non-record item. Carries the value's family name.
    NotRecord {
        /// The family name of the non-record value.
        actual: &'static str,
    },
    /// The producer raised a runtime error mid-drain, carrying its own span.
    Failed(RuntimeError),
    /// The carried cancellation token tripped mid-drain.
    Cancelled(CancelReason),
}

/// `sort`: materialize the stream and order it under the ratified ordering
/// contract.
///
/// Sorting requires reading the whole stream, so `limit` caps the total items
/// read; a stream longer than `limit` is refused. With no `key` the values are
/// compared directly; with a `key` each item must be a record and is ordered by
/// the value at that key. Ordering is stable — items that compare equal keep their
/// input order — and the first pair the ordering contract cannot compare is an
/// `Incomparable` outcome rather than an arbitrary ranking.
#[must_use]
pub fn sort(mut stream: ValueStream, key: Option<String>, limit: usize) -> SortOutcome {
    let mut items: Vec<Value> = Vec::new();
    loop {
        match stream.pull() {
            StreamPull::Item(value) => {
                if items.len() == limit {
                    return SortOutcome::LimitExceeded { limit };
                }
                items.push(value);
            }
            StreamPull::End => break,
            StreamPull::Failed(error) => return SortOutcome::Failed(error),
            StreamPull::Cancelled(reason) => return SortOutcome::Cancelled(reason),
        }
    }

    match key {
        None => {
            let mut error = None;
            items.sort_by(|left, right| capture_order(left, right, &mut error));
            match error {
                Some((left, right)) => SortOutcome::Incomparable { left, right },
                None => SortOutcome::Sorted(items),
            }
        }
        Some(key) => sort_by_key(items, &key),
    }
}

/// Orders `items` by each record's value at `key`, keeping the sort stable.
fn sort_by_key(items: Vec<Value>, key: &str) -> SortOutcome {
    let mut keys: Vec<Value> = Vec::with_capacity(items.len());
    for item in &items {
        match item {
            Value::Record(record) => match record.get(key) {
                Some(value) => keys.push(value.clone()),
                None => {
                    return SortOutcome::MissingKey {
                        key: key.to_owned(),
                    };
                }
            },
            other => {
                return SortOutcome::NotRecord {
                    actual: other.family_name(),
                };
            }
        }
    }

    // Sort indices so the reorder stays stable and the extracted keys are compared
    // rather than the whole records.
    let mut indices: Vec<usize> = (0..items.len()).collect();
    let mut error = None;
    indices.sort_by(|&left, &right| capture_order(&keys[left], &keys[right], &mut error));
    if let Some((left, right)) = error {
        return SortOutcome::Incomparable { left, right };
    }
    let sorted = indices
        .into_iter()
        .map(|index| items[index].clone())
        .collect();
    SortOutcome::Sorted(sorted)
}

/// Compares two values under the ratified ordering contract, recording the first
/// incomparable pair's family names and treating it as `Equal` so the sort still
/// terminates.
fn capture_order(
    left: &Value,
    right: &Value,
    error: &mut Option<(&'static str, &'static str)>,
) -> Ordering {
    match operation::order(left, right) {
        Ok(ordering) => ordering,
        Err(_) => {
            if error.is_none() {
                *error = Some((left.family_name(), right.family_name()));
            }
            Ordering::Equal
        }
    }
}
