//! Evaluation-bound, cancellable system entropy. Native implementations own
//! their worker until it is terminated and reaped; embeddings opt in explicitly.

use crate::operational::{OperationalError, OperationalErrorKind};

/// Maximum bytes admitted to one maintained entropy fill.
pub const MAX_ENTROPY_FILL_BYTES: usize = 256;

/// Host progress survives a later transport, cancellation or cleanup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillProgress {
    NotStarted,
    Uncertain,
    Confirmed,
}

/// A failed fill retains its settled progress beside the original error.
/// Cleanup failure stays secondary to that error.
#[derive(Debug)]
pub struct FillError {
    pub error: OperationalError,
    pub progress: FillProgress,
    pub cleanup_error: Option<OperationalError>,
}

impl FillError {
    pub fn not_started(error: OperationalError) -> Self {
        Self {
            error,
            progress: FillProgress::NotStarted,
            cleanup_error: None,
        }
    }
    pub fn attempted(error: OperationalError) -> Self {
        Self {
            error,
            progress: FillProgress::Uncertain,
            cleanup_error: None,
        }
    }
    pub fn confirmed(error: OperationalError) -> Self {
        Self {
            error,
            progress: FillProgress::Confirmed,
            cleanup_error: None,
        }
    }
}

/// One explicit host binding. A successful fill acknowledges the entire buffer;
/// failure retains either a confirmed fill or the entire admitted uncertainty.
/// Implementations must not retry failed fills or return with detached work.
pub trait StandardHost: Send {
    fn evaluation(&self) -> u64;
    fn available(&self) -> bool;
    fn fill(
        &mut self,
        destination: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), FillError>;
    fn close(&mut self) -> Result<(), OperationalError>;
}

/// Validate an entropy request before allocation, transport or host work.
pub fn validate_fill(bytes: usize) -> Result<(), OperationalError> {
    if bytes == 0 || bytes > MAX_ENTROPY_FILL_BYTES {
        Err(OperationalError::new(
            OperationalErrorKind::InvalidInput,
            "entropy fill must admit 1 through 256 bytes",
        ))
    } else {
        Ok(())
    }
}
