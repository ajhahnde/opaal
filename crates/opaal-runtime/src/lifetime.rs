//! Deadline, cancellation, and adapter-owned resource lifetime contracts.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::authority::EvaluationContextId;
use crate::eval::{CancelReason, CancellationToken, Clock, Instant};
use crate::security::SecretStore;

/// The owned-resource identity for one evaluation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourceOwnerId(EvaluationContextId);

impl ResourceOwnerId {
    /// Build the sole resource owner for `evaluation`.
    #[must_use]
    pub const fn new(evaluation: EvaluationContextId) -> Self {
        Self(evaluation)
    }

    /// The evaluation that owns every registered resource.
    #[must_use]
    pub const fn evaluation(self) -> EvaluationContextId {
        self.0
    }
}

/// One cancellation scope owned by an evaluation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CancellationScopeId(EvaluationContextId);

impl CancellationScopeId {
    /// Build the sole cancellation scope for `evaluation`.
    #[must_use]
    pub const fn new(evaluation: EvaluationContextId) -> Self {
        Self(evaluation)
    }

    /// The evaluation that owns this scope.
    #[must_use]
    pub const fn evaluation(self) -> EvaluationContextId {
        self.0
    }
}

/// An absolute monotonic deadline.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Deadline(Instant);

impl Deadline {
    /// Build a deadline at one monotonic instant.
    #[must_use]
    pub const fn at(instant: Instant) -> Self {
        Self(instant)
    }

    /// The absolute instant on the originating clock.
    #[must_use]
    pub const fn instant(self) -> Instant {
        self.0
    }

    /// Return the earlier of this deadline and `other`.
    #[must_use]
    pub const fn narrowed_to(self, other: Self) -> Self {
        if self.0.as_nanos() <= other.0.as_nanos() {
            self
        } else {
            other
        }
    }
}

/// A sticky cancellation scope combining caller cancellation and a deadline.
pub struct CancellationScope {
    id: CancellationScopeId,
    token: CancellationToken,
    clock: Arc<dyn Clock>,
    deadline: Option<Deadline>,
    state: AtomicU8,
}

impl CancellationScope {
    /// Construct one evaluation-owned cancellation scope.
    #[must_use]
    pub fn new(
        id: CancellationScopeId,
        token: CancellationToken,
        clock: Arc<dyn Clock>,
        deadline: Option<Deadline>,
    ) -> Self {
        Self {
            id,
            token,
            clock,
            deadline,
            state: AtomicU8::new(0),
        }
    }

    /// This scope's stable identity.
    #[must_use]
    pub const fn id(&self) -> CancellationScopeId {
        self.id
    }

    /// The optional absolute deadline.
    #[must_use]
    pub const fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// Narrow the deadline. A child or later phase can never extend it.
    pub fn narrow_deadline(&mut self, deadline: Deadline) {
        self.deadline = Some(match self.deadline {
            Some(current) => current.narrowed_to(deadline),
            None => deadline,
        });
    }

    /// Poll caller cancellation first, then the monotonic deadline.
    ///
    /// Once observed, the result is sticky even if a test predicate later
    /// changes.
    #[must_use]
    pub fn poll(&self) -> Option<CancelReason> {
        match self.state.load(Ordering::Acquire) {
            1 => return Some(CancelReason::Requested),
            2 => return Some(CancelReason::Timeout),
            _ => {}
        }

        let reason = if self.token.is_cancelled() {
            Some(self.token.reason())
        } else if self
            .deadline
            .is_some_and(|deadline| self.clock.now() >= deadline.instant())
        {
            Some(CancelReason::Timeout)
        } else {
            None
        };
        if let Some(reason) = reason {
            let value = match reason {
                CancelReason::Requested => 1,
                CancelReason::Timeout => 2,
            };
            let _ = self
                .state
                .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire);
            return match self.state.load(Ordering::Acquire) {
                2 => Some(CancelReason::Timeout),
                _ => Some(CancelReason::Requested),
            };
        }
        None
    }
}

