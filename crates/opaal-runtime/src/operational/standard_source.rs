//! Explicit standard host binding for one source root or interactive submission.

use std::sync::Arc;

use opaal_platform::Platform;

use crate::Value;
use crate::authority::{AuthorityContext, CapabilityRequest, EffectSet};
use crate::context::OperationalContext;
use crate::eval::{CancellationToken, Clock, ResourceBudget};
use crate::lifetime::Deadline;
use crate::module::{ModuleId, ModuleOrigin};

use super::ModuleError;
use super::standard::StandardState;

/// A caller-owned explicit grant and adapter, consumed by one evaluation.
/// Language checkpoints and retained sessions never copy this binding.
pub struct StandardBinding {
    context: OperationalContext,
    state: StandardState,
    pub(crate) cancellation: CancellationToken,
    cleanup_errors: Vec<ModuleError>,
}

impl StandardBinding {
    #[must_use]
    pub fn new(
        authority: AuthorityContext,
        cancellation: CancellationToken,
        clock: Arc<dyn Clock>,
        deadline: Option<Deadline>,
        state: StandardState,
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
        let ModuleOrigin::Standard { namespace, module } = module.origin() else {
            return None;
        };
        if namespace != "std" {
            return None;
        }
        let effects = EffectSet::new([
            CapabilityRequest::entropy_system(),
            CapabilityRequest::stdin_read(),
            CapabilityRequest::stdout_write(),
            CapabilityRequest::stderr_write(),
        ]);
        match module.as_str() {
            "random" => Some(self.state.invoke(
                &self.context,
                &effects,
                platform,
                budget,
                operation,
                arguments,
            )),
            "io" => Some(self.state.invoke_stdio(
                &self.context,
                &effects,
                platform,
                budget,
                operation,
                arguments,
            )),
            _ => None,
        }
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

impl Drop for StandardBinding {
    fn drop(&mut self) {
        let _ = self.state.close();
    }
}

fn cleanup_error() -> ModuleError {
    ModuleError::invalid("OPERATION004", "standard host cleanup failed")
}
