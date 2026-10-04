//! Explicit entropy binding for one source root or interactive submission.

use std::sync::Arc;

use opaal_platform::Platform;

use crate::Value;
use crate::authority::{AuthorityContext, CapabilityRequest, EffectSet};
use crate::context::OperationalContext;
use crate::eval::{CancellationToken, Clock, ResourceBudget};
use crate::lifetime::Deadline;
use crate::module::{ModuleId, ModuleOrigin};

use super::ModuleError;
use super::random::RandomState;

/// A caller-owned explicit grant and adapter, consumed by one evaluation.
/// Language checkpoints and retained sessions never copy this binding.
pub struct RandomBinding {
    context: OperationalContext,
    state: RandomState,
    pub(crate) cancellation: CancellationToken,
    cleanup_errors: Vec<ModuleError>,
}

impl RandomBinding {
    #[must_use]
    pub fn new(
        authority: AuthorityContext,
        cancellation: CancellationToken,
        clock: Arc<dyn Clock>,
        deadline: Option<Deadline>,
        state: RandomState,
    ) -> Self {
        Self {
            context: OperationalContext::new(authority, cancellation.clone(), clock, deadline),
            state,
            cancellation,
            cleanup_errors: Vec::new(),
        }
    }

    pub(crate) fn invoke(
        &mut self,
        module: &ModuleId,
        operation: &str,
        arguments: &[Value],
        budget: &mut ResourceBudget,
        platform: &dyn Platform,
    ) -> Option<Result<Value, ModuleError>> {
        if !matches!(module.origin(), ModuleOrigin::Standard { namespace, module }
            if namespace == "std" && module == "random")
        {
            return None;
        }
        Some(self.state.invoke(
            &self.context,
            &EffectSet::new([CapabilityRequest::entropy_system()]),
            platform,
            budget,
            operation,
            arguments,
        ))
    }

    /// Return ordered fixed cleanup diagnostics beside the primary result.
    pub fn take_cleanup_errors(&mut self) -> Vec<ModuleError> {
        std::mem::take(&mut self.cleanup_errors)
    }

    pub(crate) fn finish(&mut self) {
        if self.state.take_cleanup_error().is_some() {
            self.cleanup_errors.push(cleanup_error());
        }
        if self.state.close().is_err() {
            self.cleanup_errors.push(cleanup_error());
        }
    }
}

impl Drop for RandomBinding {
    fn drop(&mut self) {
        let _ = self.state.close();
    }
}

fn cleanup_error() -> ModuleError {
    ModuleError::invalid("OPERATION004", "standard host cleanup failed")
}
