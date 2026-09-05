//! Opaque metadata seams reserved for later execution and project owners.
//!
//! Flash 2 freezes where authority, resource, action, project, and task
//! metadata attaches without defining those later-owned concepts. The current
//! foundation can represent only an absent or unknown slot; it cannot create
//! an identity, grant authority, schedule a deadline, or claim cleanup.

use std::marker::PhantomData;

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

opaque_later_owned_type!(
    EvaluationContextId,
    "Opaque identity of a later-owned evaluation context."
);
opaque_later_owned_type!(EffectSet, "Opaque later-owned effect declaration.");
opaque_later_owned_type!(
    CapabilityRequest,
    "Opaque later-owned request for explicit capability authority."
);
opaque_later_owned_type!(
    AuthorityVerdict,
    "Opaque later-owned authority decision; this foundation creates no grant."
);
opaque_later_owned_type!(
    ResourceOwnerId,
    "Opaque identity of a later-owned execution resource owner."
);
opaque_later_owned_type!(
    CancellationScopeId,
    "Opaque identity of a later-owned cancellation scope."
);
opaque_later_owned_type!(Deadline, "Opaque later-owned execution deadline.");
opaque_later_owned_type!(
    CleanupOutcome,
    "Opaque later-owned cleanup result attached to a structured outcome."
);
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

/// Later-owned metadata attached to every inspectable call record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownstreamCallMetadata {
    evaluation_context: OpaqueSlot<EvaluationContextId>,
    effects: OpaqueSlot<EffectSet>,
    capability_request: OpaqueSlot<CapabilityRequest>,
    authority_verdict: OpaqueSlot<AuthorityVerdict>,
    resource_owner: OpaqueSlot<ResourceOwnerId>,
    cancellation_scope: OpaqueSlot<CancellationScopeId>,
    deadline: OpaqueSlot<Deadline>,
    action: OpaqueSlot<ActionId>,
    project: OpaqueSlot<ProjectId>,
    task: OpaqueSlot<TaskId>,
    tool: OpaqueSlot<ToolId>,
    environment: OpaqueSlot<EnvironmentId>,
    declared_inputs: OpaqueSlot<DeclaredInputs>,
    declared_outputs: OpaqueSlot<DeclaredOutputs>,
}

impl DownstreamCallMetadata {
    /// The foundation value: every later-owned concept is explicitly absent.
    #[must_use]
    pub const fn foundation() -> Self {
        Self {
            evaluation_context: OpaqueSlot::absent(),
            effects: OpaqueSlot::absent(),
            capability_request: OpaqueSlot::absent(),
            authority_verdict: OpaqueSlot::absent(),
            resource_owner: OpaqueSlot::absent(),
            cancellation_scope: OpaqueSlot::absent(),
            deadline: OpaqueSlot::absent(),
            action: OpaqueSlot::absent(),
            project: OpaqueSlot::absent(),
            task: OpaqueSlot::absent(),
            tool: OpaqueSlot::absent(),
            environment: OpaqueSlot::absent(),
            declared_inputs: OpaqueSlot::absent(),
            declared_outputs: OpaqueSlot::absent(),
        }
    }

    /// Whether every later-owned slot is explicitly absent.
    #[must_use]
    pub const fn is_foundation_only(&self) -> bool {
        matches!(self.evaluation_context.state(), OpaqueSlotState::Absent)
            && matches!(self.effects.state(), OpaqueSlotState::Absent)
            && matches!(self.capability_request.state(), OpaqueSlotState::Absent)
            && matches!(self.authority_verdict.state(), OpaqueSlotState::Absent)
            && matches!(self.resource_owner.state(), OpaqueSlotState::Absent)
            && matches!(self.cancellation_scope.state(), OpaqueSlotState::Absent)
            && matches!(self.deadline.state(), OpaqueSlotState::Absent)
            && matches!(self.action.state(), OpaqueSlotState::Absent)
            && matches!(self.project.state(), OpaqueSlotState::Absent)
            && matches!(self.task.state(), OpaqueSlotState::Absent)
            && matches!(self.tool.state(), OpaqueSlotState::Absent)
            && matches!(self.environment.state(), OpaqueSlotState::Absent)
            && matches!(self.declared_inputs.state(), OpaqueSlotState::Absent)
            && matches!(self.declared_outputs.state(), OpaqueSlotState::Absent)
    }

