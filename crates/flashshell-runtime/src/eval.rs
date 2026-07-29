//! Pure tree-walking evaluation of a parsed script.
//!
//! This layer turns a `Script` into a `Value` without any process, terminal,
//! filesystem, or environment dependency. Command execution and boolean
//! short-circuiting are deferred to the slices that own them and currently
//! surface as precise unsupported errors.

use std::any::Any;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant as SystemInstant;

use flashshell_platform::{
    DescriptorReadError, DescriptorWriteError, DirectoryReadError, FileActionError, PipeError,
    PlatformError, SignalError, SpawnError, WaitError, WorkingDirectoryError,
};
use flashshell_syntax::{
    AndChain, Assignment, BinaryOperator, Block, CallExpression, Closure, ConditionalChain,
    ControlTransfer, Declaration, ElseBranch, EnvironmentStatement, Expression, ExpressionKind,
    ForStatement, FunctionDefinition, IfStatement, Literal, LiteralKind, MatchArm, MatchStatement,
    Parameter, Pattern, Pipeline, RecordKey, SourceFile, Span, StageKind, Statement, StatementKind,
    UnaryOperator, VariableReference, WhileStatement, Word, WordPart, WordPartKind,
};

use crate::operation::{self, OperationError};
use crate::{BindingMutability, Callable, Environment, Record, ScopeError, ScopeStack, Value};

/// A source-anchored runtime evaluation failure.
///
/// The `kind` and primary `span` identify the failing node. `frames` records the
/// chain of function and closure calls the error unwound out of, ordered from the
/// call nearest the failure outward; a top-level failure has no frames.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeError {
    kind: RuntimeErrorKind,
    span: Span,
    frames: Vec<CallFrame>,
}

impl RuntimeError {
    #[must_use]
    pub const fn new(kind: RuntimeErrorKind, span: Span) -> Self {
        Self {
            kind,
            span,
            frames: Vec::new(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &RuntimeErrorKind {
        &self.kind
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    /// The innermost-first call frames the error unwound through.
    #[must_use]
    pub fn frames(&self) -> &[CallFrame] {
        &self.frames
    }

    /// Appends an enclosing call frame as the error unwinds outward.
    #[must_use]
    fn with_frame(mut self, frame: CallFrame) -> Self {
        self.frames.push(frame);
        self
    }
}

/// One function or closure call a [`RuntimeError`] unwound out of.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallFrame {
    callee: FrameCallee,
    call_site: Span,
}

impl CallFrame {
    fn new(name: Option<&str>, call_site: Span) -> Self {
        let callee = match name {
            Some(name) => FrameCallee::Function(name.to_owned()),
            None => FrameCallee::Closure,
        };
        Self { callee, call_site }
    }

    /// The identity of the called function or closure.
    #[must_use]
    pub const fn callee(&self) -> &FrameCallee {
        &self.callee
    }

    /// The span of the call expression that entered the body.
    #[must_use]
    pub const fn call_site(&self) -> Span {
        self.call_site
    }
}

/// The identity of a called function or closure recorded in a [`CallFrame`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameCallee {
    /// A named `def` function.
    Function(String),
    /// An anonymous closure.
    Closure,
}

/// A capability deliberately unavailable while automatic startup config runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestrictedCapability {
    /// Starting or composing commands and pipelines.
    ProcessExecution,
    /// Capturing the output of a command substitution.
    CommandSubstitution,
}

impl RestrictedCapability {
    /// Stable diagnostic spelling for the unavailable capability.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ProcessExecution => "process execution",
            Self::CommandSubstitution => "command substitution",
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(formatter)
    }
}

impl Error for RuntimeError {}

/// The explicit boundary that would repair a structured-to-byte or
/// byte-to-structured pipeline edge.
///
/// A structured producer meeting a byte consumer needs serialization; a byte
/// producer meeting a structured consumer needs parsing. The concrete boundary
/// commands (`encode`/`to`, `decode`/`from`) arrive with the structured-command
/// slice; this hint names the direction so a mismatch points at its own fix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CarrierBridge {
    /// Serialize a structured stream into bytes before a byte consumer.
    StructuredToByte,
    /// Parse bytes into structured values before a structured consumer.
    ByteToStructured,
}

/// The actionable detail of an incompatible pipeline edge.
///
/// Boxed into its [`RuntimeErrorKind::CarrierMismatch`] variant so a large,
/// rarely constructed diagnostic does not widen every runtime `Result`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CarrierMismatch {
    /// The producing stage's head word, as the reader typed it.
    pub producer_command: String,
    /// The carrier the producing stage emits.
    pub produced: crate::command::Carrier,
    /// The consuming stage's head word, as the reader typed it.
    pub consumer_command: String,
    /// The carrier set the consuming stage accepts, in a deterministic order.
    pub accepted: Vec<crate::command::Carrier>,
    /// The explicit boundary that would repair a structured-to-byte crossing,
    /// when one applies.
    pub bridge: Option<CarrierBridge>,
}

