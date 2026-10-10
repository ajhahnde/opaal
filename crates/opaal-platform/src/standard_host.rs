//! Evaluation-bound, cancellable system entropy. Native implementations own
//! their worker until it is terminated and reaped; embeddings opt in explicitly.

use crate::operational::{OperationalError, OperationalErrorKind};

/// Maximum bytes admitted to one maintained entropy fill.
pub const MAX_ENTROPY_FILL_BYTES: usize = 256;

/// One direct stream syscall and its bounded transport buffer.
pub const MAX_STREAM_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StandardStream {
    Stdin,
    Stdout,
    Stderr,
}

/// One failed syscall/acknowledgement, without any returned partial payload.
/// Counts describe this chunk only and may not exceed its admitted length.
#[derive(Debug)]
pub struct TransferError {
    pub error: OperationalError,
    pub confirmed_bytes: usize,
    pub uncertain_bytes_upper_bound: usize,
    /// EOF was acknowledged by the worker before a later transport failure.
    pub eof: Option<bool>,
    pub cleanup_error: Option<OperationalError>,
}

impl TransferError {
    pub fn not_started(error: OperationalError) -> Self {
        Self {
            error,
            confirmed_bytes: 0,
            uncertain_bytes_upper_bound: 0,
            eof: None,
            cleanup_error: None,
        }
    }
}

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
    /// Availability includes endpoint identity, kind and exclusive ownership.
    fn stream_available(&self, _stream: StandardStream) -> bool {
        false
    }
    /// Perform at most one positive-length read. Zero proves operation-scoped EOF.
    /// Interrupted/nonblocking attempts return zero-progress errors to the
    /// supervisor for fresh work/deadline admission; never retry internally.
    fn read(
        &mut self,
        _destination: &mut [u8],
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<usize, TransferError> {
        Err(TransferError::not_started(OperationalError::new(
            OperationalErrorKind::Unsupported,
            "standard input is unavailable",
        )))
    }
    /// Perform at most one write to the exact bound output endpoint.
    /// Success acknowledges the returned count, including a positive partial write.
    fn write(
        &mut self,
        _stream: StandardStream,
        _bytes: &[u8],
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<usize, TransferError> {
        Err(TransferError::not_started(OperationalError::new(
            OperationalErrorKind::Unsupported,
            "standard output is unavailable",
        )))
    }
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
