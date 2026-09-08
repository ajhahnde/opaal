//! Public metadata shared by pure call descriptors and embedding outcomes.
//!
//! Authority, cancellation, deadlines, secrets, and resource cleanup are
//! concrete embedding contracts. Action, project, task, tool, and environment
//! identities remain deliberately opaque until their source-facing owner is
//! delivered.

use std::marker::PhantomData;

pub use crate::authority::{AuthorityVerdict, CapabilityRequest, EffectSet, EvaluationContextId};
pub use crate::lifetime::{CancellationScopeId, CleanupOutcome, Deadline, ResourceOwnerId};

/// The observable state of one later-owned metadata slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum OpaqueSlotState {
    /// The current call or outcome has no value for this slot.
    Absent,
    /// A later owner may supply the value, but it is not known at this boundary.
    Unknown,
}

/// One typed slot whose value remains owned by a later architecture layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpaqueSlot<T> {
    state: OpaqueSlotState,
    marker: PhantomData<fn() -> T>,
}

impl<T> OpaqueSlot<T> {
    /// Build a slot that is explicitly absent.
    #[must_use]
    pub const fn absent() -> Self {
        Self {
            state: OpaqueSlotState::Absent,
            marker: PhantomData,
        }
    }

    /// Build a slot whose later-owned value is not known at this boundary.
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            state: OpaqueSlotState::Unknown,
            marker: PhantomData,
        }
    }

    /// Report whether the slot is absent or unknown.
    #[must_use]
    pub const fn state(&self) -> OpaqueSlotState {
        self.state
    }
}

impl<T> Default for OpaqueSlot<T> {
    fn default() -> Self {
        Self::absent()
    }
}

macro_rules! opaque_later_owned_type {
    ($name:ident, $summary:literal) => {
        #[doc = $summary]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name {
            private: (),
        }
    };
}

opaque_later_owned_type!(ActionId, "Opaque identity of a future typed action.");
opaque_later_owned_type!(ProjectId, "Opaque identity of a future project.");
opaque_later_owned_type!(TaskId, "Opaque identity of a future exported task.");
opaque_later_owned_type!(ToolId, "Opaque identity of a future declared tool.");
opaque_later_owned_type!(
    EnvironmentId,
    "Opaque identity of a future declared execution environment."
);
opaque_later_owned_type!(
    DeclaredInputs,
    "Opaque future declaration of a callable's external inputs."
);
opaque_later_owned_type!(
    DeclaredOutputs,
    "Opaque future declaration of a callable's external outputs."
);

/// Metadata attached to every inspectable call record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownstreamCallMetadata {
    evaluation_context: Option<EvaluationContextId>,
    effects: EffectSet,
    capability_request: Option<CapabilityRequest>,
    authority_verdict: Option<AuthorityVerdict>,
    resource_owner: Option<ResourceOwnerId>,
    cancellation_scope: Option<CancellationScopeId>,
    deadline: Option<Deadline>,
    action: OpaqueSlot<ActionId>,
    project: OpaqueSlot<ProjectId>,
    task: OpaqueSlot<TaskId>,
    tool: OpaqueSlot<ToolId>,
    environment: OpaqueSlot<EnvironmentId>,
    declared_inputs: OpaqueSlot<DeclaredInputs>,
    declared_outputs: OpaqueSlot<DeclaredOutputs>,
}

impl DownstreamCallMetadata {
    /// The pure-source value: no authority, resource, or later-owned identity.
    #[must_use]
    pub fn foundation() -> Self {
        Self {
            evaluation_context: None,
            effects: EffectSet::default(),
            capability_request: None,
            authority_verdict: None,
            resource_owner: None,
            cancellation_scope: None,
            deadline: None,
            action: OpaqueSlot::absent(),
            project: OpaqueSlot::absent(),
            task: OpaqueSlot::absent(),
            tool: OpaqueSlot::absent(),
            environment: OpaqueSlot::absent(),
            declared_inputs: OpaqueSlot::absent(),
            declared_outputs: OpaqueSlot::absent(),
        }
    }

    pub(crate) fn operational(
        evaluation_context: EvaluationContextId,
        effects: EffectSet,
        capability_request: Option<CapabilityRequest>,
        authority_verdict: Option<AuthorityVerdict>,
        resource_owner: ResourceOwnerId,
        cancellation_scope: CancellationScopeId,
        deadline: Option<Deadline>,
    ) -> Self {
        Self {
            evaluation_context: Some(evaluation_context),
            effects,
            capability_request,
            authority_verdict,
            resource_owner: Some(resource_owner),
            cancellation_scope: Some(cancellation_scope),
            deadline,
            action: OpaqueSlot::absent(),
            project: OpaqueSlot::absent(),
            task: OpaqueSlot::absent(),
            tool: OpaqueSlot::absent(),
            environment: OpaqueSlot::absent(),
            declared_inputs: OpaqueSlot::absent(),
            declared_outputs: OpaqueSlot::absent(),
        }
    }