/// A source-independent runtime failure kind.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum RuntimeErrorKind {
    /// A pure-operation failure raised while evaluating an operator.
    Operation(OperationError),
    /// A lexical-scope failure raised by a binding, read, or assignment.
    Scope(ScopeError),
    /// An `if`/`while` condition that did not evaluate to `Bool` or `Status`.
    ConditionNotBool { actual: &'static str },
    /// An operand of unary `!` that did not evaluate to `Bool`.
    LogicOperandNotBool { actual: &'static str },
    /// An operand of `&&` or `||` that did not evaluate to `Bool` or `Status`.
    ConditionalOperandNotBoolOrStatus { actual: &'static str },
    /// A `for` iterable that is neither a `List` nor a `Range`.
    NotIterable { actual: &'static str },
    /// `break` or `continue` used outside any loop.
    ControlOutsideLoop { control: ControlKind },
    /// `return` used outside any function.
    ReturnOutsideFunction,
    /// A `match` whose scrutinee matched no arm.
    NoMatchingArm,
    /// A call whose callee is not a function or closure.
    NotCallable { actual: &'static str },
    /// A call whose argument count does not match the parameter count.
    ArityMismatch { expected: usize, actual: usize },
    /// A function or closure declaring the same parameter name twice.
    DuplicateParameter { name: String },
    /// A construct requiring the execution engine that does not exist yet.
    ExecutionUnsupported,
    /// Automatic startup config reached an operation outside its capability set.
    RestrictedStartup { capability: RestrictedCapability },
    /// Evaluation charged more steps than its resource budget allowed.
    ResourceBudgetExceeded,
    /// A language form deferred to a later evaluation slice.
    Unsupported { feature: &'static str },
    /// An integer literal outside the signed 64-bit range.
    IntegerLiteralOverflow,
    /// A float literal that is not a finite binary64 value.
    FloatLiteralOverflow,
    /// A record literal repeating a key.
    DuplicateRecordKey { key: String },
    /// An ordinary word interpolation produced a value that cannot become an
    /// argument. `actual` names the offending value family.
    WordValueNotWordEligible { actual: &'static str },
    /// A `...$name` spread whose binding did not hold a `List`. `actual` names the
    /// offending value family.
    SpreadValueNotList { actual: &'static str },
    /// A `...$name` spread element that cannot become an argument. `index` is the
    /// zero-based list position and `actual` names the offending value family.
    SpreadElementNotWordEligible { index: usize, actual: &'static str },
    /// An `export` whose value cannot become a native environment string.
    /// `actual` names the offending value family.
    ExportValueNotEligible { actual: &'static str },
    /// A command name that resolved to neither an internal command nor an
    /// executable on `PATH`. `name` is the searched native command name.
    CommandNotFound { name: OsString },
    /// A redirection descriptor number whose decimal spelling does not fit in a
    /// `u32`.
    RedirectionDescriptorOverflow,
    /// An argv argument or redirection target containing a NUL byte, which no
    /// external argv or platform path can represent.
    ArgumentContainsNul,
    /// A pipeline edge whose producer carrier the consumer cannot accept. The
    /// boxed [`CarrierMismatch`] names both stages, the accepted carrier set, and
    /// the explicit boundary that would repair a structured-to-byte crossing.
    CarrierMismatch(Box<CarrierMismatch>),
    /// A merged stdout+stderr edge (`|&`) whose producer is not a byte stream.
    /// `producer_command` is the producing head word and `produced` its carrier.
    MergedEdgeNotByteStream {
        producer_command: String,
        produced: crate::command::Carrier,
    },
    /// A pipeline head stage whose command does not accept an empty input: it
    /// requires an upstream stage to consume. `command` is the head word and
    /// `accepted` the carrier set it consumes.
    PipelineHeadInput {
        command: String,
        accepted: Vec<crate::command::Carrier>,
    },
    /// A descriptor duplication (`n>&m`) whose source `m` is not open in the
    /// stage's descriptor map at that point.
    DescriptorNotOpen { descriptor: u32 },
    /// A standard built-in received the wrong number of arguments.
    BuiltinArity {
        command: &'static str,
        minimum: usize,
        maximum: Option<usize>,
        actual: usize,
    },
    /// A standard built-in received a pipeline carrier it does not accept.
    BuiltinInputCarrier {
        command: &'static str,
        input: crate::command::Carrier,
    },
    /// A standard internal command received an argument it could not interpret.
    BuiltinArgument {
        command: &'static str,
        message: String,
    },
    /// A structured command refused or could not transform its input.
    StructuredCommand {
        command: &'static str,
        message: String,
    },
    /// A platform directory walk failed while a structured `ls` was being pulled.
    DirectoryRead(DirectoryReadError),
    /// A lazy structured stream observed cooperative cancellation.
    StreamCancelled { reason: CancelReason },
    /// `cd` without an argument could not find a HOME environment entry.
    MissingHome,
    /// Resolving or validating a requested logical working directory failed.
    WorkingDirectory(WorkingDirectoryError),
    /// An explicit `exit` code was not ASCII decimal in the range 0 through 255.
    InvalidExitCode,
    /// `check` appeared without an upstream stage and status.
    CheckRequiresUpstream,
    /// `check` explicitly converted an unsuccessful completed status.
    UnsuccessfulStatus { status: Box<crate::Status> },
    /// A structured final carrier was aimed at a destination that requires
    /// explicit serialization rather than terminal presentation.
    Presentation(crate::presentation::PresentationError),
    /// The platform could not supply the terminal information needed to select
    /// width-aware presentation.
    TerminalPresentation(PlatformError),
    /// The platform could not identify the running shell executable needed for
    /// an isolated background chain.
    ShellExecutable(PlatformError),
    /// The platform rejected or failed creation of an anonymous pipeline edge.
    PipeCreate(PipeError),
    /// The platform rejected or failed creation of the stdout capture pipe.
    CapturePipe(PipeError),
    /// Reading the captured stdout pipe failed while draining it.
    CaptureRead(DescriptorReadError),
    /// Reading an external stage's bytes into an internal pipeline failed.
    PipelineRead(DescriptorReadError),
    /// Writing an internal byte stream into an external stage failed.
    PipelineWrite(DescriptorWriteError),
    /// Captured stdout exceeded the plan's configured raw-byte limit.
    CaptureLimitExceeded { limit: usize },
    /// Text capture encountered invalid UTF-8.
    CaptureInvalidUtf8 {
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    /// A source-ordered redirection file action could not be prepared.
    RedirectionSetup(FileActionError),
    /// The platform rejected or failed a direct external-process spawn.
    ProcessSpawn(SpawnError),
    /// Waiting for a successfully spawned external process failed.
    ProcessWait(WaitError),
    /// The terminal could not be handed to a foreground job that must own it.
    ///
    /// Distinct from a spawn failure: the job's processes exist, and running
    /// them without the terminal would send every keyboard interrupt to the
    /// shell instead of to the job.
    ForegroundTerminal(PlatformError),
    /// Background startup could not establish and verify one process group for
    /// every pipeline member.
    BackgroundProcessGroupUnavailable,
    /// A started child reported the reserved zero process identity.
    InvalidProcessIdentity,
    /// The session exhausted its monotonic background job or notice identity
    /// space.
    BackgroundIdentityExhausted,
    /// Idle background observer preparation failed before process creation.
    BackgroundObserverUnavailable { message: String },
    /// A ready observer unexpectedly refused its owned child assignment.
    BackgroundAssignmentUnavailable,
    /// Observation of a coordinator-owned foreground job was abandoned.
    ForegroundObservation { message: String },
    /// The background coordinator reached an invalid pure job transition.
    BackgroundJobState { message: String },
    /// A job-control command ran in a session that has no job coordinator.
    ///
    /// Distinct from an unimplemented feature: the command exists, and the
    /// session it was typed into simply owns no job table.
    JobControlUnavailable { command: &'static str },
    /// A job-control command shared a pipeline with an external stage.
    ///
    /// Job commands run against the session-owned coordinator, which the mixed
    /// process executor deliberately cannot reach.
    JobControlNotInternal { command: &'static str },
    /// A job-control command with process or terminal effects was not alone.
    ///
    /// Ordinary internal stages run against a clone of the session state that a
    /// failure rolls back. A delivered signal cannot be rolled back, so these
    /// commands may not be a member of a longer pipeline.
    JobControlNotSoleStage { command: &'static str },
    /// A job-control command failed against an addressable record.
    JobOperation {
        command: &'static str,
        message: String,
    },
    /// A stopped job could not be resumed, so waiting on it could not continue.
    ///
    /// Distinct from a wait failure: the job exists and is merely stopped. A
    /// stop is reported to the observer once, so waiting again without resuming
    /// the job would block until something else resumed it, and in a session
    /// with no way to address a stopped job nothing ever would.
    JobSignal(SignalError),
    /// A job reported a stop without belonging to a process group.
    ///
    /// Resuming addresses the group, so there is nothing to send the resume to.
    /// Every member stays in the shell's own group when process groups are
    /// unavailable, which means a terminal stop would have stopped the shell as
    /// well; observing one anyway is a state the executor reports rather than
    /// one it can act on.
    UngroupedStop,
}

/// Renders a carrier set as a human list: `A`, `A or B`, or `A, B, or C`.
fn render_carrier_set(carriers: &[crate::command::Carrier]) -> String {
    match carriers {
        [] => "nothing".to_owned(),
        [only] => format!("{only:?}"),
        [first, second] => format!("{first:?} or {second:?}"),
        [rest @ .., last] => {
            let mut out = String::new();
            for carrier in rest {
                out.push_str(&format!("{carrier:?}, "));
            }
            out.push_str(&format!("or {last:?}"));
            out
        }
    }
}

impl fmt::Display for RuntimeErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operation(error) => error.fmt(formatter),
            Self::Scope(error) => error.fmt(formatter),
            Self::ConditionNotBool { actual } => {
                write!(
                    formatter,
                    "condition must be a bool or status, found {actual}"
                )
            }
            Self::LogicOperandNotBool { actual } => {
                write!(formatter, "logical operand must be a bool, found {actual}")
            }
            Self::ConditionalOperandNotBoolOrStatus { actual } => {
                write!(
                    formatter,
                    "conditional operand must be a bool or status, found {actual}"
                )
            }
            Self::NotIterable { actual } => {
                write!(formatter, "cannot iterate a {actual}")
            }
            Self::ControlOutsideLoop { control } => {
                write!(formatter, "`{}` used outside a loop", control.keyword())
            }
            Self::ReturnOutsideFunction => formatter.write_str("`return` used outside a function"),
            Self::NoMatchingArm => formatter.write_str("no match arm matched the value"),
            Self::NotCallable { actual } => {
                write!(formatter, "cannot call a {actual}")
            }
            Self::ArityMismatch { expected, actual } => {
                write!(formatter, "expected {expected} argument(s), found {actual}")
            }
            Self::DuplicateParameter { name } => {
                write!(formatter, "duplicate parameter {name:?}")
            }
            Self::ExecutionUnsupported => {
                formatter.write_str("command execution is not available in pure evaluation")
            }
            Self::RestrictedStartup { capability } => write!(
                formatter,
                "{} is not available during automatic startup",
                capability.name()
            ),
            Self::ResourceBudgetExceeded => {
                formatter.write_str("evaluation exceeded its resource budget")
            }
            Self::Unsupported { feature } => write!(formatter, "{feature} is not yet supported"),
            Self::IntegerLiteralOverflow => formatter.write_str("integer literal is out of range"),
            Self::FloatLiteralOverflow => formatter.write_str("float literal is not finite"),
            Self::DuplicateRecordKey { key } => {
                write!(formatter, "duplicate record key {key:?}")
            }
            Self::WordValueNotWordEligible { actual } => write!(
                formatter,
                "cannot use a {actual} as a command word; expected bool, int, float, \
                 string, path, duration, or byte size"
            ),
            Self::SpreadValueNotList { actual } => {
                write!(formatter, "cannot spread a {actual}; `...` requires a list")
            }
            Self::SpreadElementNotWordEligible { index, actual } => write!(
                formatter,
                "cannot use the {actual} at spread index {index} as a command word; expected \
                 bool, int, float, string, path, duration, or byte size"
            ),
            Self::ExportValueNotEligible { actual } => write!(
                formatter,
                "cannot export a {actual}; expected bool, int, float, string, path, \
                 duration, or byte size"
            ),
            Self::CommandNotFound { name } => {
                write!(formatter, "command not found: {}", name.to_string_lossy())
            }
            Self::RedirectionDescriptorOverflow => {
                formatter.write_str("redirection descriptor number is out of range")
            }
            Self::ArgumentContainsNul => {
                formatter.write_str("argument or redirection target contains a NUL byte")
            }
            Self::CarrierMismatch(mismatch) => {
                let CarrierMismatch {
                    producer_command,
                    produced,
                    consumer_command,
                    accepted,
                    bridge,
                } = mismatch.as_ref();
                write!(
                    formatter,
                    "incompatible pipeline edge: `{producer_command}` emits a {produced:?} but \
                     `{consumer_command}` accepts {}",
                    render_carrier_set(accepted)
                )?;
                match bridge {
                    Some(CarrierBridge::StructuredToByte) => formatter.write_str(
                        "; add an explicit `encode`/`to` boundary to serialize the \
                         structured stream to bytes",
                    ),
                    Some(CarrierBridge::ByteToStructured) => formatter.write_str(
                        "; add an explicit `decode`/`from` boundary to parse the bytes \
                         into structured values",
                    ),
                    None => Ok(()),
                }
            }
            Self::MergedEdgeNotByteStream {
                producer_command,
                produced,
            } => write!(
                formatter,
                "a `|&` edge merges stderr and requires a byte-stream producer, but \
                 `{producer_command}` emits a {produced:?}"
            ),
            Self::PipelineHeadInput { command, accepted } => write!(
                formatter,
                "`{command}` needs an upstream pipeline stage: it accepts {} input, not an \
                 empty pipeline head",
                render_carrier_set(accepted)
            ),
            Self::DescriptorNotOpen { descriptor } => write!(
                formatter,
                "cannot duplicate descriptor {descriptor}: it is not open in this stage"
            ),
            Self::BuiltinArity {
                command,
                minimum,
                maximum,
                actual,
            } => match maximum {
                Some(maximum) if minimum == maximum => write!(
                    formatter,
                    "{command} expects {minimum} argument(s), found {actual}"
                ),
                Some(maximum) => write!(
                    formatter,
                    "{command} expects {minimum}..={maximum} arguments, found {actual}"
                ),
                None => write!(
                    formatter,
                    "{command} expects at least {minimum} argument(s), found {actual}"
                ),
            },
            Self::BuiltinInputCarrier { command, input } => {
                write!(formatter, "{command} does not accept {input:?} input")
            }
            Self::BuiltinArgument { command, message } => {
                write!(formatter, "{command}: {message}")
            }
            Self::StructuredCommand { command, message } => {
                write!(formatter, "{command}: {message}")
            }
            Self::DirectoryRead(error) => error.fmt(formatter),
            Self::StreamCancelled { reason } => {
                write!(formatter, "structured stream cancelled: {reason:?}")
            }
            Self::MissingHome => formatter.write_str("cd requires a HOME environment entry"),
            Self::WorkingDirectory(error) => error.fmt(formatter),
            Self::InvalidExitCode => {
                formatter.write_str("exit code must be ASCII decimal from 0 through 255")
            }
            Self::CheckRequiresUpstream => {
                formatter.write_str("check requires an upstream pipeline stage")
            }
            Self::UnsuccessfulStatus { status } => {
                write!(formatter, "checked command was unsuccessful: {status}")
            }
            Self::Presentation(error) => error.fmt(formatter),
            Self::TerminalPresentation(error) => {
                write!(formatter, "terminal presentation is unavailable: {error}")
            }
            Self::ShellExecutable(error) => {
                write!(formatter, "shell re-execution is unavailable: {error}")
            }
            Self::PipeCreate(error) => error.fmt(formatter),
            Self::CapturePipe(error) => error.fmt(formatter),
            Self::CaptureRead(error) => error.fmt(formatter),
            Self::PipelineRead(error) => error.fmt(formatter),
            Self::PipelineWrite(error) => error.fmt(formatter),
            Self::CaptureLimitExceeded { limit } => {
                write!(
                    formatter,
                    "command output exceeds the {limit}-byte capture limit"
                )
            }
            Self::CaptureInvalidUtf8 {
                valid_up_to,
                error_len,
            } => {
                write!(
                    formatter,
                    "command output is not valid UTF-8 at byte {valid_up_to}"
                )?;
                if let Some(length) = error_len {
                    write!(formatter, " (invalid sequence length {length})")?;
                }
                formatter.write_str("; use capture bytes to preserve arbitrary output")
            }
            Self::RedirectionSetup(error) => error.fmt(formatter),
            Self::ProcessSpawn(error) => error.fmt(formatter),
            Self::ProcessWait(error) => error.fmt(formatter),
            Self::ForegroundTerminal(error) => {
                write!(formatter, "terminal handover to the job failed: {error}")
            }
            Self::BackgroundProcessGroupUnavailable => {
                formatter.write_str("background execution requires one established process group")
            }
            Self::InvalidProcessIdentity => {
                formatter.write_str("the platform reported the reserved zero process identity")
            }
            Self::BackgroundIdentityExhausted => {
                formatter.write_str("background job identity space is exhausted")
            }
            Self::BackgroundObserverUnavailable { message } => {
                write!(
                    formatter,
                    "background child observation is unavailable: {message}"
                )
            }
            Self::BackgroundAssignmentUnavailable => {
                formatter.write_str("a ready background observer refused its child")
            }
            Self::ForegroundObservation { message } => {
                write!(formatter, "foreground child observation failed: {message}")
            }
            Self::BackgroundJobState { message } => {
                write!(formatter, "invalid background job state: {message}")
            }
            Self::JobControlUnavailable { command } => {
                write!(formatter, "`{command}` requires a session with job control")
            }
            Self::JobControlNotInternal { command } => {
                write!(
                    formatter,
                    "`{command}` cannot share a pipeline with an external command"
                )
            }
            Self::JobControlNotSoleStage { command } => {
                write!(
                    formatter,
                    "`{command}` must be the only stage of its pipeline"
                )
            }
            Self::JobOperation { command, message } => {
                write!(formatter, "{command}: {message}")
            }
            Self::JobSignal(error) => {
                write!(formatter, "resuming the stopped job failed: {error}")
            }
            Self::UngroupedStop => {
                formatter.write_str("the stopped job has no process group to resume")
            }
        }
    }
}

/// Which loop-transfer keyword produced a control-flow signal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlKind {
    Break,
    Continue,
}

impl ControlKind {
    #[must_use]
    const fn keyword(self) -> &'static str {
        match self {
            Self::Break => "break",
            Self::Continue => "continue",
        }
    }
}

/// A structured cancellation outcome, distinct from both a `Value` and a
/// [`RuntimeError`]. Cancellation stops evaluation cooperatively at loop and call
/// boundaries; it never selects `else`/`||` and is never a script value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cancellation {
    reason: CancelReason,
    span: Span,
}

impl Cancellation {
    #[must_use]
    pub const fn new(reason: CancelReason, span: Span) -> Self {
        Self { reason, span }
    }

    /// Why evaluation was cancelled.
    #[must_use]
    pub const fn reason(&self) -> CancelReason {
        self.reason
    }

    /// The boundary at which cancellation was observed.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// The reason a [`Cancellation`] was raised. Timeout, shutdown, and parent-driven
/// reasons are added by the slices that own their triggers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CancelReason {
    /// Cancellation requested through the cooperative token.
    Requested,
    /// A deadline elapsed on the token's clock.
    Timeout,
}

/// A monotonic point in time, measured in nanoseconds from a clock's origin.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Instant {
    nanos: u64,
}

impl Instant {
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self { nanos }
    }

    /// Nanoseconds from the originating clock's base.
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.nanos
    }
}

/// A monotonic time source. Implementors must never move time backwards.
pub trait Clock: Send + Sync {
    /// The current monotonic instant.
    fn now(&self) -> Instant;
}

/// A real monotonic clock reading nanoseconds elapsed from its construction.
#[derive(Clone)]
pub struct SystemClock {
    origin: SystemInstant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: SystemInstant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        // Saturating cast keeps the reading monotonic well past any test horizon.
        let nanos = self.origin.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        Instant::from_nanos(nanos)
    }
}

/// A deterministic clock whose time only advances when a test advances it.
///
/// Cloning shares one underlying time, so a token built from a clone observes
/// advances made through any handle.
#[derive(Clone, Debug, Default)]
pub struct FakeClock {
    now: Arc<AtomicU64>,
}

impl FakeClock {
    /// A clock starting at time zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A clock starting at `nanos`.
    #[must_use]
    pub fn at(nanos: u64) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(nanos)),
        }
    }

    /// Advances the shared time by `nanos`.
    pub fn advance(&self, nanos: u64) {
        self.now.fetch_add(nanos, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        Instant::from_nanos(self.now.load(Ordering::SeqCst))
    }
}

/// A cooperative cancellation signal polled at loop and call boundaries.
///
/// The token wraps a predicate so a caller can back it with an atomic flag, a
/// deadline, or a test-controlled counter without the evaluator depending on any
/// platform or clock. It also carries the reason it reports when it trips.
/// Cloning shares one underlying signal.
#[derive(Clone)]
pub struct CancellationToken {
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    reason: CancelReason,
}

impl CancellationToken {
    /// A token that never reports cancellation.
    #[must_use]
    pub fn never() -> Self {
        Self {
            is_cancelled: Arc::new(|| false),
            reason: CancelReason::Requested,
        }
    }

    /// A token whose cancellation state is decided by `predicate` on each poll.
    #[must_use]
    pub fn from_fn(predicate: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            is_cancelled: Arc::new(predicate),
            reason: CancelReason::Requested,
        }
    }

    /// A token that trips with reason `Timeout` once `clock` reaches `deadline`.
    #[must_use]
    pub fn deadline<C: Clock + 'static>(clock: C, deadline: Instant) -> Self {
        Self {
            is_cancelled: Arc::new(move || clock.now() >= deadline),
            reason: CancelReason::Timeout,
        }
    }

    /// Polls the underlying signal.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        (self.is_cancelled)()
    }

    /// The reason this token reports when it trips.
    #[must_use]
    pub const fn reason(&self) -> CancelReason {
        self.reason
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

/// A bound on the number of evaluation steps a script may charge.
///
/// The evaluator charges one step at each statement and each expression.
/// Exhausting the budget is a [`RuntimeErrorKind::ResourceBudgetExceeded`] runtime
/// error, not a cancellation.
#[derive(Clone, Copy, Debug)]
pub struct ResourceBudget {
    limit: Option<u64>,
    used: u64,
}

impl ResourceBudget {
    /// A budget that never runs out.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            limit: None,
            used: 0,
        }
    }

    /// A budget of exactly `steps` charges.
    #[must_use]
    pub const fn steps(steps: u64) -> Self {
        Self {
            limit: Some(steps),
            used: 0,
        }
    }

    /// Charges one step, returning `false` when the budget is exhausted.
    fn charge(&mut self) -> bool {
        let Some(limit) = self.limit else {
            return true;
        };
        if self.used >= limit {
            return false;
        }
        self.used += 1;
        true
    }
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// The cancellation and resource limits applied to one evaluation.
#[derive(Clone, Debug)]
pub struct EvalLimits {
    cancel: CancellationToken,
    budget: ResourceBudget,
    policy: EvaluationPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvaluationPolicy {
    General,
    Startup,
}

impl EvalLimits {
    /// Limits pairing a cancellation token with a resource budget.
    #[must_use]
    pub const fn new(cancel: CancellationToken, budget: ResourceBudget) -> Self {
        Self {
            cancel,
            budget,
            policy: EvaluationPolicy::General,
        }
    }

