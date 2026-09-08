//! Authority-gated bounded file operations.

use std::path::Path;

use opaal_platform::Platform;
use opaal_platform::operational::{AtomicWriteRequest, OperationalAdapter, ReadFileRequest};

use crate::authority::{CapabilityRequest, EffectSet};
use crate::context::OperationalContext;

use super::{MAX_FILE_BYTES, MAX_TOTAL_READ_BYTES, ModuleError, adapter_error, authorize, path};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReadBudget {
    used: usize,
}

impl ReadBudget {
    #[must_use]
    pub const fn used(&self) -> usize {
        self.used
    }

    fn admit(&mut self, bytes: usize) -> Result<(), ModuleError> {
        let used = self
            .used
            .checked_add(bytes)
            .ok_or_else(|| ModuleError::invalid("FS001", "aggregate read size overflow"))?;
        if used > MAX_TOTAL_READ_BYTES {
            return Err(ModuleError::invalid(
                "FS002",
                "aggregate reads exceed 32 MiB",
            ));
        }
        self.used = used;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn read(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
    authority_scope: &Path,
    root: &Path,
    target: &Path,
    max_bytes: usize,
    budget: &mut ReadBudget,
) -> Result<Vec<u8>, ModuleError> {
    if max_bytes > MAX_FILE_BYTES {
        return Err(ModuleError::invalid(
            "FS003",
            "one file read may request at most 16 MiB",
        ));
    }
    let root = path::normalize(root)?;
    let target = path::contained(&root, target)?;
    if path::normalize(authority_scope)? != root {
        return Err(ModuleError::invalid(
            "FS004",
            "filesystem read authority must name the exact containment root",
        ));
    }
    let request = CapabilityRequest::filesystem_read(root.clone())
        .map_err(|error| ModuleError::invalid("FS004", error.to_string()))?;
    authorize(context, effects, &request, platform)?;
    let remaining = MAX_TOTAL_READ_BYTES.saturating_sub(budget.used);
    let bytes = adapter
        .read_file(ReadFileRequest {
            root: &root,
            path: &target,
            max_bytes: max_bytes.min(remaining),
        })
        .map_err(|error| adapter_error(context, error))?;
    if let Some(reason) = context.poll_cancellation() {
        return Err(ModuleError::Cancelled(reason));
    }
    budget.admit(bytes.len())?;
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
pub fn write_atomic(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
    authority_scope: &Path,
    root: &Path,
    target: &Path,
    bytes: &[u8],
) -> Result<(), ModuleError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ModuleError::invalid(
            "FS005",
            "one atomic write exceeds 16 MiB",
        ));
    }
    let root = path::normalize(root)?;
    let target = path::contained(&root, target)?;
    if path::normalize(authority_scope)? != target {
        return Err(ModuleError::invalid(
            "FS006",
            "filesystem write authority must name the exact evidence target",
        ));
    }
    let request = CapabilityRequest::filesystem_write(target.clone())
        .map_err(|error| ModuleError::invalid("FS006", error.to_string()))?;
    authorize(context, effects, &request, platform)?;
    adapter
        .write_atomic(AtomicWriteRequest {
            root: &root,
            path: &target,
            bytes,
            max_bytes: MAX_FILE_BYTES,
        })
        .map_err(|error| adapter_error(context, error))?;
    if let Some(reason) = context.poll_cancellation() {
        return Err(ModuleError::Cancelled(reason));
    }
    Ok(())
}