    /// Whether this is the empty pure-source value.
    #[must_use]
    pub fn is_foundation_only(&self) -> bool {
        self.evaluation_context.is_none()
            && self.effects.is_empty()
            && self.capability_request.is_none()
            && self.authority_verdict.is_none()
            && self.resource_owner.is_none()
            && self.cancellation_scope.is_none()
            && self.deadline.is_none()
            && matches!(self.action.state(), OpaqueSlotState::Absent)
            && matches!(self.project.state(), OpaqueSlotState::Absent)
            && matches!(self.task.state(), OpaqueSlotState::Absent)
            && matches!(self.tool.state(), OpaqueSlotState::Absent)
            && matches!(self.environment.state(), OpaqueSlotState::Absent)
            && matches!(self.declared_inputs.state(), OpaqueSlotState::Absent)
            && matches!(self.declared_outputs.state(), OpaqueSlotState::Absent)
    }

    /// Explicit evaluation identity, absent for pure-source descriptors.
    #[must_use]
    pub const fn evaluation_context(&self) -> Option<EvaluationContextId> {
        self.evaluation_context
    }

    /// Canonical declared request set.
    #[must_use]
    pub const fn effects(&self) -> &EffectSet {
        &self.effects
    }

    /// The exact current adapter request, when one is being inspected.
    #[must_use]
    pub const fn capability_request(&self) -> Option<&CapabilityRequest> {
        self.capability_request.as_ref()
    }

    /// The exact verdict for the current request.
    #[must_use]
    pub const fn authority_verdict(&self) -> Option<AuthorityVerdict> {
        self.authority_verdict
    }

    /// Explicit resource owner, absent for pure-source descriptors.
    #[must_use]
    pub const fn resource_owner(&self) -> Option<ResourceOwnerId> {
        self.resource_owner
    }

    /// Explicit cancellation scope, absent for pure-source descriptors.
    #[must_use]
    pub const fn cancellation_scope(&self) -> Option<CancellationScopeId> {
        self.cancellation_scope
    }

    /// The current deadline, when present.
    #[must_use]
    pub const fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// Opaque future action slot.
    #[must_use]
    pub const fn action(&self) -> &OpaqueSlot<ActionId> {
        &self.action
    }

    /// Opaque future project slot.
    #[must_use]
    pub const fn project(&self) -> &OpaqueSlot<ProjectId> {
        &self.project
    }

    /// Opaque future task slot.
    #[must_use]
    pub const fn task(&self) -> &OpaqueSlot<TaskId> {
        &self.task
    }

    /// Opaque future tool slot.
    #[must_use]
    pub const fn tool(&self) -> &OpaqueSlot<ToolId> {
        &self.tool
    }

    /// Opaque future execution-environment slot.
    #[must_use]
    pub const fn environment(&self) -> &OpaqueSlot<EnvironmentId> {
        &self.environment
    }

    /// Opaque future declared-input slot.
    #[must_use]
    pub const fn declared_inputs(&self) -> &OpaqueSlot<DeclaredInputs> {
        &self.declared_inputs
    }

    /// Opaque future declared-output slot.
    #[must_use]
    pub const fn declared_outputs(&self) -> &OpaqueSlot<DeclaredOutputs> {
        &self.declared_outputs
    }
}

impl Default for DownstreamCallMetadata {
    fn default() -> Self {
        Self::foundation()
    }
}

/// Metadata attached to every structured execution outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownstreamOutcomeMetadata {
    evaluation_context: Option<EvaluationContextId>,
    resource_owner: Option<ResourceOwnerId>,
    cancellation_scope: Option<CancellationScopeId>,
    deadline: Option<Deadline>,
    cleanup: Vec<CleanupOutcome>,
}

impl DownstreamOutcomeMetadata {
    /// The pure-source value: no operational identity or cleanup claim.
    #[must_use]
    pub fn foundation() -> Self {
        Self {
            evaluation_context: None,
            resource_owner: None,
            cancellation_scope: None,
            deadline: None,
            cleanup: Vec::new(),
        }
    }

    pub(crate) fn operational(
        evaluation_context: EvaluationContextId,
        resource_owner: ResourceOwnerId,
        cancellation_scope: CancellationScopeId,
        deadline: Option<Deadline>,
        cleanup: Vec<CleanupOutcome>,
    ) -> Self {
        Self {
            evaluation_context: Some(evaluation_context),
            resource_owner: Some(resource_owner),
            cancellation_scope: Some(cancellation_scope),
            deadline,
            cleanup,
        }
    }

    /// Whether this is the empty pure-source value.
    #[must_use]
    pub fn is_foundation_only(&self) -> bool {
        self.evaluation_context.is_none()
            && self.resource_owner.is_none()
            && self.cancellation_scope.is_none()
            && self.deadline.is_none()
            && self.cleanup.is_empty()
    }

    /// Explicit evaluation identity, absent for pure-source outcomes.
    #[must_use]
    pub const fn evaluation_context(&self) -> Option<EvaluationContextId> {
        self.evaluation_context
    }

    /// Explicit resource owner, absent for pure-source outcomes.
    #[must_use]
    pub const fn resource_owner(&self) -> Option<ResourceOwnerId> {
        self.resource_owner
    }

    /// Explicit cancellation scope, absent for pure-source outcomes.
    #[must_use]
    pub const fn cancellation_scope(&self) -> Option<CancellationScopeId> {
        self.cancellation_scope
    }

    /// The effective deadline, when present.
    #[must_use]
    pub const fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// LIFO cleanup results in observation order.
    #[must_use]
    pub fn cleanup(&self) -> &[CleanupOutcome] {
        &self.cleanup
    }
}

impl Default for DownstreamOutcomeMetadata {
    fn default() -> Self {
        Self::foundation()
    }
}