    /// Limits for automatic startup config with process capabilities disabled.
    #[must_use]
    pub const fn startup(cancel: CancellationToken, budget: ResourceBudget) -> Self {
        Self {
            cancel,
            budget,
            policy: EvaluationPolicy::Startup,
        }
    }
}

impl Default for EvalLimits {
    fn default() -> Self {
        Self::new(CancellationToken::never(), ResourceBudget::unlimited())
    }
}

/// The top-level outcome of evaluating a script: either a value or cancellation.
#[derive(Clone, Debug)]
pub enum Completion {
    /// The script produced its final value.
    Value(Value),
    /// Evaluation was cancelled cooperatively before completing.
    Cancelled(Cancellation),
}

/// An in-flight evaluation abort: a runtime error or a cancellation.
///
/// This is the internal short-circuit channel. A `RuntimeError` converts into the
/// `Error` arm through `?`, while cancellation rides the separate `Cancelled` arm
/// so it never becomes a `RuntimeError` at the public boundary.
enum Abort {
    Error(RuntimeError),
    Cancelled(Cancellation),
}

impl Abort {
    /// Appends a call frame to an error as it unwinds; cancellation is unchanged.
    fn with_frame(self, frame: CallFrame) -> Self {
        match self {
            Self::Error(error) => Self::Error(error.with_frame(frame)),
            cancelled @ Self::Cancelled(_) => cancelled,
        }
    }
}

impl From<RuntimeError> for Abort {
    fn from(error: RuntimeError) -> Self {
        Self::Error(error)
    }
}

/// The internal evaluation result type, short-circuiting on error or cancel.
type Eval<T> = Result<T, Abort>;

/// Evaluates a parsed script against a scope stack, returning its final value.
///
/// The script value is the value of the last expression statement, otherwise
/// `Null`. The scope stack is mutated in place so a REPL can reuse it. This entry
/// point cannot be cancelled; use [`evaluate_with_cancellation`] for a token.
pub fn evaluate(
    script: &flashshell_syntax::Script,
    source: &SourceFile,
    scope: &mut ScopeStack,
) -> Result<Value, RuntimeError> {
    match evaluate_with_limits(script, source, scope, &EvalLimits::default())? {
        Completion::Value(value) => Ok(value),
        Completion::Cancelled(_) => {
            unreachable!("a never-cancelling token cannot produce a cancellation")
        }
    }
}

/// Evaluates a parsed script, polling `cancel` at loop and call boundaries.
///
/// Returns [`Completion::Cancelled`] when the token trips at a boundary before the
/// script finishes; otherwise returns the final value. A cancellation is never a
/// [`RuntimeError`] and never a script value. The resource budget is unlimited.
pub fn evaluate_with_cancellation(
    script: &flashshell_syntax::Script,
    source: &SourceFile,
    scope: &mut ScopeStack,
    cancel: &CancellationToken,
) -> Result<Completion, RuntimeError> {
    let limits = EvalLimits::new(cancel.clone(), ResourceBudget::unlimited());
    evaluate_with_limits(script, source, scope, &limits)
}

/// Evaluates a parsed script under cancellation and resource limits.
///
/// Cancellation surfaces as [`Completion::Cancelled`]; exhausting the resource
/// budget is a [`RuntimeErrorKind::ResourceBudgetExceeded`] runtime error. This
/// entry runs against a private empty environment; use
/// [`evaluate_in_environment`] to observe `export`/`unset` mutations.
pub fn evaluate_with_limits(
    script: &flashshell_syntax::Script,
    source: &SourceFile,
    scope: &mut ScopeStack,
    limits: &EvalLimits,
) -> Result<Completion, RuntimeError> {
    let mut env = Environment::new();
    evaluate_in_environment(script, source, scope, &mut env, limits)
}

/// Evaluates a parsed script against a scope stack and a mutable environment.
///
/// `export` and `unset` mutate `env` in place, so a caller observes the child
/// environment after evaluation and a REPL reuses it across calls. Cancellation
/// and resource limits behave as in [`evaluate_with_limits`].
pub fn evaluate_in_environment(
    script: &flashshell_syntax::Script,
    source: &SourceFile,
    scope: &mut ScopeStack,
    env: &mut Environment,
    limits: &EvalLimits,
) -> Result<Completion, RuntimeError> {
    let mut evaluator = Evaluator {
        source,
        cancel: limits.cancel.clone(),
        budget: limits.budget,
        policy: limits.policy,
        env,
    };
    let mut last = Value::Null;
    for statement in script.statements() {
        let flow = match evaluator.statement(statement, scope) {
            Ok(flow) => flow,
            Err(Abort::Cancelled(cancellation)) => return Ok(Completion::Cancelled(cancellation)),
            Err(Abort::Error(error)) => return Err(error),
        };
        match flow {
            Flow::Fallthrough(Some(value)) => last = value,
            Flow::Fallthrough(None) => {}
            Flow::Break(span) => {
                return Err(RuntimeError::new(
                    RuntimeErrorKind::ControlOutsideLoop {
                        control: ControlKind::Break,
                    },
                    span,
                ));
            }
            Flow::Continue(span) => {
                return Err(RuntimeError::new(
                    RuntimeErrorKind::ControlOutsideLoop {
                        control: ControlKind::Continue,
                    },
                    span,
                ));
            }
            Flow::Return(_, span) => {
                return Err(RuntimeError::new(
                    RuntimeErrorKind::ReturnOutsideFunction,
                    span,
                ));
            }
        }
    }
    Ok(Completion::Value(last))
}

/// Evaluates one parsed closure command argument into its captured callable
/// value.
///
/// Command planning uses this seam after resolving an internal stage. Creating
/// the callable captures the current lexical scope by value but does not execute
/// the body, mutate the environment, or consume any pipeline input.
pub fn evaluate_closure_argument(
    closure: &Closure,
    source: &SourceFile,
    scope: &ScopeStack,
) -> Result<Value, RuntimeError> {
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let evaluator = Evaluator {
        source,
        cancel: limits.cancel,
        budget: limits.budget,
        policy: limits.policy,
        env: &mut env,
    };
    match evaluator.make_closure(closure, scope) {
        Ok(value) => Ok(value),
        Err(Abort::Error(error)) => Err(error),
        Err(Abort::Cancelled(_)) => {
            unreachable!("creating a closure does not poll cancellation")
        }
    }
}

/// Applies a runtime callable to already-evaluated argument values.
///
/// This is the closure-invocation seam the structured closure commands (`each`,
/// `where`) drive: it takes a callable [`Value`] and a vector of argument values
/// rather than a call-expression AST, so a stream transformer can apply a closure
/// per item without synthesizing syntax. `span` attributes the synthetic call site
/// — the driving command's span — to a cancellation and to an arity or
/// not-callable diagnostic; errors raised inside the body keep their own body
/// spans. `env` is shared so a closure observes and mutates the same environment,
/// and `limits` supplies the cancellation token and a fresh resource budget per
/// application.
///
/// A tripped cancellation token yields [`Completion::Cancelled`] rather than a
/// [`RuntimeError`], mirroring [`evaluate_with_limits`]. A non-callable value or an
/// arity mismatch is a [`RuntimeError`] at `span`.
pub fn apply_callable(
    callable: &Value,
    arguments: Vec<Value>,
    source: &SourceFile,
    span: Span,
    env: &mut Environment,
    limits: &EvalLimits,
) -> Result<Completion, RuntimeError> {
    let Value::Callable(callable) = callable else {
        return Err(RuntimeError::new(
            RuntimeErrorKind::NotCallable {
                actual: callable.family_name(),
            },
            span,
        ));
    };
    let function = callable
        .as_any()
        .downcast_ref::<CallableValue>()
        .expect("every runtime callable is a CallableValue");
    if arguments.len() != function.parameters.len() {
        return Err(RuntimeError::new(
            RuntimeErrorKind::ArityMismatch {
                expected: function.parameters.len(),
                actual: arguments.len(),
            },
            span,
        ));
    }

    let mut evaluator = Evaluator {
        source,
        cancel: limits.cancel.clone(),
        budget: limits.budget,
        policy: limits.policy,
        env,
    };
    // Cancellation is polled before entering the body, matching an ordinary call.
    if let Err(abort) = evaluator.check_cancel(span) {
        return match abort {
            Abort::Cancelled(cancellation) => Ok(Completion::Cancelled(cancellation)),
            Abort::Error(error) => Err(error),
        };
    }
    match evaluator.run_call(callable, function, arguments, span) {
        Ok(value) => Ok(Completion::Value(value)),
        Err(Abort::Cancelled(cancellation)) => Ok(Completion::Cancelled(cancellation)),
        Err(Abort::Error(error)) => Err(error),
    }
}

/// One ordinary command word expanded to a single platform-native argument.
///
/// `value` is the concatenation of every part's native units in source order; on
/// Unix, text parts contribute their UTF-8 bytes and path parts their exact
/// native bytes. `span` is the whole word and `parts` records the span of every
/// source part that contributed at least one native unit, so a later
/// platform-conversion error can point at the offending part rather than the
/// entire word. An empty quoted part still forms a word but contributes no unit
/// and therefore no provenance entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpandedWord {
    value: OsString,
    span: Span,
    parts: Vec<Span>,
}

