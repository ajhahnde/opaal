//! Closure-driven structured commands over a value stream.
//!
//! `each` (map), `where` (filter), and `update` (per-record field replacement)
//! apply a runtime closure to values a [`ValueStream`] yields. They are the
//! structured commands that are neither
//! host-free nor span-independent: each threads the pure evaluator and a closure
//! through the stream, so it carries the closure's [`SourceFile`] and the driving
//! command's [`Span`], and shares one [`Environment`] and one [`EvalLimits`]
//! across every application. The closure-invocation seam is
//! [`crate::eval::apply_callable`].
//!
//! All three are lazy transformers — [`each`] returns an [`EachStream`],
//! [`r#where`] a [`WhereStream`], and [`update`] an [`UpdateStream`], each
//! pull-driven like [`crate::convert`]'s encoder — so behind a bounded downstream
//! they stay bounded over an unbounded producer. Owned counterparts use one
//! [`OwnedClosureContext`] when a stream must outlive stage dispatch. Every
//! terminal state is a first-class step: exhaustion, an upstream producer
//! failure, a cancellation (either from upstream or from the shared token during
//! a closure application), a closure runtime error, and the command-specific
//! faults — a non-boolean `where` predicate, and a missing key or non-record item
//! for `update`. The live executor attaches the command's source span at the
//! boundary. `update`'s replacement is applied as a closure when it is callable
//! and used verbatim otherwise.

use std::cell::RefCell;
use std::rc::Rc;

use crate::eval::{CancelReason, Completion, EvalLimits, RuntimeError, apply_callable};
use crate::stream::{StreamPull, ValueStream};
use crate::{Environment, Record, Value};
use flash_syntax::{SourceFile, Span};

/// Owned evaluator context shared by lazy closure-driven pipeline stages.
///
/// The source and limits are immutable. The environment is shared through
/// interior mutability so a closure stream can be stored in an owned
/// [`ValueStream`] and still preserve environment changes across successive
/// pulls. The session snapshots it only after the complete pipeline succeeds.
#[derive(Clone)]
pub struct OwnedClosureContext {
    source: SourceFile,
    environment: Rc<RefCell<Environment>>,
    limits: EvalLimits,
}

impl OwnedClosureContext {
    /// Build a transactional closure context from one submitted source and
    /// environment snapshot.
    #[must_use]
    pub fn new(source: SourceFile, environment: Environment, limits: EvalLimits) -> Self {
        Self {
            source,
            environment: Rc::new(RefCell::new(environment)),
            limits,
        }
    }

    /// Clone the environment after all lazy closure applications have finished.
    #[must_use]
    pub fn environment_snapshot(&self) -> Environment {
        self.environment.borrow().clone()
    }

    fn apply(
        &self,
        callable: &Value,
        arguments: Vec<Value>,
        span: Span,
    ) -> Result<Completion, RuntimeError> {
        apply_callable(
            callable,
            arguments,
            &self.source,
            span,
            &mut self.environment.borrow_mut(),
            &self.limits,
        )
    }
}

/// One step of mapping a value stream through a closure.
#[derive(Debug)]
pub enum EachStep {
    /// The next value: the closure's result for the corresponding source item.
    Item(Value),
    /// The upstream stream is exhausted; further steps stay `End`.
    End,
    /// The upstream producer failed, or the closure raised a runtime error (a
    /// non-callable value, an arity mismatch, or an error inside the body).
    Failed(RuntimeError),
    /// The upstream stream was cancelled, or the shared token tripped while
    /// applying the closure.
    Cancelled(CancelReason),
}

/// A pull-driven `each` (map) transformer produced by [`each`].
pub struct EachStream<'a> {
    input: ValueStream,
    closure: Value,
    source: &'a SourceFile,
    span: Span,
    env: &'a mut Environment,
    limits: &'a EvalLimits,
    done: bool,
}

/// `each`: apply `closure` to every value, yielding each result in turn.
///
/// The transformer is lazy: it pulls `input` once and applies `closure` once per
/// [`EachStream::pull`], so a bounded downstream stays bounded over an unbounded
/// source. `source`, `span`, `env`, and `limits` drive the closure application
/// through [`apply_callable`].
pub fn each<'a>(
    input: ValueStream,
    closure: Value,
    source: &'a SourceFile,
    span: Span,
    env: &'a mut Environment,
    limits: &'a EvalLimits,
) -> EachStream<'a> {
    EachStream {
        input,
        closure,
        source,
        span,
        env,
        done: false,
        limits,
    }
}