    /// Opaque evaluation-context slot.
    #[must_use]
    pub const fn evaluation_context(&self) -> &OpaqueSlot<EvaluationContextId> {
        &self.evaluation_context
    }

    /// Opaque effect-declaration slot.
    #[must_use]
    pub const fn effects(&self) -> &OpaqueSlot<EffectSet> {
        &self.effects
    }

    /// Opaque capability-request slot.
    #[must_use]
    pub const fn capability_request(&self) -> &OpaqueSlot<CapabilityRequest> {
        &self.capability_request
    }

    /// Opaque authority-verdict slot.
    #[must_use]
    pub const fn authority_verdict(&self) -> &OpaqueSlot<AuthorityVerdict> {
        &self.authority_verdict
    }

    /// Opaque resource-owner slot.
    #[must_use]
    pub const fn resource_owner(&self) -> &OpaqueSlot<ResourceOwnerId> {
        &self.resource_owner
    }

    /// Opaque cancellation-scope slot.
    #[must_use]
    pub const fn cancellation_scope(&self) -> &OpaqueSlot<CancellationScopeId> {
        &self.cancellation_scope
    }

    /// Opaque deadline slot.
    #[must_use]
    pub const fn deadline(&self) -> &OpaqueSlot<Deadline> {
        &self.deadline
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

/// Later-owned metadata attached to every structured execution outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownstreamOutcomeMetadata {
    evaluation_context: OpaqueSlot<EvaluationContextId>,
    resource_owner: OpaqueSlot<ResourceOwnerId>,
    cancellation_scope: OpaqueSlot<CancellationScopeId>,
    deadline: OpaqueSlot<Deadline>,
    cleanup: OpaqueSlot<CleanupOutcome>,
}

impl DownstreamOutcomeMetadata {
    /// The foundation value: no later owner or cleanup claim is attached.
    #[must_use]
    pub const fn foundation() -> Self {
        Self {
            evaluation_context: OpaqueSlot::absent(),
            resource_owner: OpaqueSlot::absent(),
            cancellation_scope: OpaqueSlot::absent(),
            deadline: OpaqueSlot::absent(),
            cleanup: OpaqueSlot::absent(),
        }
    }

    /// Whether every later-owned outcome slot is explicitly absent.
    #[must_use]
    pub const fn is_foundation_only(&self) -> bool {
        matches!(self.evaluation_context.state(), OpaqueSlotState::Absent)
            && matches!(self.resource_owner.state(), OpaqueSlotState::Absent)
            && matches!(self.cancellation_scope.state(), OpaqueSlotState::Absent)
            && matches!(self.deadline.state(), OpaqueSlotState::Absent)
            && matches!(self.cleanup.state(), OpaqueSlotState::Absent)
    }

    /// Opaque evaluation-context slot.
    #[must_use]
    pub const fn evaluation_context(&self) -> &OpaqueSlot<EvaluationContextId> {
        &self.evaluation_context
    }

    /// Opaque resource-owner slot.
    #[must_use]
    pub const fn resource_owner(&self) -> &OpaqueSlot<ResourceOwnerId> {
        &self.resource_owner
    }

    /// Opaque cancellation-scope slot.
    #[must_use]
    pub const fn cancellation_scope(&self) -> &OpaqueSlot<CancellationScopeId> {
        &self.cancellation_scope
    }

    /// Opaque deadline slot.
    #[must_use]
    pub const fn deadline(&self) -> &OpaqueSlot<Deadline> {
        &self.deadline
    }

    /// Opaque cleanup-outcome slot.
    #[must_use]
    pub const fn cleanup(&self) -> &OpaqueSlot<CleanupOutcome> {
        &self.cleanup
    }
}

impl Default for DownstreamOutcomeMetadata {
    fn default() -> Self {
        Self::foundation()
    }
}