impl ExpandedWord {
    /// Build one runtime-supplied argument with source provenance.
    pub(crate) fn synthetic(value: OsString, span: Span) -> Self {
        Self {
            value,
            span,
            parts: vec![span],
        }
    }

    /// The concatenated native argument.
    #[must_use]
    pub fn value(&self) -> &OsStr {
        &self.value
    }

    /// The whole-word source span.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    /// The spans of the parts that contributed native units, in source order.
    #[must_use]
    pub fn parts(&self) -> &[Span] {
        &self.parts
    }
}

/// Expands one ordinary command word into a single native argument.
///
/// Bare, single-quoted, and double-quoted parts contribute their decoded text;
/// each `$name` or `${expression}` part is evaluated once and must produce a
/// word-eligible scalar encoded with its canonical word encoding. An ineligible
/// value, an unknown binding, or a deferred part is a [`RuntimeError`]. Spread,
/// command substitution, NUL rejection, and argv planning belong to later slices.
pub fn expand_word(
    word: &Word,
    source: &SourceFile,
    scope: &mut ScopeStack,
) -> Result<ExpandedWord, RuntimeError> {
    let limits = EvalLimits::default();
    // Word expansion never touches the environment; a throwaway is sufficient.
    let mut env = Environment::new();
    let mut evaluator = Evaluator {
        source,
        cancel: limits.cancel.clone(),
        budget: limits.budget,
        policy: limits.policy,
        env: &mut env,
    };
    match evaluator.expand_word(word, scope) {
        Ok(expanded) => Ok(expanded),
        Err(Abort::Error(error)) => Err(error),
        Err(Abort::Cancelled(_)) => {
            unreachable!("a never-cancelling token cannot produce a cancellation")
        }
    }
}