impl EachStream<'_> {
    /// Pulls the next mapped value, exhaustion, a failure, or a cancellation.
    ///
    /// Deliberately not `Iterator::next`, mirroring [`crate::stream::ValueStream::pull`].
    pub fn pull(&mut self) -> EachStep {
        if self.done {
            return EachStep::End;
        }
        match self.input.pull() {
            StreamPull::Item(value) => match apply_callable(
                &self.closure,
                vec![value],
                self.source,
                self.span,
                self.env,
                self.limits,
            ) {
                Ok(Completion::Value(mapped)) => EachStep::Item(mapped),
                Ok(Completion::Cancelled(cancellation)) => {
                    self.done = true;
                    EachStep::Cancelled(cancellation.reason())
                }
                Err(error) => {
                    self.done = true;
                    EachStep::Failed(error)
                }
            },
            StreamPull::End => {
                self.done = true;
                EachStep::End
            }
            StreamPull::Failed(error) => {
                self.done = true;
                EachStep::Failed(error)
            }
            StreamPull::Cancelled(reason) => {
                self.done = true;
                EachStep::Cancelled(reason)
            }
        }
    }
}

/// Owned lazy `each` transformer for the live internal executor.
pub struct OwnedEachStream {
    input: ValueStream,
    closure: Value,
    span: Span,
    context: OwnedClosureContext,
    done: bool,
}

/// Build an owned lazy `each` transformer.
#[must_use]
pub fn each_owned(
    input: ValueStream,
    closure: Value,
    span: Span,
    context: OwnedClosureContext,
) -> OwnedEachStream {
    OwnedEachStream {
        input,
        closure,
        span,
        context,
        done: false,
    }
}

impl OwnedEachStream {
    /// Pull the next mapped value while retaining the complete terminal-state
    /// contract of [`EachStream::pull`].
    pub fn pull(&mut self) -> EachStep {
        if self.done {
            return EachStep::End;
        }
        match self.input.pull() {
            StreamPull::Item(value) => {
                match self.context.apply(&self.closure, vec![value], self.span) {
                    Ok(Completion::Value(mapped)) => EachStep::Item(mapped),
                    Ok(Completion::Cancelled(cancellation)) => {
                        self.done = true;
                        EachStep::Cancelled(cancellation.reason())
                    }
                    Err(error) => {
                        self.done = true;
                        EachStep::Failed(error)
                    }
                }
            }
            StreamPull::End => {
                self.done = true;
                EachStep::End
            }
            StreamPull::Failed(error) => {
                self.done = true;
                EachStep::Failed(error)
            }
            StreamPull::Cancelled(reason) => {
                self.done = true;
                EachStep::Cancelled(reason)
            }
        }
    }
}

/// One step of filtering a value stream through a boolean-returning closure.
#[derive(Debug)]
pub enum WhereStep {
    /// The next value: a source item whose predicate evaluated to `true`.
    Item(Value),
    /// The upstream stream is exhausted; further steps stay `End`.
    End,
    /// The predicate evaluated to a non-boolean value. Carries the family name;
    /// the executor attaches the command's source span at the pipeline boundary.
    PredicateNotBool {
        /// The family name of the non-boolean predicate result.
        actual: &'static str,
    },
    /// The upstream producer failed, or the closure raised a runtime error (a
    /// non-callable value, an arity mismatch, or an error inside the body).
    Failed(RuntimeError),
    /// The upstream stream was cancelled, or the shared token tripped while
    /// applying the closure.
    Cancelled(CancelReason),
}

/// A pull-driven `where` (filter) transformer produced by [`r#where`].
pub struct WhereStream<'a> {
    input: ValueStream,
    closure: Value,
    source: &'a SourceFile,
    span: Span,
    env: &'a mut Environment,
    limits: &'a EvalLimits,
    done: bool,
}

/// `where`: yield only the values whose `closure` predicate evaluates to `true`.
///
/// The predicate must return a boolean; any other family is a
/// [`WhereStep::PredicateNotBool`], matching the strict boolean rule the language
/// uses for conditions. The transformer is lazy — it pulls and tests one value per
/// [`WhereStream::pull`], skipping non-matching values without buffering — so it
/// stays bounded behind a bounded downstream.
pub fn r#where<'a>(
    input: ValueStream,
    closure: Value,
    source: &'a SourceFile,
    span: Span,
    env: &'a mut Environment,
    limits: &'a EvalLimits,
) -> WhereStream<'a> {
    WhereStream {
        input,
        closure,
        source,
        span,
        env,
        done: false,
        limits,
    }
}

