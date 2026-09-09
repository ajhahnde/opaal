//! Maintained, bounded operational modules.
//!
//! Pure data, path, version, URL, and integrity operations are immediately
//! usable by embedders. File, clock, HTTP, secret, and process operations also
//! require an explicit [`crate::context::OperationalContext`], an exact effect
//! request, and a maintained adapter verdict. This module deliberately exposes
//! no route from effectful OPAAL source; controlled invocation belongs to the
//! later plan/execution lifecycle.

use std::fmt;

use opaal_platform::Platform;
use opaal_platform::operational::{OperationalError, OperationalErrorKind};

use crate::authority::{AuthorityVerdict, CapabilityRequest, EffectSet};
use crate::context::OperationalContext;
use crate::eval::CancelReason;

pub mod data;
pub mod filesystem;
pub mod http;
pub mod integrity;
pub mod path;
pub mod process;
pub mod source;
pub mod time;
pub mod url;
pub mod version;

pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_TOTAL_READ_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_HTTP_BODY_BYTES: usize = opaal_platform::operational::MAX_HTTP_BODY_BYTES;
pub const MAX_PROCESS_OUTPUT_BYTES: usize = opaal_platform::operational::MAX_PROCESS_OUTPUT_BYTES;

/// The maintained standard-module catalog implemented by this adapter slice.
///
/// The names are stable semantic identities for inspection and later
/// controlled execution. They are not ambient globals and do not make the
/// current source evaluator effectful.
pub const STANDARD_MODULES: [&str; 9] = [
    "std::data",
    "std::path",
    "std::filesystem",
    "std::time",
    "std::version",
    "std::integrity",
    "std::url",
    "std::http",
    "std::process",
];

pub(crate) fn is_source_module(module: &str) -> bool {
    STANDARD_MODULES
        .iter()
        .any(|candidate| candidate.strip_prefix("std::") == Some(module))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleError {
    Invalid { code: &'static str, message: String },
    Authority { verdict: AuthorityVerdict },
    Cancelled(CancelReason),
    Adapter(OperationalError),
}

impl ModuleError {
    pub fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { code, .. } => code,
            Self::Authority { .. } => "OPERATION002",
            Self::Cancelled(_) => "OPERATION003",
            Self::Adapter(_) => "OPERATION004",
        }
    }
}

impl fmt::Display for ModuleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
            Self::Authority { verdict } => {
                write!(formatter, "operational authority is {verdict:?}")
            }
            Self::Cancelled(reason) => write!(formatter, "operation was cancelled: {reason:?}"),
            Self::Adapter(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ModuleError {}

impl From<OperationalError> for ModuleError {
    fn from(error: OperationalError) -> Self {
        match error.kind() {
            OperationalErrorKind::Cancelled => Self::Cancelled(CancelReason::Requested),
            OperationalErrorKind::TimedOut => Self::Cancelled(CancelReason::Timeout),
            _ => Self::Adapter(error),
        }
    }
}

pub(crate) fn adapter_error(context: &OperationalContext, error: OperationalError) -> ModuleError {
    let error = OperationalError::new(error.kind(), context.redact_text(error.message()));
    match error.kind() {
        OperationalErrorKind::Cancelled => context
            .poll_cancellation()
            .map_or_else(|| ModuleError::Adapter(error), ModuleError::Cancelled),
        OperationalErrorKind::TimedOut => ModuleError::Cancelled(CancelReason::Timeout),
        _ => ModuleError::Adapter(error),
    }
}

pub(crate) fn authorize(
    context: &OperationalContext,
    effects: &EffectSet,
    request: &CapabilityRequest,
    platform: &dyn Platform,
) -> Result<(), ModuleError> {
    if let Some(reason) = context.poll_cancellation() {
        return Err(ModuleError::Cancelled(reason));
    }
    let verdict = context.verdict(effects, request, platform);
    if verdict.is_executable() {
        Ok(())
    } else {
        Err(ModuleError::Authority { verdict })
    }
}