/// Expands a standalone `...$name` spread item into zero or more command
/// arguments.
///
/// `variable` is the spread's binding and `item_span` is the whole `...$name`
/// source item, used to anchor diagnostics. The binding is read once and must
/// hold a finite `List`; each element is validated in list order against the
/// word-eligible scalar families and encoded with its canonical word encoding.
/// A non-list value, an ineligible element, or an unknown binding is a
/// [`RuntimeError`]. Spread never recursively flattens a nested list.
pub fn expand_spread(
    variable: &VariableReference,
    item_span: Span,
    source: &SourceFile,
    scope: &mut ScopeStack,
) -> Result<Vec<ExpandedWord>, RuntimeError> {
    let limits = EvalLimits::default();
    // Spread expansion never touches the environment; a throwaway is sufficient.
    let mut env = Environment::new();
    let mut evaluator = Evaluator {
        source,
        cancel: limits.cancel.clone(),
        budget: limits.budget,
        policy: limits.policy,
        env: &mut env,
    };
    match evaluator.expand_spread(variable, item_span, scope) {
        Ok(expanded) => Ok(expanded),
        Err(Abort::Error(error)) => Err(error),
        Err(Abort::Cancelled(_)) => {
            unreachable!("a never-cancelling token cannot produce a cancellation")
        }
    }
}

/// Encodes a word-eligible scalar with its canonical word encoding, or returns
/// `None` for an ineligible value family. Callers attach the span-appropriate
/// diagnostic for their position (ordinary word versus spread element).
fn word_encoding(value: &Value) -> Option<OsString> {
    match value {
        Value::Bool(flag) => Some(OsString::from(if *flag { "true" } else { "false" })),
        Value::Int(integer) => Some(OsString::from(integer.to_string())),
        Value::Float(float) => Some(OsString::from(float.to_string())),
        Value::String(text) => Some(OsString::from(text.as_ref())),
        Value::Path(path) => Some(path.as_os_str().to_os_string()),
        Value::Duration(duration) => Some(OsString::from(duration.to_string())),
        Value::ByteSize(size) => Some(OsString::from(size.to_string())),
        _ => None,
    }
}

/// The control-flow result of evaluating a statement or statement sequence.
enum Flow {
    /// Continue with the current frame; the value is present for expressions.
    Fallthrough(Option<Value>),
    /// A `break` originating at the given span is propagating outward.
    Break(Span),
    /// A `continue` originating at the given span is propagating outward.
    Continue(Span),
    /// A `return` carrying its value is propagating outward to the function.
    Return(Value, Span),
}

struct Evaluator<'source> {
    source: &'source SourceFile,
    cancel: CancellationToken,
    budget: ResourceBudget,
    policy: EvaluationPolicy,
    env: &'source mut Environment,
}

impl<'source> Evaluator<'source> {
    /// Aborts with a cancellation when the token trips at a boundary.
    fn check_cancel(&self, span: Span) -> Eval<()> {
        if self.cancel.is_cancelled() {
            Err(Abort::Cancelled(Cancellation::new(
                self.cancel.reason(),
                span,
            )))
        } else {
            Ok(())
        }
    }