impl fmt::Debug for CancellationScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let observed = match self.state.load(Ordering::Acquire) {
            1 => Some(CancelReason::Requested),
            2 => Some(CancelReason::Timeout),
            _ => None,
        };
        formatter
            .debug_struct("CancellationScope")
            .field("id", &self.id)
            .field("deadline", &self.deadline)
            .field("observed", &observed)
            .finish()
    }
}

/// One resource registered beneath an evaluation owner.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourceId {
    owner: ResourceOwnerId,
    ordinal: u32,
}

impl ResourceId {
    /// The evaluation owner.
    #[must_use]
    pub const fn owner(self) -> ResourceOwnerId {
        self.owner
    }

    /// The source-order registration ordinal.
    #[must_use]
    pub const fn ordinal(self) -> u32 {
        self.ordinal
    }
}

/// The result of cleaning one resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CleanupStatus {
    /// Cleanup completed.
    Succeeded,
    /// Cleanup failed; the retained message has already been redacted.
    Failed(String),
}

/// One ordered resource cleanup result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CleanupOutcome {
    resource: ResourceId,
    status: CleanupStatus,
}

impl CleanupOutcome {
    /// The resource that was cleaned.
    #[must_use]
    pub const fn resource(&self) -> ResourceId {
        self.resource
    }

    /// Success or a redacted failure message.
    #[must_use]
    pub const fn status(&self) -> &CleanupStatus {
        &self.status
    }
}

type Cleanup = Box<dyn FnOnce() -> Result<(), String> + Send + 'static>;

struct RegisteredResource {
    id: ResourceId,
    cleanup: Option<Cleanup>,
}

/// One LIFO, exactly-once cleanup stack for adapter-owned resources.
pub struct ResourceScope {
    owner: ResourceOwnerId,
    next_ordinal: u32,
    resources: Vec<RegisteredResource>,
    closed: bool,
}

impl ResourceScope {
    /// Build an empty scope for one evaluation owner.
    #[must_use]
    pub const fn new(owner: ResourceOwnerId) -> Self {
        Self {
            owner,
            next_ordinal: 0,
            resources: Vec::new(),
            closed: false,
        }
    }

    /// The owner attached to every registration.
    #[must_use]
    pub const fn owner(&self) -> ResourceOwnerId {
        self.owner
    }

    /// Register cleanup before an adapter makes its handle externally visible.
    ///
    /// Registration after cleanup has begun is refused by returning `None`.
    pub fn register(
        &mut self,
        cleanup: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) -> Option<ResourceId> {
        if self.closed {
            return None;
        }
        let id = ResourceId {
            owner: self.owner,
            ordinal: self.next_ordinal,
        };
        self.next_ordinal = self.next_ordinal.checked_add(1)?;
        self.resources.push(RegisteredResource {
            id,
            cleanup: Some(Box::new(cleanup)),
        });
        Some(id)
    }

    /// Clean every registered resource once in reverse registration order.
    /// Cleanup continues after failures.
    pub fn close(&mut self, secrets: &SecretStore) -> Vec<CleanupOutcome> {
        if self.closed {
            return Vec::new();
        }
        self.closed = true;
        let mut outcomes = Vec::with_capacity(self.resources.len());
        while let Some(mut resource) = self.resources.pop() {
            let result = resource
                .cleanup
                .take()
                .expect("a registered cleanup is consumed exactly once")();
            outcomes.push(CleanupOutcome {
                resource: resource.id,
                status: match result {
                    Ok(()) => CleanupStatus::Succeeded,
                    Err(message) => CleanupStatus::Failed(secrets.redact_text(&message)),
                },
            });
        }
        outcomes
    }

    /// Whether cleanup has already run.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }
}

impl Drop for ResourceScope {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        while let Some(mut resource) = self.resources.pop() {
            if let Some(cleanup) = resource.cleanup.take() {
                let _ = cleanup();
            }
        }
    }
}