impl WhereStream<'_> {
    /// Pulls the next matching value, exhaustion, a non-boolean predicate, a
    /// failure, or a cancellation.
    ///
    /// Deliberately not `Iterator::next`, mirroring [`crate::stream::ValueStream::pull`].
    pub fn pull(&mut self) -> WhereStep {
        if self.done {
            return WhereStep::End;
        }
        loop {
            match self.input.pull() {
                StreamPull::Item(value) => match apply_callable(
                    &self.closure,
                    vec![value.clone()],
                    self.source,
                    self.span,
                    self.env,
                    self.limits,
                ) {
                    Ok(Completion::Value(Value::Bool(true))) => return WhereStep::Item(value),
                    Ok(Completion::Value(Value::Bool(false))) => {}
                    Ok(Completion::Value(other)) => {
                        self.done = true;
                        return WhereStep::PredicateNotBool {
                            actual: other.family_name(),
                        };
                    }
                    Ok(Completion::Cancelled(cancellation)) => {
                        self.done = true;
                        return WhereStep::Cancelled(cancellation.reason());
                    }
                    Err(error) => {
                        self.done = true;
                        return WhereStep::Failed(error);
                    }
                },
                StreamPull::End => {
                    self.done = true;
                    return WhereStep::End;
                }
                StreamPull::Failed(error) => {
                    self.done = true;
                    return WhereStep::Failed(error);
                }
                StreamPull::Cancelled(reason) => {
                    self.done = true;
                    return WhereStep::Cancelled(reason);
                }
            }
        }
    }
}

/// Owned lazy `where` transformer for the live internal executor.
pub struct OwnedWhereStream {
    input: ValueStream,
    closure: Value,
    span: Span,
    context: OwnedClosureContext,
    done: bool,
}

/// Build an owned lazy `where` transformer.
#[must_use]
pub fn where_owned(
    input: ValueStream,
    closure: Value,
    span: Span,
    context: OwnedClosureContext,
) -> OwnedWhereStream {
    OwnedWhereStream {
        input,
        closure,
        span,
        context,
        done: false,
    }
}

impl OwnedWhereStream {
    /// Pull the next matching value while retaining the complete terminal-state
    /// contract of [`WhereStream::pull`].
    pub fn pull(&mut self) -> WhereStep {
        if self.done {
            return WhereStep::End;
        }
        loop {
            match self.input.pull() {
                StreamPull::Item(value) => {
                    match self
                        .context
                        .apply(&self.closure, vec![value.clone()], self.span)
                    {
                        Ok(Completion::Value(Value::Bool(true))) => {
                            return WhereStep::Item(value);
                        }
                        Ok(Completion::Value(Value::Bool(false))) => {}
                        Ok(Completion::Value(other)) => {
                            self.done = true;
                            return WhereStep::PredicateNotBool {
                                actual: other.family_name(),
                            };
                        }
                        Ok(Completion::Cancelled(cancellation)) => {
                            self.done = true;
                            return WhereStep::Cancelled(cancellation.reason());
                        }
                        Err(error) => {
                            self.done = true;
                            return WhereStep::Failed(error);
                        }
                    }
                }
                StreamPull::End => {
                    self.done = true;
                    return WhereStep::End;
                }
                StreamPull::Failed(error) => {
                    self.done = true;
                    return WhereStep::Failed(error);
                }
                StreamPull::Cancelled(reason) => {
                    self.done = true;
                    return WhereStep::Cancelled(reason);
                }
            }
        }
    }
}

/// One step of updating a field in a stream of records.
#[derive(Debug)]
pub enum UpdateStep {
    /// The next record, with the target field replaced.
    Record(Value),
    /// The upstream stream is exhausted; further steps stay `End`.
    End,
    /// A record did not contain the target key. Carries the key; the executor
    /// attaches the command's source span at the pipeline boundary.
    MissingKey {
        /// The target key absent from the record.
        key: String,
    },
    /// An upstream item was not a record. Carries the value's family name.
    NotRecord {
        /// The family name of the non-record value.
        actual: &'static str,
    },
    /// The upstream producer failed, or a closure replacement raised a runtime
    /// error (an arity mismatch, or an error inside the body).
    Failed(RuntimeError),
    /// The upstream stream was cancelled, or the shared token tripped while
    /// applying a closure replacement.
    Cancelled(CancelReason),
}

/// A pull-driven `update` transformer produced by [`update`].
pub struct UpdateStream<'a> {
    input: ValueStream,
    key: String,
    replacement: Value,
    source: &'a SourceFile,
    span: Span,
    env: &'a mut Environment,
    limits: &'a EvalLimits,
    done: bool,
}

/// `update`: replace the value at `key` in each record.
///
/// When `replacement` is callable it is applied to the field's current value (the
/// result is the new value); otherwise it replaces the field verbatim. The
/// transformer is lazy and 1:1 — one record in, one updated record out per
/// [`UpdateStream::pull`] — and preserves the record's key order. The target key
/// must exist; a missing key is a [`UpdateStep::MissingKey`], matching the record
/// model rather than inserting a new field.
pub fn update<'a>(
    input: ValueStream,
    key: String,
    replacement: Value,
    source: &'a SourceFile,
    span: Span,
    env: &'a mut Environment,
    limits: &'a EvalLimits,
) -> UpdateStream<'a> {
    UpdateStream {
        input,
        key,
        replacement,
        source,
        span,
        env,
        limits,
        done: false,
    }
}

