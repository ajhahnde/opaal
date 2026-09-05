//! Structured execution outcomes shared by evaluators, sessions, and hosts.

use std::fmt;
use std::sync::Arc;

use flash_syntax::Span;

use crate::Status;
use crate::seam::DownstreamOutcomeMetadata;

/// Why an operation was refused before its effect began.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefusalReason {
    /// A known authority contract exists but did not grant the operation.
    Denied,
    /// The selected host or language generation does not implement the operation.
    Unsupported,
    /// No executable operational contract can be established.
    Unknown,
}

impl RefusalReason {
    /// Stable machine-facing spelling of this reason.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// A structured refusal raised before an operation can begin an effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Refusal {
    reason: RefusalReason,
    operation: &'static str,
    span: Span,
}

impl Refusal {
    /// Build a refusal with its exact reason, operation class, and source span.
    #[must_use]
    pub const fn new(reason: RefusalReason, operation: &'static str, span: Span) -> Self {
        Self {
            reason,
            operation,
            span,
        }
    }

    /// Why the operation was refused.
    #[must_use]
    pub const fn reason(&self) -> RefusalReason {
        self.reason
    }

    /// Stable operation class that could not begin.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    /// Source boundary at which refusal was established.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} operation `{}` was refused before execution",
            self.reason, self.operation
        )
    }
}

/// Host boundary that failed outside language recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatalHostFailureKind {
    /// Required program output could not be delivered.
    Output,
    /// The host could not render or report the structured result.
    Reporting,
    /// A session invariant failed outside the language evaluator.
    Session,
}

impl FatalHostFailureKind {
    /// Stable machine-facing spelling of this failure family.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Output => "output",
            Self::Reporting => "reporting",
            Self::Session => "session",
        }
    }
}

/// An uncatchable host/report/session failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FatalHostFailure {
    kind: FatalHostFailureKind,
    message: Arc<str>,
}

impl FatalHostFailure {
    /// Build one fatal host failure.
    #[must_use]
    pub fn new(kind: FatalHostFailureKind, message: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Host boundary that failed.
    #[must_use]
    pub const fn kind(&self) -> FatalHostFailureKind {
        self.kind
    }

    /// Stable diagnostic detail supplied by that boundary.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for FatalHostFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} failure: {}", self.kind.name(), self.message)
    }
}

/// Evidence for a stage that completed before the primary outcome was known.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedEvidence {
    operation: Arc<str>,
    status: Option<Status>,
}

impl CompletedEvidence {
    /// Record a completed operation and its optional real status.
    #[must_use]
    pub fn new(operation: impl Into<Arc<str>>, status: Option<Status>) -> Self {
        Self {
            operation: operation.into(),
            status,
        }
    }

    /// Stable producer identity supplied by the owning adapter.
    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// Real completed status, when this operation produced one.
    #[must_use]
    pub const fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }
}

/// Evidence that an externally observable effect completed only in part.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartialEffectEvidence {
    operation: Arc<str>,
    detail: Arc<str>,
}

impl PartialEffectEvidence {
    /// Record typed opaque evidence from the adapter that owns the effect.
    #[must_use]
    pub fn new(operation: impl Into<Arc<str>>, detail: impl Into<Arc<str>>) -> Self {
        Self {
            operation: operation.into(),
            detail: detail.into(),
        }
    }

    /// Stable producer identity supplied by the owning adapter.
    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// Opaque adapter-owned evidence detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// The one primary result of an execution boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum PrimaryOutcome<T, E> {
    /// Normal completion, including an optional completed status inside `T`.
    Completed(T),
    /// A catchable language failure that escaped its source boundary.
    Error(E),
    /// Cooperative cancellation, distinct from language failure.
    Cancelled(crate::eval::Cancellation),
    /// Refusal established before the requested effect began.
    Refused(Refusal),
    /// An uncatchable host/report/session failure.
    FatalHostFailure(FatalHostFailure),
}

/// Ordered evidence retained beside, but never substituted for, the primary.
#[derive(Clone, Debug, PartialEq)]
pub enum OutcomeEvidence<E> {
    /// A stage completed before the primary outcome was selected.
    Completed(CompletedEvidence),
    /// An adapter reports already-observable partial external work.
    PartialEffect(PartialEffectEvidence),
    /// Owned-resource cleanup failed after another event.
    CleanupFailure(E),
}

/// One primary outcome plus ordered secondary evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionOutcome<T, E> {
    primary: PrimaryOutcome<T, E>,
    evidence: Vec<OutcomeEvidence<E>>,
    downstream: DownstreamOutcomeMetadata,
}

impl<T, E> ExecutionOutcome<T, E> {
    /// Build an outcome whose primary is already known.
    #[must_use]
    pub fn new(primary: PrimaryOutcome<T, E>, evidence: Vec<OutcomeEvidence<E>>) -> Self {
        Self {
            primary,
            evidence,
            downstream: DownstreamOutcomeMetadata::foundation(),
        }
    }

    /// The sole primary outcome.
    #[must_use]
    pub const fn primary(&self) -> &PrimaryOutcome<T, E> {
        &self.primary
    }

    /// Secondary evidence in observation/cleanup order.
    #[must_use]
    pub fn evidence(&self) -> &[OutcomeEvidence<E>] {
        &self.evidence
    }

    /// Opaque attachment points reserved for later execution/resource owners.
    #[must_use]
    pub const fn downstream(&self) -> &DownstreamOutcomeMetadata {
        &self.downstream
    }

    /// Consume the record into its primary, ordered evidence, and downstream metadata.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        PrimaryOutcome<T, E>,
        Vec<OutcomeEvidence<E>>,
        DownstreamOutcomeMetadata,
    ) {
        (self.primary, self.evidence, self.downstream)
    }
}

/// Invalid inputs to primary/evidence composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissingPrimary;

impl fmt::Display for MissingPrimary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an execution outcome requires exactly one primary")
    }
}

impl std::error::Error for MissingPrimary {}

/// Compose a possibly absent primary with ordered evidence.
///
/// If cleanup is the sole failure, its first failure becomes the structured
/// language/resource error primary. With any existing primary, every cleanup
/// failure remains secondary and the primary is retained unchanged.
pub fn compose_outcome<T, E>(
    primary: Option<PrimaryOutcome<T, E>>,
    mut evidence: Vec<OutcomeEvidence<E>>,
) -> Result<ExecutionOutcome<T, E>, MissingPrimary> {
    if let Some(primary) = primary {
        return Ok(ExecutionOutcome::new(primary, evidence));
    }

    let Some(index) = evidence
        .iter()
        .position(|item| matches!(item, OutcomeEvidence::CleanupFailure(_)))
    else {
        return Err(MissingPrimary);
    };
    let OutcomeEvidence::CleanupFailure(error) = evidence.remove(index) else {
        unreachable!("the selected evidence item is a cleanup failure")
    };
    Ok(ExecutionOutcome::new(
        PrimaryOutcome::Error(error),
        evidence,
    ))
}