    /// Charges one evaluation step, aborting when the resource budget is spent.
    fn charge(&mut self, span: Span) -> Eval<()> {
        if self.budget.charge() {
            Ok(())
        } else {
            Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))
        }
    }

    fn statement(&mut self, statement: &Statement, scope: &mut ScopeStack) -> Eval<Flow> {
        let span = statement.span();
        self.charge(span)?;
        match statement.kind() {
            StatementKind::Declaration(declaration) => {
                self.declaration(declaration, scope)?;
                Ok(Flow::Fallthrough(None))
            }
            StatementKind::Assignment(assignment) => {
                self.assignment(assignment, scope)?;
                Ok(Flow::Fallthrough(None))
            }
            StatementKind::Environment(environment) => {
                self.environment(environment, scope)?;
                Ok(Flow::Fallthrough(None))
            }
            StatementKind::Function(definition) => {
                self.function_definition(definition, scope)?;
                Ok(Flow::Fallthrough(None))
            }
            StatementKind::If(if_statement) => self.if_statement(if_statement, scope),
            StatementKind::While(while_statement) => self.while_statement(while_statement, scope),
            StatementKind::For(for_statement) => self.for_statement(for_statement, scope),
            StatementKind::Match(match_statement) => self.match_statement(match_statement, scope),
            StatementKind::Control(control) => self.control(control, scope, span),
            StatementKind::Job(job) => {
                if self.policy == EvaluationPolicy::Startup {
                    return Err(self.error(
                        RuntimeErrorKind::RestrictedStartup {
                            capability: RestrictedCapability::ProcessExecution,
                        },
                        span,
                    ));
                }
                if job.background_span.is_some() {
                    return Err(self.error(RuntimeErrorKind::ExecutionUnsupported, span));
                }
                let value = self.eval_chain(&job.chain, scope)?;
                Ok(Flow::Fallthrough(Some(value)))
            }
        }
    }

    fn declaration(&mut self, declaration: &Declaration, scope: &mut ScopeStack) -> Eval<()> {
        let value = self.expression(&declaration.value, scope)?;
        let name = self.text(declaration.name.span());
        let mutability = if declaration.mutable {
            BindingMutability::Mutable
        } else {
            BindingMutability::Immutable
        };
        scope
            .declare(name, mutability, value)
            .map_err(|error| self.error(RuntimeErrorKind::Scope(error), declaration.name.span()))
    }

    fn assignment(&mut self, assignment: &Assignment, scope: &mut ScopeStack) -> Eval<()> {
        let value = self.expression(&assignment.value, scope)?;
        let name = self.text(assignment.target.name.span());
        scope
            .assign(name, value)
            .map_err(|error| self.error(RuntimeErrorKind::Scope(error), assignment.target.span))
    }

    /// Applies an `export` or `unset` to the environment. `export` encodes its
    /// value with the canonical word encoding; `unset` removes the name and is a
    /// no-op when absent. Neither creates or removes a lexical binding.
    fn environment(
        &mut self,
        environment: &EnvironmentStatement,
        scope: &mut ScopeStack,
    ) -> Eval<()> {
        match environment {
            EnvironmentStatement::Export { name, value } => {
                let resolved = self.expression(value, scope)?;
                let encoded = word_encoding(&resolved).ok_or_else(|| {
                    self.error(
                        RuntimeErrorKind::ExportValueNotEligible {
                            actual: resolved.family_name(),
                        },
                        value.span(),
                    )
                })?;
                let name = self.text(name.span()).to_owned();
                self.env.set(name, encoded);
            }
            EnvironmentStatement::Unset { name } => {
                let name = self.text(name.span());
                self.env.remove(name);
            }
        }
        Ok(())
    }

    fn if_statement(&mut self, statement: &IfStatement, scope: &mut ScopeStack) -> Eval<Flow> {
        if self.condition(&statement.condition, scope)? {
            return self.block(&statement.then_block, scope);
        }
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => self.block(block, scope),
            Some(ElseBranch::If(nested)) => self.if_statement(nested.kind(), scope),
            None => Ok(Flow::Fallthrough(None)),
        }
    }

    fn while_statement(
        &mut self,
        statement: &WhileStatement,
        scope: &mut ScopeStack,
    ) -> Eval<Flow> {
        // Cancellation is polled before each loop condition, so an otherwise
        // unbounded loop stops cooperatively at its next boundary.
        loop {
            self.check_cancel(statement.condition.span())?;
            if !self.condition(&statement.condition, scope)? {
                break;
            }
            match self.block(&statement.body, scope)? {
                Flow::Break(_) => break,
                Flow::Continue(_) | Flow::Fallthrough(_) => {}
                transfer @ Flow::Return(..) => return Ok(transfer),
            }
        }
        Ok(Flow::Fallthrough(None))
    }

    fn for_statement(&mut self, statement: &ForStatement, scope: &mut ScopeStack) -> Eval<Flow> {
        let iterable = self.expression(&statement.iterable, scope)?;
        let name = self.text(statement.binding.span());
        let boundary = statement.iterable.span();
        match iterable {
            Value::List(items) => {
                for item in items.iter() {
                    self.check_cancel(boundary)?;
                    match self.iteration(name, item.clone(), &statement.body, scope)? {
                        Flow::Break(_) => break,
                        Flow::Continue(_) | Flow::Fallthrough(_) => {}
                        transfer @ Flow::Return(..) => return Ok(transfer),
                    }
                }
            }
            Value::Range(range) => {
                let mut current = range.start();
                while range.contains(current) {
                    self.check_cancel(boundary)?;
                    match self.iteration(name, Value::Int(current), &statement.body, scope)? {
                        Flow::Break(_) => break,
                        Flow::Continue(_) | Flow::Fallthrough(_) => {}
                        transfer @ Flow::Return(..) => return Ok(transfer),
                    }
                    match current.checked_add(1) {
                        Some(next) => current = next,
                        None => break,
                    }
                }
            }
            other => {
                return Err(self.error(
                    RuntimeErrorKind::NotIterable {
                        actual: other.family_name(),
                    },
                    statement.iterable.span(),
                ));
            }
        }
        Ok(Flow::Fallthrough(None))
    }

    /// Evaluates a `match`: the scrutinee is evaluated once, then arms are tried
    /// in source order. The first arm whose pattern matches and whose guard (if
    /// any) is `true` runs its block; no later arm is tried. A `match` that
    /// matches no arm is a runtime error, keeping non-exhaustive matches loud.
    fn match_statement(
        &mut self,
        statement: &MatchStatement,
        scope: &mut ScopeStack,
    ) -> Eval<Flow> {
        let subject = self.expression(&statement.value, scope)?;
        for arm in &statement.arms {
            scope.push();
            let outcome = self.arm(arm, &subject, scope);
            scope.pop().expect("an arm pushes exactly one frame");
            if let Some(flow) = outcome? {
                return Ok(flow);
            }
        }
        Err(self.error(RuntimeErrorKind::NoMatchingArm, statement.value.span()))
    }

    /// Tries one arm against the subject in the already-pushed arm frame.
    ///
    /// Returns `Ok(Some(flow))` when the arm is selected and its block runs,
    /// `Ok(None)` when the pattern or guard rejects the subject, or an error.
    fn arm(
        &mut self,
        arm: &MatchArm,
        subject: &Value,
        scope: &mut ScopeStack,
    ) -> Eval<Option<Flow>> {
        if !self.pattern_matches(&arm.pattern, subject, scope)? {
            return Ok(None);
        }
        if let Some(guard) = &arm.guard {
            let value = self.expression(guard, scope)?;
            if !self.expect_condition(&value, guard.span())? {
                return Ok(None);
            }
        }
        self.statements(&arm.body.statements, scope).map(Some)
    }

    /// Decides whether a pattern matches the subject, binding an identifier
    /// pattern as a fresh immutable cell in the current arm frame.
    fn pattern_matches(
        &mut self,
        pattern: &Pattern,
        subject: &Value,
        scope: &mut ScopeStack,
    ) -> Eval<bool> {
        match pattern {
            Pattern::Wildcard(_) => Ok(true),
            Pattern::Literal(literal) => {
                let expected = self.literal(literal)?;
                Ok(&expected == subject)
            }
            Pattern::Binding(identifier) => {
                let name = self.text(identifier.span());
                scope
                    .declare(name, BindingMutability::Immutable, subject.clone())
                    .map_err(|error| {
                        self.error(RuntimeErrorKind::Scope(error), identifier.span())
                    })?;
                Ok(true)
            }
        }
    }

    /// Runs one loop iteration in a fresh frame holding the immutable loop value.
    fn iteration(
        &mut self,
        name: &str,
        value: Value,
        body: &Block,
        scope: &mut ScopeStack,
    ) -> Eval<Flow> {
        scope.push();
        let outcome = (|| {
            scope
                .declare(name, BindingMutability::Immutable, value)
                .map_err(|error| self.error(RuntimeErrorKind::Scope(error), body.span))?;
            self.statements(&body.statements, scope)
        })();
        scope.pop().expect("iteration pushes exactly one frame");
        outcome
    }

    fn block(&mut self, block: &Block, scope: &mut ScopeStack) -> Eval<Flow> {
        scope.push();
        let outcome = self.statements(&block.statements, scope);
        scope.pop().expect("a block pushes exactly one frame");
        outcome
    }

    /// Evaluates statements in the current frame, stopping at a loop transfer.
    fn statements(&mut self, statements: &[Statement], scope: &mut ScopeStack) -> Eval<Flow> {
        for statement in statements {
            match self.statement(statement, scope)? {
                Flow::Fallthrough(_) => {}
                transfer => return Ok(transfer),
            }
        }
        Ok(Flow::Fallthrough(None))
    }

    fn control(
        &mut self,
        control: &ControlTransfer,
        scope: &mut ScopeStack,
        span: Span,
    ) -> Eval<Flow> {
        match control {
            ControlTransfer::Break => Ok(Flow::Break(span)),
            ControlTransfer::Continue => Ok(Flow::Continue(span)),
            ControlTransfer::Return(expression) => {
                let value = match expression {
                    Some(expression) => self.expression(expression, scope)?,
                    None => Value::Null,
                };
                Ok(Flow::Return(value, span))
            }
        }
    }

    fn condition(&mut self, chain: &ConditionalChain, scope: &mut ScopeStack) -> Eval<bool> {
        let value = self.eval_chain(chain, scope)?;
        self.expect_condition(&value, chain.span())
    }

    /// Evaluates a conditional chain to a value.
    ///
    /// A single-term chain is transparent. Multiple `||` terms return the last
    /// evaluated operand and branch over either `Bool` or `Status`.
    fn eval_chain(&mut self, chain: &ConditionalChain, scope: &mut ScopeStack) -> Eval<Value> {
        let mut terms = chain.or_terms().iter();
        let first = terms
            .next()
            .expect("a parsed conditional chain contains an operand");
        let mut value = self.eval_and_chain(first, scope)?;
        let mut value_span = first.span();
        for and_chain in terms {
            if self.expect_logic_condition(&value, value_span)? {
                return Ok(value);
            }
            value = self.eval_and_chain(and_chain, scope)?;
            value_span = and_chain.span();
        }
        if chain.or_terms().len() > 1 {
            self.expect_logic_condition(&value, value_span)?;
        }
        Ok(value)
    }

    /// Evaluates one `&&` chain and returns its last evaluated operand.
    fn eval_and_chain(&mut self, and_chain: &AndChain, scope: &mut ScopeStack) -> Eval<Value> {
        let mut terms = and_chain.and_terms().iter();
        let first = terms
            .next()
            .expect("a parsed and-chain contains an operand");
        let mut value = self.eval_pipeline(first, scope)?;
        let mut value_span = first.span();
        for pipeline in terms {
            if !self.expect_logic_condition(&value, value_span)? {
                return Ok(value);
            }
            value = self.eval_pipeline(pipeline, scope)?;
            value_span = pipeline.span();
        }
        if and_chain.and_terms().len() > 1 {
            self.expect_logic_condition(&value, value_span)?;
        }
        Ok(value)
    }

    /// Evaluates a single-stage expression pipeline. A multi-stage pipeline or a
    /// command stage needs process execution and is unsupported here.
    fn eval_pipeline(&mut self, pipeline: &Pipeline, scope: &mut ScopeStack) -> Eval<Value> {
        let [stage] = pipeline.stages() else {
            if self.policy == EvaluationPolicy::Startup {
                return Err(self.error(
                    RuntimeErrorKind::RestrictedStartup {
                        capability: RestrictedCapability::ProcessExecution,
                    },
                    pipeline.span(),
                ));
            }
            return Err(self.error(RuntimeErrorKind::ExecutionUnsupported, pipeline.span()));
        };
        match stage.kind() {
            StageKind::Expression(expression) => self.expression(expression, scope),
            StageKind::Command(_) if self.policy == EvaluationPolicy::Startup => Err(self.error(
                RuntimeErrorKind::RestrictedStartup {
                    capability: RestrictedCapability::ProcessExecution,
                },
                pipeline.span(),
            )),
            StageKind::Command(_) => {
                Err(self.error(RuntimeErrorKind::ExecutionUnsupported, pipeline.span()))
            }
        }
    }

    /// Requires a value to be `Bool` or `Status` at a conditional-chain edge.
    fn expect_logic_condition(&self, value: &Value, span: Span) -> Eval<bool> {
        match value {
            Value::Bool(boolean) => Ok(*boolean),
            Value::Status(status) => Ok(status.is_ok()),
            other => Err(self.error(
                RuntimeErrorKind::ConditionalOperandNotBoolOrStatus {
                    actual: other.family_name(),
                },
                span,
            )),
        }
    }

    /// Requires a `Bool` for unary logical negation.
    fn expect_logic_bool(&self, value: &Value, span: Span) -> Eval<bool> {
        match value {
            Value::Bool(boolean) => Ok(*boolean),
            other => Err(self.error(
                RuntimeErrorKind::LogicOperandNotBool {
                    actual: other.family_name(),
                },
                span,
            )),
        }
    }

    /// Requires a value to be `Bool` or `Status` at a language condition.
    fn expect_condition(&self, value: &Value, span: Span) -> Eval<bool> {
        match value {
            Value::Bool(boolean) => Ok(*boolean),
            Value::Status(status) => Ok(status.is_ok()),
            other => Err(self.error(
                RuntimeErrorKind::ConditionNotBool {
                    actual: other.family_name(),
                },
                span,
            )),
        }
    }

    fn expression(&mut self, expression: &Expression, scope: &mut ScopeStack) -> Eval<Value> {
        let span = expression.span();
        self.charge(span)?;
        match expression.kind() {
            ExpressionKind::Literal(literal) => self.literal(literal),
            ExpressionKind::Variable(variable) => {
                let name = self.text(variable.name.span());
                scope.get(name).cloned().ok_or_else(|| {
                    self.error(
                        RuntimeErrorKind::Scope(ScopeError::UnknownBinding(name.to_owned())),
                        span,
                    )
                })
            }
            ExpressionKind::Symbol(_) => Err(self.unsupported("bare symbol", span)),
            ExpressionKind::List(elements) => {
                let mut values = Vec::with_capacity(elements.len());
                for element in elements {
                    values.push(self.expression(element, scope)?);
                }
                Ok(Value::list(values))
            }
            ExpressionKind::Record(entries) => {
                let mut pairs = Vec::with_capacity(entries.len());
                for entry in entries {
                    let key = self.record_key(&entry.key)?;
                    let value = self.expression(&entry.value, scope)?;
                    pairs.push((key, value));
                }
                Record::new(pairs).map(Value::from).map_err(|error| {
                    self.error(
                        RuntimeErrorKind::DuplicateRecordKey {
                            key: error.key().to_owned(),
                        },
                        span,
                    )
                })
            }
            ExpressionKind::Closure(closure) => self.make_closure(closure, scope),
            ExpressionKind::CommandSubstitution(_) | ExpressionKind::GroupedJob(_) => {
                self.grouped_or_substitution(expression, scope)
            }
            ExpressionKind::Call(call) => self.call(call, scope, span),
            ExpressionKind::Index(index) => {
                let target = self.expression(&index.target, scope)?;
                let position = self.expression(&index.index, scope)?;
                operation::index(&target, &position).map_err(|error| self.operation(error, span))
            }
            ExpressionKind::Member(member) => {
                let target = self.expression(&member.target, scope)?;
                let name = self.text(member.member.span());
                operation::field(&target, name).map_err(|error| self.operation(error, span))
            }
            ExpressionKind::Unary(unary) => {
                let operand = self.expression(&unary.operand, scope)?;
                let result = match unary.operator.kind() {
                    UnaryOperator::Not => {
                        return Ok(Value::Bool(!self.expect_logic_bool(&operand, span)?));
                    }
                    UnaryOperator::Positive => operation::plus(&operand),
                    UnaryOperator::Negative => operation::negate(&operand),
                };
                result.map_err(|error| self.operation(error, span))
            }
            ExpressionKind::Binary(binary) => {
                let left = self.expression(&binary.left, scope)?;
                let right = self.expression(&binary.right, scope)?;
                self.binary(*binary.operator.kind(), &left, &right, span)
            }
        }
    }

    fn grouped_or_substitution(
        &mut self,
        expression: &Expression,
        scope: &mut ScopeStack,
    ) -> Eval<Value> {
        let span = expression.span();
        match expression.kind() {
            // A parenthesized pure expression is evaluated; a grouped command is not.
            ExpressionKind::GroupedJob(chain) => self.eval_chain(chain, scope),
            ExpressionKind::CommandSubstitution(_) if self.policy == EvaluationPolicy::Startup => {
                Err(self.error(
                    RuntimeErrorKind::RestrictedStartup {
                        capability: RestrictedCapability::CommandSubstitution,
                    },
                    span,
                ))
            }
            ExpressionKind::CommandSubstitution(_) => {
                Err(self.error(RuntimeErrorKind::ExecutionUnsupported, span))
            }
            _ => unreachable!("caller restricts this to grouped jobs and substitutions"),
        }
    }

    fn binary(
        &self,
        operator: BinaryOperator,
        left: &Value,
        right: &Value,
        span: Span,
    ) -> Eval<Value> {
        let map = |result: Result<Value, OperationError>| {
            result.map_err(|error| self.operation(error, span))
        };
        match operator {
            BinaryOperator::Add => map(operation::add(left, right)),
            BinaryOperator::Subtract => map(operation::subtract(left, right)),
            BinaryOperator::Multiply => map(operation::multiply(left, right)),
            BinaryOperator::Divide => map(operation::divide(left, right)),
            BinaryOperator::Remainder => map(operation::remainder(left, right)),
            BinaryOperator::Less => map(operation::less(left, right)),
            BinaryOperator::LessEqual => map(operation::less_equal(left, right)),
            BinaryOperator::Greater => map(operation::greater(left, right)),
            BinaryOperator::GreaterEqual => map(operation::greater_equal(left, right)),
            BinaryOperator::Equal => Ok(operation::equal(left, right)),
            BinaryOperator::NotEqual => Ok(operation::not_equal(left, right)),
            BinaryOperator::In => map(operation::member(left, right)),
            BinaryOperator::Range => map(operation::range(left, right, false)),
            BinaryOperator::RangeInclusive => map(operation::range(left, right, true)),
        }
    }

    fn literal(&self, literal: &Literal) -> Eval<Value> {
        let span = literal.span();
        match literal.kind() {
            LiteralKind::Null => Ok(Value::Null),
            LiteralKind::Boolean(value) => Ok(Value::Bool(*value)),
            LiteralKind::Integer => self.integer(span),
            LiteralKind::Float => self.float(span),
            LiteralKind::SingleQuoted => {
                let raw = self.text(span);
                // The span includes both surrounding single quotes; content is exact.
                let inner = &raw[1..raw.len() - 1];
                Ok(Value::string(inner))
            }
            LiteralKind::DoubleQuoted(_) => Err(self.unsupported("double-quoted string", span)),
        }
    }

    fn integer(&self, span: Span) -> Eval<Value> {
        let raw = self.text(span);
        let parsed = if let Some(hex) = strip_base(raw, "0x", "0X") {
            i64::from_str_radix(hex, 16)
        } else if let Some(octal) = strip_base(raw, "0o", "0O") {
            i64::from_str_radix(octal, 8)
        } else if let Some(binary) = strip_base(raw, "0b", "0B") {
            i64::from_str_radix(binary, 2)
        } else {
            raw.parse::<i64>()
        };
        parsed
            .map(Value::Int)
            .map_err(|_| self.error(RuntimeErrorKind::IntegerLiteralOverflow, span))
    }

    fn float(&self, span: Span) -> Eval<Value> {
        let raw = self.text(span);
        raw.parse::<f64>()
            .ok()
            .and_then(|value| crate::FiniteFloat::new(value).ok())
            .map(Value::from)
            .ok_or_else(|| self.error(RuntimeErrorKind::FloatLiteralOverflow, span))
    }

    fn record_key(&self, key: &RecordKey) -> Eval<String> {
        match key {
            RecordKey::Identifier(identifier) => Ok(self.text(identifier.span()).to_owned()),
            RecordKey::SingleQuoted(span) => {
                let raw = self.text(*span);
                Ok(raw[1..raw.len() - 1].to_owned())
            }
            RecordKey::DoubleQuoted(part) => {
                Err(self.unsupported("double-quoted key", part.span()))
            }
        }
    }

    /// Expands one ordinary word into a single native argument.
    fn expand_word(&mut self, word: &Word, scope: &mut ScopeStack) -> Eval<ExpandedWord> {
        let mut value = OsString::new();
        let mut parts = Vec::new();
        for part in word.parts() {
            self.expand_part(part, scope, &mut value, &mut parts)?;
        }
        Ok(ExpandedWord {
            value,
            span: word.span(),
            parts,
        })
    }

    /// Appends one part's native units to `value`, recording its span in `parts`
    /// when it contributed at least one unit.
    fn expand_part(
        &mut self,
        part: &WordPart,
        scope: &mut ScopeStack,
        value: &mut OsString,
        parts: &mut Vec<Span>,
    ) -> Eval<()> {
        let span = part.span();
        self.charge(span)?;

        // A double-quoted part is a container; its inner parts record their own
        // provenance, so it never contributes a wrapper span of its own.
        if let WordPartKind::DoubleQuoted(inner) = part.kind() {
            for inner_part in inner {
                self.expand_part(inner_part, scope, value, parts)?;
            }
            return Ok(());
        }

        let before = value.len();
        match part.kind() {
            WordPartKind::Bare | WordPartKind::DoubleText => value.push(self.text(span)),
            WordPartKind::SingleQuoted => {
                // The span includes both single quotes; the content is exact.
                let raw = self.text(span);
                value.push(&raw[1..raw.len() - 1]);
            }
            WordPartKind::BareEscape => {
                // A bare backslash quotes exactly the next scalar literally.
                let raw = self.text(span);
                value.push(&raw[1..]);
            }
            WordPartKind::DoubleEscape => {
                value.push(decode_double_escape(self.text(span)));
            }
            WordPartKind::Variable(identifier) => {
                let name = self.text(identifier.span());
                let resolved = scope.get(name).cloned().ok_or_else(|| {
                    self.error(
                        RuntimeErrorKind::Scope(ScopeError::UnknownBinding(name.to_owned())),
                        span,
                    )
                })?;
                value.push(self.encode_scalar(&resolved, span)?);
            }
            WordPartKind::BracedInterpolation(expression) => {
                let resolved = self.expression(expression, scope)?;
                value.push(self.encode_scalar(&resolved, span)?);
            }
            WordPartKind::CommandSubstitution(_) => {
                if self.policy == EvaluationPolicy::Startup {
                    return Err(self.error(
                        RuntimeErrorKind::RestrictedStartup {
                            capability: RestrictedCapability::CommandSubstitution,
                        },
                        span,
                    ));
                }
                return Err(self.unsupported("command substitution in a word", span));
            }
            WordPartKind::DoubleQuoted(_) => unreachable!("handled before provenance tracking"),
        }

        if value.len() != before {
            parts.push(span);
        }
        Ok(())
    }

    /// Encodes a word-eligible scalar with its canonical word encoding. Ineligible
    /// families are a [`RuntimeErrorKind::WordValueNotWordEligible`] error at `span`.
    fn encode_scalar(&self, value: &Value, span: Span) -> Eval<OsString> {
        word_encoding(value).ok_or_else(|| {
            self.error(
                RuntimeErrorKind::WordValueNotWordEligible {
                    actual: value.family_name(),
                },
                span,
            )
        })
    }

    /// Expands a `...$name` spread into zero or more native arguments. The binding
    /// is read once and must hold a `List`; each element is encoded with its
    /// canonical word encoding in list order. Diagnostics anchor on `item_span`.
    fn expand_spread(
        &mut self,
        variable: &VariableReference,
        item_span: Span,
        scope: &mut ScopeStack,
    ) -> Eval<Vec<ExpandedWord>> {
        self.charge(item_span)?;
        let name = self.text(variable.name.span());
        let resolved = scope.get(name).cloned().ok_or_else(|| {
            self.error(
                RuntimeErrorKind::Scope(ScopeError::UnknownBinding(name.to_owned())),
                item_span,
            )
        })?;
        let elements = match resolved {
            Value::List(elements) => elements,
            other => {
                return Err(self.error(
                    RuntimeErrorKind::SpreadValueNotList {
                        actual: other.family_name(),
                    },
                    item_span,
                ));
            }
        };
        let mut words = Vec::with_capacity(elements.len());
        for (index, element) in elements.iter().enumerate() {
            self.charge(item_span)?;
            let value = word_encoding(element).ok_or_else(|| {
                self.error(
                    RuntimeErrorKind::SpreadElementNotWordEligible {
                        index,
                        actual: element.family_name(),
                    },
                    item_span,
                )
            })?;
            // An empty element still forms an argument, but like an empty word
            // part it contributes no native units and therefore no provenance.
            let parts = if value.is_empty() {
                Vec::new()
            } else {
                vec![item_span]
            };
            words.push(ExpandedWord {
                value,
                span: item_span,
                parts,
            });
        }
        Ok(words)
    }

    fn text(&self, span: Span) -> &'source str {
        self.source
            .slice(span)
            .expect("ast spans always address their own source")
    }

    /// Builds a runtime-error abort anchored at `span`.
    fn error(&self, kind: RuntimeErrorKind, span: Span) -> Abort {
        Abort::Error(RuntimeError::new(kind, span))
    }

    fn unsupported(&self, feature: &'static str, span: Span) -> Abort {
        self.error(RuntimeErrorKind::Unsupported { feature }, span)
    }

    fn operation(&self, error: OperationError, span: Span) -> Abort {
        self.error(RuntimeErrorKind::Operation(error), span)
    }

    fn function_definition(
        &self,
        definition: &FunctionDefinition,
        scope: &mut ScopeStack,
    ) -> Eval<()> {
        let name: Arc<str> = Arc::from(self.text(definition.name.span()));
        let parameters = self.parameters(&definition.parameters)?;
        let callable = CallableValue {
            name: Some(Arc::clone(&name)),
            parameters,
            body: CallableBody::Block(definition.body.clone()),
            captured: scope.captured_snapshot(),
            location: self.location(definition.name.span()),
        };
        let value = Value::Callable(Arc::new(callable));
        scope
            .declare(name.as_ref(), BindingMutability::Immutable, value)
            .map_err(|error| self.error(RuntimeErrorKind::Scope(error), definition.name.span()))
    }

    fn make_closure(&self, closure: &Closure, scope: &ScopeStack) -> Eval<Value> {
        let parameters = self.parameters(&closure.parameters)?;
        let callable = CallableValue {
            name: None,
            parameters,
            body: CallableBody::Expression(closure.body.clone()),
            captured: scope.captured_snapshot(),
            location: self.location(closure.span),
        };
        Ok(Value::Callable(Arc::new(callable)))
    }

    /// Collects parameter names, rejecting a repeated name at creation time.
    fn parameters(&self, parameters: &[Parameter]) -> Eval<Vec<Arc<str>>> {
        let mut names: Vec<Arc<str>> = Vec::with_capacity(parameters.len());
        for parameter in parameters {
            let name = self.text(parameter.name.span());
            if names.iter().any(|existing| existing.as_ref() == name) {
                return Err(self.error(
                    RuntimeErrorKind::DuplicateParameter {
                        name: name.to_owned(),
                    },
                    parameter.span,
                ));
            }
            names.push(Arc::from(name));
        }
        Ok(names)
    }

    fn call(&mut self, call: &CallExpression, scope: &mut ScopeStack, span: Span) -> Eval<Value> {
        // Cancellation is polled before entering any call.
        self.check_cancel(span)?;

        let callee = self.callee_value(&call.callee, scope)?;
        let Value::Callable(callable) = callee else {
            return Err(self.error(
                RuntimeErrorKind::NotCallable {
                    actual: callee.family_name(),
                },
                call.callee.span(),
            ));
        };
        let function = callable
            .as_any()
            .downcast_ref::<CallableValue>()
            .expect("every runtime callable is a CallableValue");

        if call.arguments.len() != function.parameters.len() {
            return Err(self.error(
                RuntimeErrorKind::ArityMismatch {
                    expected: function.parameters.len(),
                    actual: call.arguments.len(),
                },
                span,
            ));
        }

        let mut arguments = Vec::with_capacity(call.arguments.len());
        for argument in &call.arguments {
            arguments.push(self.expression(argument, scope)?);
        }

        self.run_call(&callable, function, arguments, span)
    }

    /// Binds already-evaluated `arguments` to a callable's parameters and runs its
    /// body, recording a call frame at `span` on any unwinding error.
    ///
    /// The caller has already resolved the callee, checked it is callable, and
    /// matched its arity; this is the shared body used by an ordinary call
    /// expression and by [`apply_callable`], which applies a callable to runtime
    /// values with no call-expression AST.
    fn run_call(
        &mut self,
        callable: &Arc<dyn Callable>,
        function: &CallableValue,
        arguments: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        // The captured snapshot underlies a fresh self frame (recursion) and a
        // fresh parameter frame, so parameters shadow captured names by ordinary
        // nearest-lexical lookup.
        let mut call_scope = function.captured.clone();
        call_scope.push();
        if let Some(name) = &function.name {
            call_scope
                .declare(
                    name.as_ref(),
                    BindingMutability::Immutable,
                    Value::Callable(Arc::clone(callable)),
                )
                .expect("a fresh frame cannot already hold the function name");
        }
        call_scope.push();
        for (name, value) in function.parameters.iter().zip(arguments) {
            call_scope
                .declare(name.as_ref(), BindingMutability::Mutable, value)
                .expect("parameter names are unique by construction");
        }

        // The body is entered here, so any error unwinding out of it records a
        // call frame naming this callee and its call site. Everything above
        // (cancellation, argument, arity, not-callable resolution) ran in the
        // caller's context and is deliberately left unframed.
        let frame = CallFrame::new(function.name.as_deref(), span);
        self.run_body(function, &mut call_scope)
            .map_err(|abort| abort.with_frame(frame))
    }

    /// Runs an already-prepared call body, reducing its control flow to a value.
    fn run_body(&mut self, function: &CallableValue, scope: &mut ScopeStack) -> Eval<Value> {
        let outcome = match &function.body {
            CallableBody::Block(block) => {
                scope.push();
                let flow = self.body_statements(&block.statements, scope);
                scope.pop().expect("the body pushes exactly one frame");
                flow?
            }
            CallableBody::Expression(chain) => {
                Flow::Fallthrough(Some(self.eval_chain(chain, scope)?))
            }
        };

        match outcome {
            Flow::Return(value, _) | Flow::Fallthrough(Some(value)) => Ok(value),
            Flow::Fallthrough(None) => Ok(Value::Null),
            Flow::Break(span) => Err(self.error(
                RuntimeErrorKind::ControlOutsideLoop {
                    control: ControlKind::Break,
                },
                span,
            )),
            Flow::Continue(span) => Err(self.error(
                RuntimeErrorKind::ControlOutsideLoop {
                    control: ControlKind::Continue,
                },
                span,
            )),
        }
    }

    /// Resolves a callee. A bare name resolves in scope; any other form is an
    /// ordinary expression. This keeps `$name` the only value-position read.
    fn callee_value(&mut self, callee: &Expression, scope: &mut ScopeStack) -> Eval<Value> {
        if let ExpressionKind::Symbol(identifier) = callee.kind() {
            let name = self.text(identifier.span());
            return scope.get(name).cloned().ok_or_else(|| {
                self.error(
                    RuntimeErrorKind::Scope(ScopeError::UnknownBinding(name.to_owned())),
                    callee.span(),
                )
            });
        }
        self.expression(callee, scope)
    }

    /// Runs a function body, tracking the last expression value as the result.
    fn body_statements(&mut self, statements: &[Statement], scope: &mut ScopeStack) -> Eval<Flow> {
        let mut last = None;
        for statement in statements {
            match self.statement(statement, scope)? {
                Flow::Fallthrough(Some(value)) => last = Some(value),
                Flow::Fallthrough(None) => {}
                transfer => return Ok(transfer),
            }
        }
        Ok(Flow::Fallthrough(last))
    }

    /// Formats a callable's `source:line:column` origin for display.
    fn location(&self, span: Span) -> String {
        let location = self
            .source
            .location(span.start())
            .expect("ast spans always address their own source");
        format!(
            "{}:{}:{}",
            self.source.name(),
            location.line(),
            location.column()
        )
    }
}

