//! Bounded system sampling under explicit evaluation authority.

use std::time::Duration;

use opaal_platform::Platform;
use opaal_platform::standard_host::{FillProgress, MAX_ENTROPY_FILL_BYTES, StandardHost};

use crate::Value;
use crate::authority::{AuthorityVerdict, CapabilityRequest, EffectSet};
use crate::context::OperationalContext;
use crate::eval::ResourceBudget;
use crate::lifetime::Deadline;
use crate::value::FiniteFloat;

use super::{ModuleError, adapter_error, authorize};

pub const MAX_CALL_BYTES: usize = 1024 * 1024;
pub const MAX_HOST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_INTEGER_CANDIDATES: usize = 128;
pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Only lower limits may be injected; production ceilings cannot be raised.
#[derive(Clone, Copy, Debug)]
pub struct RandomLimits {
    pub max_call_bytes: usize,
    pub max_host_bytes: usize,
    pub max_integer_candidates: usize,
    pub operation_timeout: Duration,
}

impl Default for RandomLimits {
    fn default() -> Self {
        Self {
            max_call_bytes: MAX_CALL_BYTES,
            max_host_bytes: MAX_HOST_BYTES,
            max_integer_candidates: MAX_INTEGER_CANDIDATES,
            operation_timeout: OPERATION_TIMEOUT,
        }
    }
}

/// Counts describe admitted fills, never internal library syscalls or payloads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EntropyProgress {
    pub requested_bytes: usize,
    pub admitted_bytes: usize,
    pub confirmed_bytes: usize,
    pub uncertain_bytes_upper_bound: usize,
}

/// One serialized host and cumulative byte counter for a logical evaluation.
/// Catchable language checkpoints must not clone or restore this state.
pub struct RandomState {
    host: Option<Box<dyn StandardHost>>,
    limits: RandomLimits,
    consumed_bytes: usize,
    progress: EntropyProgress,
    cleanup_error: Option<opaal_platform::operational::OperationalError>,
}

impl RandomState {
    pub fn new(
        host: Option<Box<dyn StandardHost>>,
        limits: RandomLimits,
    ) -> Result<Self, ModuleError> {
        if limits.max_call_bytes > MAX_CALL_BYTES
            || limits.max_host_bytes > MAX_HOST_BYTES
            || limits.max_integer_candidates > MAX_INTEGER_CANDIDATES
            || limits.operation_timeout.is_zero()
            || limits.operation_timeout > OPERATION_TIMEOUT
        {
            return Err(ModuleError::invalid(
                "OPERATION001",
                "invalid entropy limits",
            ));
        }
        Ok(Self {
            host,
            limits,
            consumed_bytes: 0,
            progress: EntropyProgress::default(),
            cleanup_error: None,
        })
    }

    #[must_use]
    pub const fn progress(&self) -> EntropyProgress {
        self.progress
    }

    #[must_use]
    pub const fn consumed_bytes(&self) -> usize {
        self.consumed_bytes
    }

    /// Validate and reserve a call without starting entropy work.
    pub fn admission(
        &self,
        context: &OperationalContext,
        effects: &EffectSet,
        platform: &dyn Platform,
        operation: &str,
        arguments: &[Value],
    ) -> Result<EntropyProgress, ModuleError> {
        if let Some(reason) = context.poll_cancellation() {
            return Err(ModuleError::Cancelled(reason));
        }
        let call = RandomCall::parse(operation, arguments, self.limits.max_call_bytes)?;
        authorize(
            context,
            effects,
            &CapabilityRequest::entropy_system(),
            platform,
        )?;
        let host =
            self.host
                .as_ref()
                .filter(|host| host.available())
                .ok_or(ModuleError::Authority {
                    verdict: AuthorityVerdict::Unsupported,
                })?;
        if host.evaluation() != context.authority().evaluation().get() {
            return Err(ModuleError::invalid(
                "EXECUTE_STALE",
                "entropy host belongs to another evaluation",
            ));
        }
        let requested_bytes = call.requested_bytes(self.limits.max_integer_candidates);
        let remaining = self
            .limits
            .max_host_bytes
            .saturating_sub(self.consumed_bytes);
        let admitted_bytes = match call {
            RandomCall::Int { width, .. } if width > 1 => requested_bytes.min(remaining / 8 * 8),
            _ if requested_bytes <= remaining => requested_bytes,
            _ => return Err(limit()),
        };
        if requested_bytes > 0 && admitted_bytes == 0 {
            return Err(limit());
        }
        Ok(EntropyProgress {
            requested_bytes,
            admitted_bytes,
            ..EntropyProgress::default()
        })
    }

