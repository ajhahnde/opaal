//! Bounded standard streams using the same host and consumed bytes as entropy.

use std::borrow::Cow;

use opaal_platform::Platform;
use opaal_platform::operational::{OperationalError, OperationalErrorKind};
use opaal_platform::standard_host::{MAX_STREAM_CHUNK_BYTES, StandardStream, TransferError};

use crate::Value;
use crate::authority::{AuthorityVerdict, CapabilityRequest, EffectSet};
use crate::context::OperationalContext;
use crate::eval::ResourceBudget;
use crate::lifetime::Deadline;

use super::standard::{HostProgress, StandardState, charge, limit, poll};
use super::{ModuleError, adapter_error, authorize};

mod display;

pub(crate) enum IoCall<'a> {
    Read(usize),
    Write {
        stream: StandardStream,
        bytes: Cow<'a, [u8]>,
        lf: bool,
    },
}

impl IoCall<'_> {
    fn request(&self) -> CapabilityRequest {
        match self {
            Self::Read(_) => CapabilityRequest::stdin_read(),
            Self::Write {
                stream: StandardStream::Stdout,
                ..
            } => CapabilityRequest::stdout_write(),
            Self::Write { .. } => CapabilityRequest::stderr_write(),
        }
    }

    fn stream(&self) -> StandardStream {
        match self {
            Self::Read(_) => StandardStream::Stdin,
            Self::Write { stream, .. } => *stream,
        }
    }

    fn requested_bytes(&self) -> usize {
        match self {
            Self::Read(cap) => cap + 1,
            Self::Write { bytes, lf, .. } => bytes.len() + usize::from(*lf),
        }
    }
}