impl UpdateStream<'_> {
    /// Pulls the next updated record, exhaustion, a missing key, a non-record item,
    /// or a passed-through failure or cancellation.
    ///
    /// Deliberately not `Iterator::next`, mirroring [`crate::stream::ValueStream::pull`].
    pub fn pull(&mut self) -> UpdateStep {
        if self.done {
            return UpdateStep::End;
        }
        match self.input.pull() {
            StreamPull::Item(Value::Record(record)) => {
                let Some(current) = record.get(&self.key).cloned() else {
                    self.done = true;
                    return UpdateStep::MissingKey {
                        key: self.key.clone(),
                    };
                };
                let replacement = match &self.replacement {
                    Value::Callable(_) => match apply_callable(
                        &self.replacement,
                        vec![current],
                        self.source,
                        self.span,
                        self.env,
                        self.limits,
                    ) {
                        Ok(Completion::Value(value)) => value,
                        Ok(Completion::Cancelled(cancellation)) => {
                            self.done = true;
                            return UpdateStep::Cancelled(cancellation.reason());
                        }
                        Err(error) => {
                            self.done = true;
                            return UpdateStep::Failed(error);
                        }
                    },
                    other => other.clone(),
                };
                let entries: Vec<(String, Value)> = record
                    .entries()
                    .iter()
                    .map(|(key, value)| {
                        if key.as_ref() == self.key {
                            (key.to_string(), replacement.clone())
                        } else {
                            (key.to_string(), value.clone())
                        }
                    })
                    .collect();
                let updated = Record::new(entries).expect("keys are unchanged and unique");
                UpdateStep::Record(Value::Record(updated))
            }
            StreamPull::Item(other) => {
                self.done = true;
                UpdateStep::NotRecord {
                    actual: other.family_name(),
                }
            }
            StreamPull::End => {
                self.done = true;
                UpdateStep::End
            }
            StreamPull::Failed(error) => {
                self.done = true;
                UpdateStep::Failed(error)
            }
            StreamPull::Cancelled(reason) => {
                self.done = true;
                UpdateStep::Cancelled(reason)
            }
        }
    }
}

/// Owned lazy `update` transformer for the live internal executor.
pub struct OwnedUpdateStream {
    input: ValueStream,
    key: String,
    replacement: Value,
    span: Span,
    context: OwnedClosureContext,
    done: bool,
}

/// Build an owned lazy `update` transformer.
#[must_use]
pub fn update_owned(
    input: ValueStream,
    key: String,
    replacement: Value,
    span: Span,
    context: OwnedClosureContext,
) -> OwnedUpdateStream {
    OwnedUpdateStream {
        input,
        key,
        replacement,
        span,
        context,
        done: false,
    }
}

impl OwnedUpdateStream {
    /// Pull the next updated record while retaining the complete terminal-state
    /// contract of [`UpdateStream::pull`].
    pub fn pull(&mut self) -> UpdateStep {
        if self.done {
            return UpdateStep::End;
        }
        match self.input.pull() {
            StreamPull::Item(Value::Record(record)) => {
                let Some(current) = record.get(&self.key).cloned() else {
                    self.done = true;
                    return UpdateStep::MissingKey {
                        key: self.key.clone(),
                    };
                };
                let replacement = match &self.replacement {
                    Value::Callable(_) => {
                        match self
                            .context
                            .apply(&self.replacement, vec![current], self.span)
                        {
                            Ok(Completion::Value(value)) => value,
                            Ok(Completion::Cancelled(cancellation)) => {
                                self.done = true;
                                return UpdateStep::Cancelled(cancellation.reason());
                            }
                            Err(error) => {
                                self.done = true;
                                return UpdateStep::Failed(error);
                            }
                        }
                    }
                    other => other.clone(),
                };
                let entries = record
                    .entries()
                    .iter()
                    .map(|(key, value)| {
                        if key.as_ref() == self.key {
                            (key.to_string(), replacement.clone())
                        } else {
                            (key.to_string(), value.clone())
                        }
                    })
                    .collect();
                let updated = Record::new(entries).expect("keys are unchanged and unique");
                UpdateStep::Record(Value::Record(updated))
            }
            StreamPull::Item(other) => {
                self.done = true;
                UpdateStep::NotRecord {
                    actual: other.family_name(),
                }
            }
            StreamPull::End => {
                self.done = true;
                UpdateStep::End
            }
            StreamPull::Failed(error) => {
                self.done = true;
                UpdateStep::Failed(error)
            }
            StreamPull::Cancelled(reason) => {
                self.done = true;
                UpdateStep::Cancelled(reason)
            }
        }
    }
}