    /// Drain one fixed cleanup diagnostic beside the original operation result.
    pub fn take_cleanup_error(&mut self) -> Option<opaal_platform::operational::OperationalError> {
        self.cleanup_error.take()
    }

    pub fn close(&mut self) -> Result<(), ModuleError> {
        if let Some(mut host) = self.host.take() {
            host.close()?;
        }
        Ok(())
    }

    /// Parse domains before touching the host, then authorize even no-draw calls.
    pub fn invoke(
        &mut self,
        context: &OperationalContext,
        effects: &EffectSet,
        platform: &dyn Platform,
        budget: &mut ResourceBudget,
        operation: &str,
        arguments: &[Value],
    ) -> Result<Value, ModuleError> {
        let deadline = self.operation_deadline(context);
        self.invoke_with_deadline(
            context, effects, platform, budget, operation, arguments, deadline,
        )
    }

    pub(crate) fn operation_deadline(&self, context: &OperationalContext) -> Deadline {
        context
            .cancellation()
            .operation_deadline(self.limits.operation_timeout)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn invoke_with_deadline(
        &mut self,
        context: &OperationalContext,
        effects: &EffectSet,
        platform: &dyn Platform,
        budget: &mut ResourceBudget,
        operation: &str,
        arguments: &[Value],
        deadline: Deadline,
    ) -> Result<Value, ModuleError> {
        let result = self.invoke_call(
            context, effects, platform, budget, operation, arguments, deadline,
        );
        // Release unused candidate reservations; evidence counts actual fills.
        self.progress.admitted_bytes =
            self.progress.confirmed_bytes + self.progress.uncertain_bytes_upper_bound;
        if matches!(result, Err(ModuleError::Cancelled(_)))
            && let Some(mut host) = self.host.take()
            && let Err(error) = host.close()
            && self.cleanup_error.is_none()
        {
            self.cleanup_error = Some(opaal_platform::operational::OperationalError::new(
                error.kind(),
                context.redact_text(error.message()),
            ));
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn invoke_call(
        &mut self,
        context: &OperationalContext,
        effects: &EffectSet,
        platform: &dyn Platform,
        budget: &mut ResourceBudget,
        operation: &str,
        arguments: &[Value],
        deadline: Deadline,
    ) -> Result<Value, ModuleError> {
        self.progress = EntropyProgress::default();
        self.progress = self.admission(context, effects, platform, operation, arguments)?;
        let call = RandomCall::parse(operation, arguments, self.limits.max_call_bytes)?;
        poll(context, deadline)?;
        charge(budget)?;
        if let RandomCall::Bytes(count) = call
            && !budget.charge_collection_bytes(count)
        {
            return Err(limit());
        }
        match call {
            RandomCall::Int { min, width: 1 } => Ok(Value::Int(min)),
            RandomCall::Int { min, width } => {
                let threshold = (1_u128 << 64) % width;
                for _ in 0..self.limits.max_integer_candidates {
                    poll(context, deadline)?;
                    charge(budget)?;
                    let mut word = [0; 8];
                    self.fill(context, budget, deadline, &mut word)?;
                    let word = u128::from(u64::from_be_bytes(word));
                    if word >= threshold {
                        let value = i128::from(min) + (word % width) as i128;
                        return Ok(Value::Int(
                            i64::try_from(value).expect("sample remains inside its i64 interval"),
                        ));
                    }
                }
                Err(limit())
            }
            RandomCall::Float => {
                let mut word = [0; 8];
                self.fill(context, budget, deadline, &mut word)?;
                let value = (u64::from_be_bytes(word) >> 11) as f64 / ((1_u64 << 53) as f64);
                Ok(Value::Float(
                    FiniteFloat::new(value).expect("53-bit fraction is finite"),
                ))
            }
            RandomCall::Bytes(count) => {
                let mut bytes = vec![0; count];
                for chunk in bytes.chunks_mut(MAX_ENTROPY_FILL_BYTES) {
                    if let Err(error) = self.fill(context, budget, deadline, chunk) {
                        bytes.fill(0);
                        return Err(error);
                    }
                }
                Ok(Value::bytes(bytes))
            }
        }
    }

    fn fill(
        &mut self,
        context: &OperationalContext,
        budget: &mut ResourceBudget,
        deadline: Deadline,
        bytes: &mut [u8],
    ) -> Result<(), ModuleError> {
        poll(context, deadline)?;
        if bytes.len()
            > self
                .progress
                .admitted_bytes
                .saturating_sub(self.progress.confirmed_bytes)
        {
            return Err(limit());
        }
        // One fill and one started transport chunk; result copy is charged
        // before acknowledgement. Neither work nor retention is replenished.
        charge(budget)?;
        charge(budget)?;
        charge(budget)?;
        self.progress.uncertain_bytes_upper_bound = bytes.len();
        self.consumed_bytes += bytes.len();
        let mut result = self
            .host
            .as_mut()
            .expect("host admitted before draw")
            .fill(bytes, &|| {
                context.cancellation().poll_until(deadline).is_some()
            });
        // A failed launch can prove that no fill started, even if cancellation
        // arrives at the same boundary. Settle that proof before the primary.
        let progress = result
            .as_ref()
            .map_or_else(|error| error.progress, |()| FillProgress::Confirmed);
        if progress == FillProgress::NotStarted {
            self.consumed_bytes -= bytes.len();
            self.progress.uncertain_bytes_upper_bound = 0;
            if self.progress.confirmed_bytes == 0 {
                self.progress.admitted_bytes = 0;
            }
        }
        if progress == FillProgress::Confirmed {
            self.progress.confirmed_bytes += bytes.len();
            self.progress.uncertain_bytes_upper_bound = 0;
        }
        if let Err(error) = &mut result
            && let Some(cleanup) = error.cleanup_error.take()
            && self.cleanup_error.is_none()
        {
            self.cleanup_error = Some(opaal_platform::operational::OperationalError::new(
                cleanup.kind(),
                context.redact_text(cleanup.message()),
            ));
        }
        if let Some(reason) = context.cancellation().poll_until(deadline) {
            bytes.fill(0);
            return Err(ModuleError::Cancelled(reason));
        }
        if let Err(error) = result {
            bytes.fill(0);
            return if error.progress == FillProgress::NotStarted
                && error.error.kind()
                    == opaal_platform::operational::OperationalErrorKind::Unsupported
            {
                Err(ModuleError::Authority {
                    verdict: AuthorityVerdict::Unsupported,
                })
            } else {
                Err(adapter_error(context, error.error))
            };
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum RandomCall {
    Int { min: i64, width: u128 },
    Float,
    Bytes(usize),
}

impl RandomCall {
    fn parse(operation: &str, arguments: &[Value], cap: usize) -> Result<Self, ModuleError> {
        match (operation, arguments) {
            ("int", [Value::Int(min), Value::Int(max)]) => {
                if min >= max {
                    return Err(ModuleError::invalid(
                        "RANDOM001",
                        "random interval must be nonempty",
                    ));
                }
                Ok(Self::Int {
                    min: *min,
                    width: (i128::from(*max) - i128::from(*min)) as u128,
                })
            }
            ("float", []) => Ok(Self::Float),
            ("bytes", [Value::Int(count)]) => {
                let count = usize::try_from(*count).map_err(|_| {
                    ModuleError::invalid("RANDOM001", "random byte count must be nonnegative")
                })?;
                if count > cap {
                    return Err(ModuleError::invalid(
                        "RANDOM001",
                        "random byte count exceeds its call limit",
                    ));
                }
                Ok(Self::Bytes(count))
            }
            _ => Err(ModuleError::invalid(
                "OPERATION001",
                "random arguments do not match the declared signature",
            )),
        }
    }

    fn requested_bytes(self, candidates: usize) -> usize {
        match self {
            Self::Int { width: 1, .. } => 0,
            Self::Int { .. } => candidates * 8,
            Self::Float => 8,
            Self::Bytes(count) => count,
        }
    }
}

fn limit() -> ModuleError {
    ModuleError::invalid("RESOURCE_LIMIT", "evaluation resource budget exhausted")
}
fn charge(budget: &mut ResourceBudget) -> Result<(), ModuleError> {
    if budget.charge() {
        Ok(())
    } else {
        Err(limit())
    }
}
fn poll(context: &OperationalContext, deadline: Deadline) -> Result<(), ModuleError> {
    context
        .cancellation()
        .poll_until(deadline)
        .map_or(Ok(()), |reason| Err(ModuleError::Cancelled(reason)))
}