impl StandardState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_stdio<'a>(
        &mut self,
        context: &OperationalContext,
        effects: &EffectSet,
        platform: &dyn Platform,
        budget: &mut ResourceBudget,
        name: &str,
        arguments: &'a [Value],
        deadline: Deadline,
    ) -> Result<IoCall<'a>, ModuleError> {
        self.progress = HostProgress::default();
        let cap = self.limits.max_call_bytes;
        let result = (|| {
            poll(context, deadline)?;
            let call = match (name, arguments) {
                ("read_stdin", [Value::Int(count)]) => {
                    IoCall::Read(usize::try_from(*count).map_err(|_| domain())?)
                }
                ("print" | "println" | "eprint" | "eprintln", [value]) => {
                    let stream = if name.starts_with('e') {
                        StandardStream::Stderr
                    } else {
                        StandardStream::Stdout
                    };
                    let bytes = match value {
                        Value::String(text) => Cow::Borrowed(text.as_bytes()),
                        value => {
                            self.stdio_binding(context, effects, platform, stream)?;
                            Cow::Owned(display::prepare(
                                value,
                                cap.checked_sub(usize::from(name.ends_with("println")))
                                    .ok_or_else(domain)?,
                                context,
                                budget,
                                deadline,
                            )?)
                        }
                    };
                    IoCall::Write {
                        stream,
                        bytes,
                        lf: name.ends_with("println"),
                    }
                }
                ("write_stdout" | "write_stderr", [Value::Bytes(bytes)]) => IoCall::Write {
                    stream: if name == "write_stdout" {
                        StandardStream::Stdout
                    } else {
                        StandardStream::Stderr
                    },
                    bytes: Cow::Borrowed(bytes),
                    lf: false,
                },
                _ => {
                    return Err(ModuleError::invalid(
                        "OPERATION001",
                        "I/O arguments do not match the declared signature",
                    ));
                }
            };
            let length = match &call {
                IoCall::Read(cap) => *cap,
                IoCall::Write { bytes, lf, .. } => bytes
                    .len()
                    .checked_add(usize::from(*lf))
                    .ok_or_else(domain)?,
            };
            if length > cap {
                return Err(domain());
            }
            Ok(call)
        })();
        if matches!(result, Err(ModuleError::Cancelled(_))) {
            self.close_after_failure();
        }
        result
    }

    /// Validate grants, exact binding and cap+probe for the prepared call.
    pub(crate) fn stdio_admission(
        &self,
        context: &OperationalContext,
        effects: &EffectSet,
        platform: &dyn Platform,
        call: &IoCall<'_>,
    ) -> Result<HostProgress, ModuleError> {
        if let Some(reason) = context.poll_cancellation() {
            return Err(ModuleError::Cancelled(reason));
        }
        self.stdio_binding(context, effects, platform, call.stream())?;
        let requested_bytes = call.requested_bytes();
        if requested_bytes
            > self
                .limits
                .max_host_bytes
                .saturating_sub(self.consumed_bytes)
        {
            return Err(limit());
        }
        Ok(HostProgress {
            requested_bytes,
            admitted_bytes: requested_bytes,
            ..HostProgress::default()
        })
    }

    fn stdio_binding(
        &self,
        context: &OperationalContext,
        effects: &EffectSet,
        platform: &dyn Platform,
        stream: StandardStream,
    ) -> Result<(), ModuleError> {
        let request = match stream {
            StandardStream::Stdin => CapabilityRequest::stdin_read(),
            StandardStream::Stdout => CapabilityRequest::stdout_write(),
            StandardStream::Stderr => CapabilityRequest::stderr_write(),
        };
        authorize(context, effects, &request, platform)?;
        let host = self
            .host
            .as_ref()
            .filter(|host| host.stream_available(stream))
            .ok_or(ModuleError::Authority {
                verdict: AuthorityVerdict::Unsupported,
            })?;
        if host.evaluation() != context.authority().evaluation().get() {
            return Err(ModuleError::invalid(
                "EXECUTE_STALE",
                "standard host belongs to another evaluation",
            ));
        }
        Ok(())
    }

    pub fn invoke_stdio(
        &mut self,
        context: &OperationalContext,
        effects: &EffectSet,
        platform: &dyn Platform,
        budget: &mut ResourceBudget,
        operation: &str,
        arguments: &[Value],
    ) -> Result<Value, ModuleError> {
        let deadline = self.operation_deadline(context);
        let call = self.prepare_stdio(
            context, effects, platform, budget, operation, arguments, deadline,
        )?;
        self.invoke_stdio_with_deadline(context, effects, platform, budget, call, deadline)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn invoke_stdio_with_deadline(
        &mut self,
        context: &OperationalContext,
        effects: &EffectSet,
        platform: &dyn Platform,
        budget: &mut ResourceBudget,
        call: IoCall<'_>,
        deadline: Deadline,
    ) -> Result<Value, ModuleError> {
        self.progress = HostProgress::default();
        let result = (|| {
            self.progress = self.stdio_admission(context, effects, platform, &call)?;
            poll(context, deadline)?;
            charge(budget)?;
            match &call {
                IoCall::Read(cap) => {
                    // Admit potential retention before reading; settle only the returned value.
                    let mut reservation = *budget;
                    if !reservation.charge_collection_bytes(*cap) {
                        return Err(limit());
                    }
                    let mut output = Vec::with_capacity(*cap);
                    let mut chunk = [0; MAX_STREAM_CHUNK_BYTES];
                    loop {
                        let length = (cap + 1 - output.len()).min(chunk.len());
                        self.admit_chunk(context, budget, deadline, length)?;
                        let result = self
                            .host
                            .as_mut()
                            .expect("admitted host")
                            .read(&mut chunk[..length], &|| {
                                context.cancellation().poll_until(deadline).is_some()
                            });
                        if matches!(result, Ok(0)) {
                            self.progress.eof = Some(true);
                        }
                        if let Err(error) = &result
                            && error.eof == Some(true)
                            && error.confirmed_bytes == 0
                            && error.uncertain_bytes_upper_bound == 0
                        {
                            self.progress.eof = Some(true);
                        }
                        let Some(count) = self.settle_chunk(context, deadline, length, result)?
                        else {
                            continue;
                        };
                        if count == 0 {
                            if !budget.charge_collection_bytes(output.len()) {
                                return Err(limit());
                            }
                            return Ok(Value::bytes(output));
                        }
                        if count > cap - output.len() {
                            return Err(ModuleError::invalid(
                                "IO002",
                                "standard input exceeds its byte cap",
                            ));
                        }
                        output.extend_from_slice(&chunk[..count]);
                    }
                }
                IoCall::Write { stream, bytes, lf } => {
                    for segment in [bytes.as_ref(), if *lf { b"\n" } else { b"" }] {
                        let mut offset = 0;
                        while offset < segment.len() {
                            let end = (offset + MAX_STREAM_CHUNK_BYTES).min(segment.len());
                            let chunk = &segment[offset..end];
                            self.admit_chunk(context, budget, deadline, chunk.len())?;
                            let result = self.host.as_mut().expect("admitted host").write(
                                *stream,
                                chunk,
                                &|| context.cancellation().poll_until(deadline).is_some(),
                            );
                            let Some(count) =
                                self.settle_chunk(context, deadline, chunk.len(), result)?
                            else {
                                continue;
                            };
                            if count == 0 {
                                return Err(ModuleError::invalid(
                                    "IO004",
                                    "standard output made no progress",
                                ));
                            }
                            offset += count;
                        }
                    }
                    Ok(Value::Null)
                }
            }
        })();
        self.progress.admitted_bytes =
            self.progress.confirmed_bytes + self.progress.uncertain_bytes_upper_bound;
        self.record_progress(call.request().effect());
        if matches!(result, Err(ModuleError::Cancelled(_))) {
            self.close_after_failure();
        }
        result
    }

    fn admit_chunk(
        &self,
        context: &OperationalContext,
        budget: &mut ResourceBudget,
        deadline: Deadline,
        length: usize,
    ) -> Result<(), ModuleError> {
        poll(context, deadline)?;
        charge(budget)?;
        // Charge both transport directions/copies before the syscall, including retries.
        for _ in 0..length.div_ceil(4096) * 2 {
            charge(budget)?;
        }
        Ok(())
    }

    /// Settle proof before polling cancellation; unacknowledged work stays charged.
    fn settle_chunk(
        &mut self,
        context: &OperationalContext,
        deadline: Deadline,
        admitted: usize,
        result: Result<usize, TransferError>,
    ) -> Result<Option<usize>, ModuleError> {
        let (confirmed, uncertain) = match &result {
            Ok(count) => (*count, 0),
            Err(error) => (error.confirmed_bytes, error.uncertain_bytes_upper_bound),
        };
        if confirmed
            .checked_add(uncertain)
            .is_none_or(|count| count > admitted)
        {
            self.progress.uncertain_bytes_upper_bound = admitted;
            self.consumed_bytes += admitted;
            self.close_after_failure();
            return Err(ModuleError::invalid(
                "OPERATION004",
                "invalid standard stream progress",
            ));
        }
        self.progress.confirmed_bytes += confirmed;
        self.progress.uncertain_bytes_upper_bound = uncertain;
        self.consumed_bytes += confirmed + uncertain;
        if let Err(error) = &result
            && error.cleanup_error.is_some()
            && self.cleanup_error.is_none()
        {
            self.cleanup_error = Some(OperationalError::new(
                OperationalErrorKind::Protocol,
                "standard host cleanup failed",
            ));
        }
        poll(context, deadline)?;
        match result {
            Ok(count) => Ok(Some(count)),
            Err(error)
                if confirmed == 0
                    && uncertain == 0
                    && matches!(
                        error.error.kind(),
                        OperationalErrorKind::Io(
                            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                        )
                    ) =>
            {
                Ok(None)
            }
            Err(error)
                if confirmed == 0
                    && uncertain == 0
                    && error.error.kind() == OperationalErrorKind::Unsupported =>
            {
                Err(ModuleError::Authority {
                    verdict: AuthorityVerdict::Unsupported,
                })
            }
            Err(error)
                if error.error.kind()
                    == OperationalErrorKind::Io(std::io::ErrorKind::BrokenPipe) =>
            {
                Err(ModuleError::invalid(
                    "IO003",
                    "standard output is a broken pipe",
                ))
            }
            Err(error) => Err(adapter_error(
                context,
                OperationalError::new(error.error.kind(), "standard stream transfer failed"),
            )),
        }
    }

    fn close_after_failure(&mut self) {
        if let Some(mut host) = self.host.take()
            && let Err(error) = host.close()
            && self.cleanup_error.is_none()
        {
            self.cleanup_error = Some(OperationalError::new(
                error.kind(),
                "standard host cleanup failed",
            ));
        }
    }
}

fn domain() -> ModuleError {
    ModuleError::invalid(
        "IO001",
        "standard stream byte count exceeds its call limit or is negative",
    )
}