/// The single runtime callable: a named function or an anonymous closure.
#[derive(Clone)]
struct CallableValue {
    /// `Some` for a `def` function, `None` for a closure.
    name: Option<Arc<str>>,
    parameters: Vec<Arc<str>>,
    body: CallableBody,
    captured: ScopeStack,
    location: String,
}

#[derive(Clone)]
enum CallableBody {
    /// A `def` body block; its value is the last expression statement.
    Block(Block),
    /// A closure body; its value is the single expression it evaluates.
    Expression(Box<ConditionalChain>),
}

impl CallableValue {
    fn write_form(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(formatter, "<function {name} at {}>", self.location),
            None => write!(formatter, "<closure at {}>", self.location),
        }
    }
}

impl fmt::Debug for CallableValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_form(formatter)
    }
}

impl Callable for CallableValue {
    fn family(&self) -> &'static str {
        if self.name.is_some() {
            "function"
        } else {
            "closure"
        }
    }

    fn display(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_form(formatter)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn strip_base<'text>(raw: &'text str, lower: &str, upper: &str) -> Option<&'text str> {
    raw.strip_prefix(lower).or_else(|| raw.strip_prefix(upper))
}

/// Decodes one validated double-quoted escape token (`\X` or `\u{H...}`).
///
/// A `Complete` parse guarantees the token is one of the ratified double-quote
/// escapes, so malformed spellings are unreachable and decode defensively to the
/// raw scalar rather than panicking.
fn decode_double_escape(raw: &str) -> String {
    let body = &raw[1..]; // the scalar(s) after the leading backslash
    let marker = body.chars().next().expect("an escape has a body");
    match marker {
        '\\' => "\\".to_owned(),
        '"' => "\"".to_owned(),
        '$' => "$".to_owned(),
        'n' => "\n".to_owned(),
        'r' => "\r".to_owned(),
        't' => "\t".to_owned(),
        '0' => "\0".to_owned(),
        'u' => {
            let hex = body
                .trim_start_matches('u')
                .trim_start_matches('{')
                .trim_end_matches('}');
            u32::from_str_radix(hex, 16)
                .ok()
                .and_then(char::from_u32)
                .map_or_else(|| body.to_owned(), |scalar| scalar.to_string())
        }
        _ => body.to_owned(),
    }
}
