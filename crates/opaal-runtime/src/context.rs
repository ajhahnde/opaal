//! The single explicit authority, cancellation, secret, and resource context
//! constructed by an operational embedder.

use std::sync::Arc;

use opaal_platform::Platform;

use crate::authority::{
    AuthorityContext, AuthorityVerdict, CapabilityRequest, EffectSet, EvaluationContextId,
};
use crate::eval::{CancelReason, CancellationToken, Clock};
use crate::lifetime::{
    CancellationScope, CancellationScopeId, Deadline, ResourceId, ResourceOwnerId, ResourceScope,
};
use crate::outcome::{ExecutionOutcome, OutcomeEvidence, PrimaryOutcome};
use crate::seam::{DownstreamCallMetadata, DownstreamOutcomeMetadata};
use crate::security::{Secret, SecretError, SecretId, SecretStore};

/// One explicit fail-closed operational evaluation context.
///
/// Constructing it does not make OPAAL source effectful. An embedder must still
/// present an exact declared request to [`verdict`](Self::verdict), receive an
/// executable verdict, and invoke an adapter it owns.
pub struct OperationalContext {
    authority: AuthorityContext,
    cancellation: CancellationScope,
    resources: ResourceScope,
    secrets: SecretStore,
}

impl OperationalContext {
    /// Construct one context from explicit authority, cancellation, clock, and
    /// deadline inputs.
    #[must_use]
    pub fn new(
        authority: AuthorityContext,
        cancellation: CancellationToken,
        clock: Arc<dyn Clock>,
        deadline: Option<Deadline>,
    ) -> Self {
        let evaluation = authority.evaluation();
        Self {
            authority,
            cancellation: CancellationScope::new(
                CancellationScopeId::new(evaluation),
                cancellation,
                clock,
                deadline,
            ),
            resources: ResourceScope::new(ResourceOwnerId::new(evaluation)),
            secrets: SecretStore::new(),
        }
    }

    /// Construct a deny-by-default context with no deadline or secrets.
    #[must_use]
    pub fn empty(evaluation: EvaluationContextId, clock: Arc<dyn Clock>) -> Self {
        Self::new(
            AuthorityContext::empty(evaluation),
            CancellationToken::never(),
            clock,
            None,
        )
    }

    /// The explicit authority document projection.
    #[must_use]
    pub const fn authority(&self) -> &AuthorityContext {
        &self.authority
    }

    /// The sticky cancellation/deadline scope.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationScope {
        &self.cancellation
    }

    /// Narrow the current deadline without allowing an extension.
    pub fn narrow_deadline(&mut self, deadline: Deadline) {
        self.cancellation.narrow_deadline(deadline);
    }

    /// Poll caller cancellation and the deadline.
    #[must_use]
    pub fn poll_cancellation(&self) -> Option<CancelReason> {
        self.cancellation.poll()
    }

    /// Resolve an exact request only when it is present in the call's declared
    /// effect set. An undeclared request is `unknown` and never executable.
    #[must_use]
    pub fn verdict(
        &self,
        effects: &EffectSet,
        request: &CapabilityRequest,
        platform: &dyn Platform,
    ) -> AuthorityVerdict {
        if effects.contains(request) {
            self.authority.verdict(request, platform)
        } else {
            AuthorityVerdict::Unknown
        }
    }

    /// Build inspectable metadata for one call or adapter boundary.
    #[must_use]
    pub fn call_metadata(
        &self,
        effects: EffectSet,
        request: Option<CapabilityRequest>,
        platform: &dyn Platform,
    ) -> DownstreamCallMetadata {
        let verdict = request
            .as_ref()
            .map(|request| self.verdict(&effects, request, platform));
        DownstreamCallMetadata::operational(
            self.authority.evaluation(),
            effects,
            request,
            verdict,
            self.resources.owner(),
            self.cancellation.id(),
            self.cancellation.deadline(),
        )
    }

    /// Register one adapter-owned cleanup before its handle becomes visible.
    pub fn register_resource(
        &mut self,
        cleanup: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) -> Option<ResourceId> {
        self.resources.register(cleanup)
    }

    /// Add one explicitly injected secret. No environment, credential store,
    /// or other ambient source is consulted.
    pub fn insert_secret(&mut self, secret: Secret) -> Result<(), SecretError> {
        self.secrets.insert(secret)
    }

    /// Whether one explicitly injected secret identity is present.
    #[must_use]
    pub fn contains_secret(&self, id: &SecretId) -> bool {
        self.secrets.contains(id)
    }

    /// Redact raw and common encoded secret representations in bytes.
    #[must_use]
    pub fn redact_bytes(&self, input: &[u8]) -> Vec<u8> {
        self.secrets.redact_bytes(input)
    }

    /// Redact raw and common encoded secret representations in text.
    #[must_use]
    pub fn redact_text(&self, input: &str) -> String {
        self.secrets.redact_text(input)
    }

    /// Finish the evaluation, run LIFO cleanup once, and attach the complete
    /// operational identity and cleanup outcomes without replacing `primary`.
    #[must_use]
    pub fn finish<T, E>(
        mut self,
        primary: PrimaryOutcome<T, E>,
        evidence: Vec<OutcomeEvidence<E>>,
    ) -> ExecutionOutcome<T, E> {
        let cleanup = self.resources.close(&self.secrets);
        let downstream = DownstreamOutcomeMetadata::operational(
            self.authority.evaluation(),
            self.resources.owner(),
            self.cancellation.id(),
            self.cancellation.deadline(),
            cleanup,
        );
        ExecutionOutcome::with_downstream(primary, evidence, downstream)
    }
}
