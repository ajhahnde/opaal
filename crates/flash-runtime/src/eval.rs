//! Host-injected tree-walking evaluation of a parsed script.
//!
//! The evaluator owns Flash expression, statement, callable, and control-flow
//! semantics while an injected host owns environment mutation and reached
//! command effects. Public pure-evaluation entry points use a host that rejects
//! process execution with the established structured diagnostics.

use std::any::Any;
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant as SystemInstant;

use flash_platform::{
    DescriptorReadError, DescriptorWriteError, DirectoryEntry, DirectoryReadError, DirectoryStream,
    FileActionError, PipeError, PlatformError, SignalError, SpawnError, WaitError,
    WorkingDirectoryError,
};
use flash_syntax::{
    AndChain, Assignment, BinaryOperator, Block, CallExpression, Closure, CommandItemKind,
    ConditionalChain, ControlTransfer, Declaration, ElseBranch, EnvironmentStatement, Expression,
    ExpressionKind, ForStatement, FunctionDefinition, IfStatement, LanguageMajor, Literal,
    LiteralKind, MatchArm, MatchStatement, Parameter, Pattern, Pipeline, RecordKey,
    RedirectionKind, Script, SourceFile, Span, StageKind, Statement, StatementKind, TryStatement,
    UnaryOperator, VariableReference, WhileStatement, Word, WordPart, WordPartKind,
};

use crate::glob::{DEFAULT_GLOB_ENTRY_LIMIT, GlobPattern};
use crate::intrinsic::{DynamicBinding, ExpressionIntrinsic};
use crate::module::{ResolvedTypeParameter, RuntimeBindingTypes, ValueType};
use crate::operation::{self, OperationError};
use crate::outcome::{Refusal, RefusalReason};
use crate::{
    BindingMutability, Callable, Environment, NativePath, NominalRecordValue, Record, ScopeError,
    ScopeStack, Status, Value, VariantValue,
};

/// Successful automatic continuations allowed for one member in one blocking operation.
pub(crate) const AUTOMATIC_RESUME_LIMIT: usize = 16;

/// A source-anchored runtime evaluation failure.
///
/// The `kind` and primary `span` identify the failing node. `frames` records the
/// chain of function and closure calls the error unwound out of, ordered from the
/// call nearest the failure outward; a top-level failure has no frames.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeError {
    kind: Box<RuntimeErrorKind>,
    span: Span,
    labels: Vec<ErrorLabel>,
    frames: Vec<CallFrame>,
    source: Option<Arc<SourceFile>>,
    cause: Option<Arc<Self>>,
}

impl RuntimeError {
    #[must_use]
    pub fn new(kind: RuntimeErrorKind, span: Span) -> Self {
        Self {
            kind: Box::new(kind),
            span,
            labels: Vec::new(),
            frames: Vec::new(),
            source: None,
            cause: None,
        }
    }

    #[must_use]
    pub fn kind(&self) -> &RuntimeErrorKind {
        self.kind.as_ref()
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    /// Stable language-facing category for this failure.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        self.kind().category()
    }

    /// Ordered secondary labels attached to the failure.
    #[must_use]
    pub fn labels(&self) -> &[ErrorLabel] {
        &self.labels
    }

    /// The innermost-first call frames the error unwound through.
    #[must_use]
    pub fn frames(&self) -> &[CallFrame] {
        &self.frames
    }

    /// The source retained by the evaluator that raised this error, when the
    /// error originated inside pure evaluation.
    #[must_use]
    pub fn source(&self) -> Option<&SourceFile> {
        self.source.as_deref()
    }

    /// An underlying structured failure, when this error wraps another one.
    #[must_use]
    pub fn cause(&self) -> Option<&Self> {
        self.cause.as_deref()
    }

    /// The completed status carried by this failure, when applicable.
    #[must_use]
    pub fn status(&self) -> Option<&Status> {
        match self.kind() {
            RuntimeErrorKind::UnsuccessfulStatus { status } => Some(status),
            RuntimeErrorKind::Transported { status, .. } => status.as_deref(),
            _ => None,
        }
    }

    /// Appends one ordered secondary label.
    #[must_use]
    pub fn with_label(mut self, label: ErrorLabel) -> Self {
        self.labels.push(label);
        self
    }

    /// Attaches one nested structured cause.
    #[must_use]
    pub fn with_cause(mut self, cause: Arc<Self>) -> Self {
        self.cause = Some(cause);
        self
    }

    pub(crate) fn with_source(mut self, source: Arc<SourceFile>) -> Self {
        self.source = Some(source);
        self
    }

    /// Appends an enclosing call frame as the error unwinds outward.
    #[must_use]
    fn with_frame(mut self, frame: CallFrame) -> Self {
        self.frames.push(frame);
        self
    }
}

/// Stable, language-facing families for structured runtime errors.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ErrorCategory {
    User,
    Type,
    Name,
    Control,
    Operation,
    Command,
    Io,
    Process,
    Job,
    Resource,
    Platform,
    Internal,
}

impl ErrorCategory {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Type => "type",
            Self::Name => "name",
            Self::Control => "control",
            Self::Operation => "operation",
            Self::Command => "command",
            Self::Io => "io",
            Self::Process => "process",
            Self::Job => "job",
            Self::Resource => "resource",
            Self::Platform => "platform",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One ordered secondary label retained by a structured runtime error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErrorLabel {
    source: Arc<SourceFile>,
    span: Span,
    message: Arc<str>,
}

impl ErrorLabel {
    #[must_use]
    pub fn new(source: Arc<SourceFile>, span: Span, message: impl Into<Arc<str>>) -> Self {
        Self {
            source,
            span,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn source(&self) -> &SourceFile {
        &self.source
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One function or closure call a [`RuntimeError`] unwound out of.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallFrame {
    callee: FrameCallee,
    call_site: Span,
    source: Arc<SourceFile>,
}

impl CallFrame {
    fn new(name: Option<&str>, call_site: Span, source: Arc<SourceFile>) -> Self {
        let callee = match name {
            Some(name) => FrameCallee::Function(name.to_owned()),
            None => FrameCallee::Closure,
        };
        Self {
            callee,
            call_site,
            source,
        }
    }

    pub(crate) fn restored(callee: FrameCallee, call_site: Span, source: Arc<SourceFile>) -> Self {
        Self {
            callee,
            call_site,
            source,
        }
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

    /// The source containing this call expression.
    #[must_use]
    pub fn source(&self) -> &SourceFile {
        &self.source
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
    /// Reading directories for explicit filesystem matching.
    FilesystemRead,
    /// Loading or exporting a source module during automatic configuration.
    ModuleLoad,
}

impl RestrictedCapability {
    /// Stable diagnostic spelling for the unavailable capability.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ProcessExecution => "process execution",
            Self::CommandSubstitution => "command substitution",
            Self::FilesystemRead => "filesystem reads",
            Self::ModuleLoad => "module loading",
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(formatter)
    }
}

impl Error for RuntimeError {}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeErrorSnapshot {
    pub(crate) category: ErrorCategory,
    pub(crate) message: String,
    pub(crate) span: Span,
    pub(crate) labels: Vec<ErrorLabel>,
    pub(crate) frames: Vec<CallFrame>,
    pub(crate) source: Option<SourceFile>,
    pub(crate) cause: Option<Box<RuntimeErrorSnapshot>>,
    pub(crate) status: Option<Status>,
}

pub(crate) fn snapshot_runtime_error(error: &RuntimeError) -> RuntimeErrorSnapshot {
    RuntimeErrorSnapshot {
        category: error.category(),
        message: error.to_string(),
        span: error.span,
        labels: error.labels.clone(),
        frames: error.frames.clone(),
        source: error.source.as_deref().cloned(),
        cause: error
            .cause
            .as_deref()
            .map(snapshot_runtime_error)
            .map(Box::new),
        status: error.status().cloned(),
    }
}

pub(crate) fn restore_runtime_error(snapshot: RuntimeErrorSnapshot) -> RuntimeError {
    RuntimeError {
        kind: Box::new(RuntimeErrorKind::Transported {
            category: snapshot.category,
            message: snapshot.message,
            status: snapshot.status.map(Box::new),
        }),
        span: snapshot.span,
        labels: snapshot.labels,
        frames: snapshot.frames,
        source: snapshot.source.map(Arc::new),
        cause: snapshot
            .cause
            .map(|cause| Arc::new(restore_runtime_error(*cause))),
    }
}

pub use crate::carrier::{CarrierBridge, CarrierMismatch};

/// A source-independent runtime failure kind.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum RuntimeErrorKind {
    /// A script explicitly raised a string as a user error.
    UserThrown { message: String },
    /// A `throw` operand was neither `String` nor `Error`.
    ThrowValueNotErrorOrString { actual: &'static str },
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
    /// A declaration or parameter destructuring pattern that did not match.
    PatternMismatch,
    /// A nominal constructor did not match its declared record or variant schema.
    NominalConstruction { message: String },
    /// Runtime generic inference or an explicit dynamic instantiation failed.
    GenericInstantiation { message: String },
    /// A call whose callee is not a function or closure.
    NotCallable { actual: &'static str },
    /// A call whose argument count does not match the parameter count.
    ArityMismatch { expected: usize, actual: usize },
    /// A declaration or assignment whose value does not match its binding type.
    BindingTypeMismatch {
        expected: ValueType,
        actual: &'static str,
    },
    /// A call argument whose value does not match its resolved parameter type.
    ParameterTypeMismatch {
        parameter: String,
        expected: ValueType,
        actual: &'static str,
    },
    /// A named function whose returned value does not match its resolved result type.
    FunctionResultTypeMismatch {
        expected: ValueType,
        actual: &'static str,
    },
    /// A function or closure declaring the same parameter name twice.
    DuplicateParameter { name: String },
    /// A construct requiring the execution engine that does not exist yet.
    ExecutionUnsupported,
    /// Automatic startup config reached an operation outside its capability set.
    RestrictedStartup { capability: RestrictedCapability },
    /// Evaluation charged more steps than its resource budget allowed.
    ResourceBudgetExceeded,
    /// An operation deliberately unavailable through the selected boundary.
    Unsupported { feature: &'static str },
    /// An integer literal outside the signed 64-bit range.
    IntegerLiteralOverflow,
    /// A float literal that is not a finite binary64 value.
    FloatLiteralOverflow,
    /// A record literal repeating a key.
    DuplicateRecordKey { key: String },
    /// A double-quoted value interpolation produced native units that cannot
    /// be represented by the UTF-8 `String` value family.
    StringInterpolationNotUtf8,
    /// A present native environment entry cannot be represented by `String`.
    EnvironmentValueNotUtf8 { name: String },
    /// An explicit glob pattern is malformed.
    GlobPattern { message: String },
    /// An explicit glob inspected more entries than its finite traversal limit.
    GlobLimitExceeded { limit: usize },
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
    /// An `export` produced a native environment value containing NUL, which
    /// no child-process environment can represent.
    EnvironmentValueContainsNul { name: String },
    /// A command name that resolved to neither an internal command nor an
    /// executable on `PATH`. `name` is the searched native command name.
    CommandNotFound { name: OsString },
    /// A bare command spelling is reserved against implicit external fallback.
    ReservedCommand(Box<ReservedCommandDetails>),
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
    /// The private typed supervisor snapshot could not be encoded or decoded.
    ExecutionCapsule { message: String },
    /// A structured error restored from a private execution capsule.
    Transported {
        category: ErrorCategory,
        message: String,
        status: Option<Box<Status>>,
    },
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
    /// One member stopped again after the operation exhausted its automatic resumes.
    RepeatedStop { signal: i32 },
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

impl RuntimeErrorKind {
    fn category(&self) -> ErrorCategory {
        match self {
            Self::Transported { category, .. } => *category,
            Self::UserThrown { .. } => ErrorCategory::User,
            Self::Scope(_) => ErrorCategory::Name,
            Self::Operation(_) => ErrorCategory::Operation,
            Self::ThrowValueNotErrorOrString { .. }
            | Self::ConditionNotBool { .. }
            | Self::LogicOperandNotBool { .. }
            | Self::ConditionalOperandNotBoolOrStatus { .. }
            | Self::NotIterable { .. }
            | Self::PatternMismatch
            | Self::NominalConstruction { .. }
            | Self::GenericInstantiation { .. }
            | Self::NotCallable { .. }
            | Self::ArityMismatch { .. }
            | Self::BindingTypeMismatch { .. }
            | Self::ParameterTypeMismatch { .. }
            | Self::FunctionResultTypeMismatch { .. }
            | Self::IntegerLiteralOverflow
            | Self::FloatLiteralOverflow
            | Self::DuplicateRecordKey { .. }
            | Self::StringInterpolationNotUtf8
            | Self::EnvironmentValueNotUtf8 { .. }
            | Self::WordValueNotWordEligible { .. }
            | Self::SpreadValueNotList { .. }
            | Self::SpreadElementNotWordEligible { .. }
            | Self::ExportValueNotEligible { .. }
            | Self::EnvironmentValueContainsNul { .. }
            | Self::ArgumentContainsNul
            | Self::CarrierMismatch(_)
            | Self::MergedEdgeNotByteStream { .. }
            | Self::PipelineHeadInput { .. }
            | Self::BuiltinInputCarrier { .. } => ErrorCategory::Type,
            Self::ControlOutsideLoop { .. }
            | Self::ReturnOutsideFunction
            | Self::NoMatchingArm
            | Self::CheckRequiresUpstream
            | Self::UnsuccessfulStatus { .. } => ErrorCategory::Control,
            Self::CommandNotFound { .. }
            | Self::ReservedCommand(_)
            | Self::BuiltinArity { .. }
            | Self::BuiltinArgument { .. }
            | Self::StructuredCommand { .. }
            | Self::MissingHome
            | Self::InvalidExitCode => ErrorCategory::Command,
            Self::ResourceBudgetExceeded
            | Self::GlobLimitExceeded { .. }
            | Self::CaptureLimitExceeded { .. }
            | Self::BackgroundIdentityExhausted => ErrorCategory::Resource,
            Self::DirectoryRead(_)
            | Self::WorkingDirectory(_)
            | Self::DescriptorNotOpen { .. }
            | Self::PipeCreate(_)
            | Self::CapturePipe(_)
            | Self::CaptureRead(_)
            | Self::PipelineRead(_)
            | Self::PipelineWrite(_)
            | Self::RedirectionSetup(_) => ErrorCategory::Io,
            Self::ProcessSpawn(_)
            | Self::ProcessWait(_)
            | Self::RepeatedStop { .. }
            | Self::BackgroundProcessGroupUnavailable
            | Self::InvalidProcessIdentity => ErrorCategory::Process,
            Self::JobControlUnavailable { .. }
            | Self::JobControlNotInternal { .. }
            | Self::JobControlNotSoleStage { .. }
            | Self::JobOperation { .. }
            | Self::JobSignal(_)
            | Self::UngroupedStop
            | Self::BackgroundObserverUnavailable { .. }
            | Self::BackgroundAssignmentUnavailable
            | Self::ForegroundObservation { .. }
            | Self::BackgroundJobState { .. } => ErrorCategory::Job,
            Self::Presentation(_)
            | Self::TerminalPresentation(_)
            | Self::ShellExecutable(_)
            | Self::ForegroundTerminal(_) => ErrorCategory::Platform,
            Self::GlobPattern { .. }
            | Self::CaptureInvalidUtf8 { .. }
            | Self::StreamCancelled { .. }
            | Self::RestrictedStartup { .. }
            | Self::ExecutionUnsupported
            | Self::Unsupported { .. }
            | Self::ExecutionCapsule { .. }
            | Self::DuplicateParameter { .. }
            | Self::RedirectionDescriptorOverflow => ErrorCategory::Internal,
        }
    }
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
            Self::UserThrown { message } => formatter.write_str(message),
            Self::ThrowValueNotErrorOrString { actual } => {
                write!(
                    formatter,
                    "throw requires a string or error, found {actual}"
                )
            }
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
            Self::PatternMismatch => formatter.write_str("value does not match the pattern"),
            Self::NominalConstruction { message } => formatter.write_str(message),
            Self::GenericInstantiation { message } => formatter.write_str(message),
            Self::NotCallable { actual } => {
                write!(formatter, "cannot call a {actual}")
            }
            Self::ArityMismatch { expected, actual } => {
                write!(formatter, "expected {expected} argument(s), found {actual}")
            }
            Self::BindingTypeMismatch { expected, actual } => {
                write!(formatter, "binding expects {expected}, found {actual}")
            }
            Self::ParameterTypeMismatch {
                parameter,
                expected,
                actual,
            } => write!(
                formatter,
                "parameter {parameter:?} expects {expected}, found {actual}"
            ),
            Self::FunctionResultTypeMismatch { expected, actual } => {
                write!(
                    formatter,
                    "function result expects {expected}, found {actual}"
                )
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
            Self::Unsupported { feature } => {
                write!(formatter, "{feature} is not supported by this boundary")
            }
            Self::IntegerLiteralOverflow => formatter.write_str("integer literal is out of range"),
            Self::FloatLiteralOverflow => formatter.write_str("float literal is not finite"),
            Self::DuplicateRecordKey { key } => {
                write!(formatter, "duplicate record key {key:?}")
            }
            Self::StringInterpolationNotUtf8 => {
                formatter.write_str("double-quoted value interpolation is not valid UTF-8")
            }
            Self::EnvironmentValueNotUtf8 { name } => {
                write!(formatter, "environment entry {name:?} is not valid UTF-8")
            }
            Self::GlobPattern { message } => write!(formatter, "invalid glob pattern: {message}"),
            Self::GlobLimitExceeded { limit } => {
                write!(formatter, "glob exceeded its {limit}-entry traversal limit")
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
            Self::EnvironmentValueContainsNul { name } => {
                write!(formatter, "environment entry {name:?} contains a NUL byte")
            }
            Self::CommandNotFound { name } => {
                write!(
                    formatter,
                    "command not found: {}",
                    NativePath::new(name.clone())
                )
            }
            Self::ReservedCommand(details) => {
                write!(
                    formatter,
                    "command `{}` is reserved: {}",
                    details.name, details.purpose
                )?;
                if let Some(replacement) = &details.replacement {
                    write!(formatter, "; use `{replacement}` instead")?;
                }
                write!(
                    formatter,
                    "; use `^{0}` or `command {0}` for intentional external execution",
                    details.name
                )
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
            Self::ExecutionCapsule { message } => {
                write!(formatter, "execution capsule is unavailable: {message}")
            }
            Self::Transported { message, .. } => formatter.write_str(message),
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
                formatter.write_str("; use `$(bytes: ...)` to preserve arbitrary output")
            }
            Self::RedirectionSetup(error) => error.fmt(formatter),
            Self::ProcessSpawn(error) => error.fmt(formatter),
            Self::ProcessWait(error) => error.fmt(formatter),
            Self::RepeatedStop { signal } => {
                write!(
                    formatter,
                    "the job stopped repeatedly (latest signal {signal})"
                )
            }
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

/// Structured runtime data for a reserved bare command spelling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservedCommandDetails {
    name: String,
    purpose: String,
    replacement: Option<String>,
}

impl ReservedCommandDetails {
    /// Build one reserved-command refusal from validated namespace metadata.
    #[must_use]
    pub const fn new(name: String, purpose: String, replacement: Option<String>) -> Self {
        Self {
            name,
            purpose,
            replacement,
        }
    }

    /// The reserved source spelling.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Stable reason the spelling is unavailable.
    #[must_use]
    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    /// Optional canonical migration target.
    #[must_use]
    pub fn replacement(&self) -> Option<&str> {
        self.replacement.as_deref()
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// Default statement/expression charges permitted for one Flash 2 evaluation.
pub const DEFAULT_V2_EVALUATION_STEPS: u64 = 1_000_000;
/// Default nested callable depth permitted for one Flash 2 evaluation.
pub const DEFAULT_V2_CALL_DEPTH: u64 = 256;
/// Default retained collection elements permitted for one Flash 2 evaluation.
pub const DEFAULT_V2_COLLECTION_ITEMS: u64 = 1_000_000;
/// Default newly retained string/collection bytes permitted for one Flash 2 evaluation.
pub const DEFAULT_V2_COLLECTION_BYTES: u64 = 16 * 1024 * 1024;

/// Deterministic resource limits and counters for one evaluation boundary.
///
/// The evaluator charges one step at each statement and each expression.
/// Exhausting any ceiling is a [`RuntimeErrorKind::ResourceBudgetExceeded`]
/// runtime error, not a cancellation. The value remains `Copy` so existing
/// Flash 1 embedding code retains its value semantics; an execution owner passes
/// one mutable copy across all statements and modules in a logical evaluation.
#[derive(Clone, Copy, Debug)]
pub struct ResourceBudget {
    step_limit: Option<u64>,
    used_steps: u64,
    call_depth_limit: Option<u64>,
    call_depth: u64,
    peak_call_depth: u64,
    collection_item_limit: Option<u64>,
    collection_items: u64,
    collection_byte_limit: Option<u64>,
    collection_bytes: u64,
}

impl ResourceBudget {
    /// A budget that never runs out.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            step_limit: None,
            used_steps: 0,
            call_depth_limit: None,
            call_depth: 0,
            peak_call_depth: 0,
            collection_item_limit: None,
            collection_items: 0,
            collection_byte_limit: None,
            collection_bytes: 0,
        }
    }

    /// A budget of exactly `steps` charges.
    #[must_use]
    pub const fn steps(steps: u64) -> Self {
        Self {
            step_limit: Some(steps),
            used_steps: 0,
            call_depth_limit: None,
            call_depth: 0,
            peak_call_depth: 0,
            collection_item_limit: None,
            collection_items: 0,
            collection_byte_limit: None,
            collection_bytes: 0,
        }
    }

    /// The complete default Flash 2 evaluator contract.
    #[must_use]
    pub const fn v2() -> Self {
        Self {
            step_limit: Some(DEFAULT_V2_EVALUATION_STEPS),
            used_steps: 0,
            call_depth_limit: Some(DEFAULT_V2_CALL_DEPTH),
            call_depth: 0,
            peak_call_depth: 0,
            collection_item_limit: Some(DEFAULT_V2_COLLECTION_ITEMS),
            collection_items: 0,
            collection_byte_limit: Some(DEFAULT_V2_COLLECTION_BYTES),
            collection_bytes: 0,
        }
    }

    /// Adds an exact nested-call ceiling to this budget.
    #[must_use]
    pub const fn with_call_depth(mut self, limit: u64) -> Self {
        self.call_depth_limit = Some(limit);
        self
    }

    /// Adds an exact cumulative retained-collection-item ceiling.
    #[must_use]
    pub const fn with_collection_items(mut self, limit: u64) -> Self {
        self.collection_item_limit = Some(limit);
        self
    }

    /// Adds an exact cumulative newly retained string/collection-byte ceiling.
    #[must_use]
    pub const fn with_collection_bytes(mut self, limit: u64) -> Self {
        self.collection_byte_limit = Some(limit);
        self
    }

    /// Charges one step, returning `false` when the budget is exhausted.
    fn charge(&mut self) -> bool {
        if let Some(limit) = self.step_limit {
            if self.used_steps >= limit {
                return false;
            }
            self.used_steps += 1;
        }
        true
    }

    fn charge_collection_items(&mut self, amount: usize) -> bool {
        let Ok(amount) = u64::try_from(amount) else {
            return false;
        };
        let Some(next) = self.collection_items.checked_add(amount) else {
            return false;
        };
        if self.collection_item_limit.is_some_and(|limit| next > limit) {
            return false;
        }
        self.collection_items = next;
        true
    }

    fn charge_collection_bytes(&mut self, amount: usize) -> bool {
        let Ok(amount) = u64::try_from(amount) else {
            return false;
        };
        let Some(next) = self.collection_bytes.checked_add(amount) else {
            return false;
        };
        if self.collection_byte_limit.is_some_and(|limit| next > limit) {
            return false;
        }
        self.collection_bytes = next;
        true
    }

    fn enter_call(&mut self) -> bool {
        let Some(next) = self.call_depth.checked_add(1) else {
            return false;
        };
        if self.call_depth_limit.is_some_and(|limit| next > limit) {
            return false;
        }
        self.call_depth = next;
        self.peak_call_depth = self.peak_call_depth.max(next);
        true
    }

    fn leave_call(&mut self) {
        self.call_depth = self
            .call_depth
            .checked_sub(1)
            .expect("every entered runtime call leaves exactly once");
    }

    /// Charges already consumed by this shared evaluation budget.
    #[must_use]
    pub const fn used(&self) -> u64 {
        self.used_steps
    }

    /// Peak nested callable depth observed by this shared budget.
    #[must_use]
    pub const fn peak_call_depth(&self) -> u64 {
        self.peak_call_depth
    }

    /// Cumulative retained collection elements charged by this budget.
    #[must_use]
    pub const fn collection_items(&self) -> u64 {
        self.collection_items
    }

    /// Cumulative newly retained string/collection bytes charged by this budget.
    #[must_use]
    pub const fn collection_bytes(&self) -> u64 {
        self.collection_bytes
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
pub(crate) enum EvaluationPolicy {
    General,
    Startup,
    PureV2,
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

    /// Limits for pure Flash 2 evaluation under the configured resource owner.
    #[must_use]
    pub const fn pure_v2(cancel: CancellationToken, budget: ResourceBudget) -> Self {
        Self {
            cancel,
            budget,
            policy: EvaluationPolicy::PureV2,
        }
    }

    pub(crate) const fn resource_budget(&self) -> ResourceBudget {
        self.budget
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

/// Rich completion returned to an evaluator host such as an interactive session.
#[allow(dead_code)] // Session-only variants are connected in the next seam phase.
pub(crate) enum HostedEvaluationOutcome {
    Value(Value),
    Cancelled(Cancellation),
    Refused(Refusal),
    Exit(u8),
    Stopped(crate::job::JobId),
}

/// Failures that remain distinct from evaluator control outcomes.
#[allow(dead_code)] // Fatal session output is connected in the next seam phase.
pub(crate) enum HostedEvaluationFailure {
    Runtime(RuntimeError),
    Output(io::Error),
}

/// An in-flight evaluation abort, including session control and fatal effects.
///
/// This is the internal short-circuit channel. A `RuntimeError` converts into the
/// `Error` arm through `?`, while cancellation rides the separate `Cancelled` arm
/// so it never becomes a `RuntimeError` at the public boundary.
#[allow(dead_code)] // Session-only outcomes are connected in the next seam phase.
pub(crate) enum Abort {
    Error(RuntimeError),
    Cancelled(Cancellation),
    Refused(Refusal),
    Exit(u8),
    Stopped(crate::job::JobId),
    Output(io::Error),
}

impl Abort {
    /// Appends a call frame to an error as it unwinds; cancellation is unchanged.
    fn with_frame(self, frame: CallFrame) -> Self {
        match self {
            Self::Error(error) => Self::Error(error.with_frame(frame)),
            other => other,
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

/// Source and analysis identity supplied to an evaluation host invocation.
pub(crate) struct EvaluationContext {
    pub(crate) source: Arc<SourceFile>,
    pub(crate) binding_types: Arc<RuntimeBindingTypes>,
    pub(crate) cancel: CancellationToken,
    /// Whether this pipeline is the complete submitted conditional chain and
    /// may therefore publish a managed foreground job.
    pub(crate) manage_foreground: bool,
}

/// Successful bounded output captured from one reached conditional chain.
pub(crate) struct CapturedChain {
    pub(crate) bytes: Vec<u8>,
    pub(crate) status: Status,
}

#[derive(Clone, Copy)]
pub(crate) enum CapturePosition {
    Expression,
    Word,
}

/// The effect boundary used by recursive language evaluation.
pub(crate) trait EvaluationHost {
    fn environment(&mut self) -> &mut Environment;
    fn current_status(&self) -> Option<&Status>;
    fn policy(&self) -> EvaluationPolicy;

    /// Whether this host can retain a complete submitted job as one managed
    /// foreground process group. Other hosts keep evaluating conditional
    /// chains operand by operand through the ordinary recursive evaluator.
    fn manages_foreground_jobs(&self) -> bool {
        false
    }

    /// Snapshots language-owned host state before a catchable transaction.
    fn language_state_checkpoint(&mut self) -> Box<dyn Any> {
        Box::new(self.environment().clone())
    }

    /// Restores a checkpoint produced by [`Self::language_state_checkpoint`].
    fn restore_language_state(&mut self, checkpoint: Box<dyn Any>) {
        *self.environment() = *checkpoint
            .downcast::<Environment>()
            .expect("the default evaluation checkpoint is an environment");
    }

    fn read_directory(&mut self, path: &Path)
    -> Result<Box<dyn DirectoryStream>, RuntimeErrorKind>;

    fn execute_chain(
        &mut self,
        chain: &ConditionalChain,
        scope: &mut ScopeStack,
        context: EvaluationContext,
    ) -> Result<Status, Abort>;

    fn capture_chain(
        &mut self,
        chain: &ConditionalChain,
        scope: &mut ScopeStack,
        span: Span,
        position: CapturePosition,
        context: EvaluationContext,
    ) -> Result<CapturedChain, Abort>;
}

/// Host used by the public evaluator APIs that deliberately cannot run jobs.
struct PureEvaluationHost<'environment> {
    environment: &'environment mut Environment,
    policy: EvaluationPolicy,
}

impl EvaluationHost for PureEvaluationHost<'_> {
    fn environment(&mut self) -> &mut Environment {
        self.environment
    }

    fn current_status(&self) -> Option<&Status> {
        None
    }

    fn policy(&self) -> EvaluationPolicy {
        self.policy
    }

    fn read_directory(
        &mut self,
        _path: &Path,
    ) -> Result<Box<dyn DirectoryStream>, RuntimeErrorKind> {
        // flash-v1-boundary(embedding-refusal): Pure evaluator entry points have no filesystem host.
        Err(RuntimeErrorKind::ExecutionUnsupported)
    }

    fn execute_chain(
        &mut self,
        chain: &ConditionalChain,
        _scope: &mut ScopeStack,
        context: EvaluationContext,
    ) -> Result<Status, Abort> {
        let _ = (&context.binding_types, &context.cancel);
        Err(Abort::Error(
            // flash-v1-boundary(embedding-refusal): Pure evaluator entry points cannot execute jobs.
            RuntimeError::new(RuntimeErrorKind::ExecutionUnsupported, chain.span())
                .with_source(context.source),
        ))
    }

    fn capture_chain(
        &mut self,
        _chain: &ConditionalChain,
        _scope: &mut ScopeStack,
        span: Span,
        position: CapturePosition,
        context: EvaluationContext,
    ) -> Result<CapturedChain, Abort> {
        let _ = (&context.binding_types, &context.cancel);
        let kind = match position {
            CapturePosition::Expression => {
                // flash-v1-boundary(embedding-refusal): Pure evaluator entry points cannot capture jobs.
                RuntimeErrorKind::ExecutionUnsupported
            }
            CapturePosition::Word => {
                // flash-v1-boundary(embedding-refusal): Pure word expansion has no session capture host.
                RuntimeErrorKind::Unsupported {
                    feature: "command substitution in a word",
                }
            }
        };
        Err(Abort::Error(
            RuntimeError::new(kind, span).with_source(context.source),
        ))
    }
}

/// Evaluates a parsed script against a scope stack, returning its final value.
///
/// The script value is the value of the last expression statement, otherwise
/// `Null`. The scope stack is mutated in place so a REPL can reuse it. This entry
/// point cannot be cancelled; use [`evaluate_with_cancellation`] for a token.
pub fn evaluate(
    script: &flash_syntax::Script,
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
    script: &flash_syntax::Script,
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
    script: &flash_syntax::Script,
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
    script: &flash_syntax::Script,
    source: &SourceFile,
    scope: &mut ScopeStack,
    env: &mut Environment,
    limits: &EvalLimits,
) -> Result<Completion, RuntimeError> {
    evaluate_in_environment_owned(script, Arc::new(source.clone()), scope, env, limits)
}

pub(crate) fn evaluate_in_environment_owned(
    script: &flash_syntax::Script,
    source: Arc<SourceFile>,
    scope: &mut ScopeStack,
    env: &mut Environment,
    limits: &EvalLimits,
) -> Result<Completion, RuntimeError> {
    evaluate_in_environment_owned_with_binding_types(
        script,
        source,
        scope,
        env,
        limits,
        Arc::new(RuntimeBindingTypes::default()),
    )
}

pub(crate) fn evaluate_in_environment_owned_with_binding_types(
    script: &flash_syntax::Script,
    source: Arc<SourceFile>,
    scope: &mut ScopeStack,
    env: &mut Environment,
    limits: &EvalLimits,
    binding_types: Arc<RuntimeBindingTypes>,
) -> Result<Completion, RuntimeError> {
    let mut host = PureEvaluationHost {
        environment: env,
        policy: limits.policy,
    };
    match evaluate_with_host(script, source, scope, limits, binding_types, &mut host) {
        Ok(HostedEvaluationOutcome::Value(value)) => Ok(Completion::Value(value)),
        Ok(HostedEvaluationOutcome::Cancelled(cancellation)) => {
            Ok(Completion::Cancelled(cancellation))
        }
        Err(HostedEvaluationFailure::Runtime(error)) => Err(error),
        Ok(
            HostedEvaluationOutcome::Refused(_)
            | HostedEvaluationOutcome::Exit(_)
            | HostedEvaluationOutcome::Stopped(_),
        )
        | Err(HostedEvaluationFailure::Output(_)) => {
            unreachable!("the pure evaluation host cannot produce session outcomes")
        }
    }
}

pub(crate) fn evaluate_with_host(
    script: &flash_syntax::Script,
    source: Arc<SourceFile>,
    scope: &mut ScopeStack,
    limits: &EvalLimits,
    binding_types: Arc<RuntimeBindingTypes>,
    host: &mut dyn EvaluationHost,
) -> Result<HostedEvaluationOutcome, HostedEvaluationFailure> {
    let mut budget = limits.budget;
    evaluate_with_host_and_budget(
        script,
        source,
        scope,
        limits,
        &mut budget,
        binding_types,
        host,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_with_host_and_budget(
    script: &flash_syntax::Script,
    source: Arc<SourceFile>,
    scope: &mut ScopeStack,
    limits: &EvalLimits,
    budget: &mut ResourceBudget,
    binding_types: Arc<RuntimeBindingTypes>,
    host: &mut dyn EvaluationHost,
) -> Result<HostedEvaluationOutcome, HostedEvaluationFailure> {
    let mut evaluator = Evaluator {
        source,
        binding_types,
        current_result_type: None,
        cancel: limits.cancel.clone(),
        budget,
        host,
    };
    let mut last = Value::Null;
    for statement in script.statements() {
        let flow = match evaluator.statement(statement, scope) {
            Ok(flow) => flow,
            Err(Abort::Cancelled(cancellation)) => {
                return Ok(HostedEvaluationOutcome::Cancelled(cancellation));
            }
            Err(Abort::Refused(refusal)) => {
                return Ok(HostedEvaluationOutcome::Refused(refusal));
            }
            Err(Abort::Error(error)) => {
                return Err(HostedEvaluationFailure::Runtime(error));
            }
            Err(Abort::Exit(code)) => return Ok(HostedEvaluationOutcome::Exit(code)),
            Err(Abort::Stopped(job)) => return Ok(HostedEvaluationOutcome::Stopped(job)),
            Err(Abort::Output(error)) => return Err(HostedEvaluationFailure::Output(error)),
        };
        match flow {
            Flow::Fallthrough(Some(result)) => last = result.value,
            Flow::Fallthrough(None) => {}
            Flow::Break(span) => {
                return Err(HostedEvaluationFailure::Runtime(RuntimeError::new(
                    RuntimeErrorKind::ControlOutsideLoop {
                        control: ControlKind::Break,
                    },
                    span,
                )));
            }
            Flow::Continue(span) => {
                return Err(HostedEvaluationFailure::Runtime(RuntimeError::new(
                    RuntimeErrorKind::ControlOutsideLoop {
                        control: ControlKind::Continue,
                    },
                    span,
                )));
            }
            Flow::Return(_, span) => {
                return Err(HostedEvaluationFailure::Runtime(RuntimeError::new(
                    RuntimeErrorKind::ReturnOutsideFunction,
                    span,
                )));
            }
        }
    }
    Ok(HostedEvaluationOutcome::Value(last))
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
    evaluate_closure_argument_with_binding_types(
        closure,
        source,
        scope,
        Arc::new(RuntimeBindingTypes::default()),
    )
}

pub(crate) fn evaluate_closure_argument_with_binding_types(
    closure: &Closure,
    source: &SourceFile,
    scope: &ScopeStack,
    binding_types: Arc<RuntimeBindingTypes>,
) -> Result<Value, RuntimeError> {
    let limits = EvalLimits::default();
    let mut env = Environment::new();
    let mut host = PureEvaluationHost {
        environment: &mut env,
        policy: limits.policy,
    };
    let mut budget = limits.budget;
    let evaluator = Evaluator {
        source: Arc::new(source.clone()),
        binding_types,
        current_result_type: None,
        cancel: limits.cancel,
        budget: &mut budget,
        host: &mut host,
    };
    match evaluator.make_closure(closure, scope) {
        Ok(value) => Ok(value),
        Err(Abort::Error(error)) => Err(error),
        Err(Abort::Cancelled(_)) => {
            unreachable!("creating a closure does not poll cancellation")
        }
        Err(Abort::Refused(_) | Abort::Exit(_) | Abort::Stopped(_) | Abort::Output(_)) => {
            unreachable!("the pure evaluation host cannot produce session outcomes")
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

    let mut host = PureEvaluationHost {
        environment: env,
        policy: limits.policy,
    };
    let mut budget = limits.budget;
    let mut evaluator = Evaluator {
        source: Arc::new(source.clone()),
        binding_types: Arc::clone(&function.binding_types),
        current_result_type: None,
        cancel: limits.cancel.clone(),
        budget: &mut budget,
        host: &mut host,
    };
    // Cancellation is polled before entering the body, matching an ordinary call.
    if let Err(abort) = evaluator.check_cancel(span) {
        return match abort {
            Abort::Cancelled(cancellation) => Ok(Completion::Cancelled(cancellation)),
            Abort::Error(error) => Err(error),
            Abort::Refused(_) | Abort::Exit(_) | Abort::Stopped(_) | Abort::Output(_) => {
                unreachable!("the pure evaluation host cannot produce session outcomes")
            }
        };
    }
    let arguments = arguments
        .into_iter()
        .map(|value| RuntimeArgument { value, span })
        .collect();
    match evaluator.run_call(callable, function, arguments, span, None, None) {
        Ok(value) => Ok(Completion::Value(value)),
        Err(Abort::Cancelled(cancellation)) => Ok(Completion::Cancelled(cancellation)),
        Err(Abort::Error(error)) => Err(error),
        Err(Abort::Refused(_) | Abort::Exit(_) | Abort::Stopped(_) | Abort::Output(_)) => {
            unreachable!("the pure evaluation host cannot produce session outcomes")
        }
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
    expand_word_with_environment(word, source, scope, &Environment::new())
}

pub(crate) fn expand_word_with_environment(
    word: &Word,
    source: &SourceFile,
    scope: &mut ScopeStack,
    environment: &Environment,
) -> Result<ExpandedWord, RuntimeError> {
    let limits = EvalLimits::default();
    let mut env = environment.clone();
    let mut host = PureEvaluationHost {
        environment: &mut env,
        policy: limits.policy,
    };
    let mut budget = limits.budget;
    let mut evaluator = Evaluator {
        source: Arc::new(source.clone()),
        binding_types: Arc::new(RuntimeBindingTypes::default()),
        current_result_type: None,
        cancel: limits.cancel.clone(),
        budget: &mut budget,
        host: &mut host,
    };
    match evaluator.expand_word(word, scope) {
        Ok(expanded) => Ok(expanded),
        Err(Abort::Error(error)) => Err(error),
        Err(Abort::Cancelled(_)) => {
            unreachable!("a never-cancelling token cannot produce a cancellation")
        }
        Err(Abort::Refused(_) | Abort::Exit(_) | Abort::Stopped(_) | Abort::Output(_)) => {
            unreachable!("the pure evaluation host cannot produce session outcomes")
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
    let mut host = PureEvaluationHost {
        environment: &mut env,
        policy: limits.policy,
    };
    let mut budget = limits.budget;
    let mut evaluator = Evaluator {
        source: Arc::new(source.clone()),
        binding_types: Arc::new(RuntimeBindingTypes::default()),
        current_result_type: None,
        cancel: limits.cancel.clone(),
        budget: &mut budget,
        host: &mut host,
    };
    match evaluator.expand_spread(variable, item_span, scope) {
        Ok(expanded) => Ok(expanded),
        Err(Abort::Error(error)) => Err(error),
        Err(Abort::Cancelled(_)) => {
            unreachable!("a never-cancelling token cannot produce a cancellation")
        }
        Err(Abort::Refused(_) | Abort::Exit(_) | Abort::Stopped(_) | Abort::Output(_)) => {
            unreachable!("the pure evaluation host cannot produce session outcomes")
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
    Fallthrough(Option<FlowValue>),
    /// A `break` originating at the given span is propagating outward.
    Break(Span),
    /// A `continue` originating at the given span is propagating outward.
    Continue(Span),
    /// A `return` carrying its value is propagating outward to the function.
    Return(FlowValue, Span),
}

struct FlowValue {
    value: Value,
    span: Span,
}

struct Evaluator<'budget, 'host> {
    source: Arc<SourceFile>,
    binding_types: Arc<RuntimeBindingTypes>,
    current_result_type: Option<ValueType>,
    cancel: CancellationToken,
    budget: &'budget mut ResourceBudget,
    host: &'host mut dyn EvaluationHost,
}

impl Evaluator<'_, '_> {
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
            StatementKind::ModuleImport(_) | StatementKind::ModuleExport(_)
                if self.binding_types.language(self.source.id()) == Some(LanguageMajor::V2) =>
            {
                Ok(Flow::Fallthrough(None))
            }
            StatementKind::Import(_)
            | StatementKind::ModuleImport(_)
            | StatementKind::ModuleExport(_)
                if self.host.policy() == EvaluationPolicy::Startup =>
            {
                Err(self.error(
                    RuntimeErrorKind::RestrictedStartup {
                        capability: RestrictedCapability::ModuleLoad,
                    },
                    span,
                ))
            }
            StatementKind::Import(_)
            | StatementKind::ModuleImport(_)
            | StatementKind::ModuleExport(_) => {
                // flash-v1-boundary(embedding-refusal): The module-program loader owns import and export execution.
                Err(self.error(RuntimeErrorKind::ExecutionUnsupported, span))
            }
            StatementKind::NominalType(_) | StatementKind::VariantType(_) => {
                Ok(Flow::Fallthrough(None))
            }
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
            StatementKind::Try(try_statement) => self.try_statement(try_statement, scope),
            StatementKind::Throw(expression) => self.throw(expression, scope),
            StatementKind::Control(control) => self.control(control, scope, span),
            StatementKind::Job(job) => self.job(job, scope, span, None),
        }
    }

    fn job(
        &mut self,
        job: &flash_syntax::JobStatement,
        scope: &mut ScopeStack,
        span: Span,
        expected: Option<&ValueType>,
    ) -> Eval<Flow> {
        if self.host.policy() == EvaluationPolicy::Startup {
            return Err(self.error(
                RuntimeErrorKind::RestrictedStartup {
                    capability: RestrictedCapability::ProcessExecution,
                },
                span,
            ));
        }
        if job.background_span.is_some() {
            // flash-v1-boundary(embedding-refusal): The session job coordinator owns background launch.
            return Err(self.error(RuntimeErrorKind::ExecutionUnsupported, span));
        }
        let value =
            if self.host.manages_foreground_jobs() && chain_contains_command_stage(&job.chain) {
                Value::Status(self.execute_chain(&job.chain, scope, true)?)
            } else {
                self.eval_chain_with_expected(&job.chain, scope, expected)?
            };
        Ok(Flow::Fallthrough(Some(FlowValue {
            value,
            span: job.chain.span(),
        })))
    }

    fn declaration(&mut self, declaration: &Declaration, scope: &mut ScopeStack) -> Eval<()> {
        let value_type = declaration
            .type_annotation
            .as_ref()
            .and_then(|annotation| {
                self.binding_types
                    .annotation_type(self.source.id(), annotation.span)
                    .cloned()
            })
            .or_else(|| {
                matches!(declaration.pattern, Pattern::Binding(_))
                    .then(|| {
                        self.binding_types
                            .binding_type(self.source.id(), declaration.name.span())
                            .cloned()
                    })
                    .flatten()
            });
        let value =
            self.expression_with_expected(&declaration.value, scope, value_type.as_ref())?;
        let mutability = if declaration.mutable {
            BindingMutability::Mutable
        } else {
            BindingMutability::Immutable
        };
        if let Some(expected) = &value_type
            && !expected.accepts(&value)
        {
            return Err(self.error(
                RuntimeErrorKind::BindingTypeMismatch {
                    expected: expected.clone(),
                    actual: value.family_name(),
                },
                declaration.value.span(),
            ));
        }
        if !matches!(declaration.pattern, Pattern::Binding(_)) {
            let checkpoint = scope.clone();
            let pattern_type = value_type.clone().or_else(|| runtime_value_type(&value));
            if !self.pattern_matches(
                &declaration.pattern,
                &value,
                scope,
                mutability,
                pattern_type.as_ref(),
            )? {
                *scope = checkpoint;
                return Err(self.error(RuntimeErrorKind::PatternMismatch, declaration.name.span()));
            }
            return Ok(());
        }
        let name = self.text(declaration.name.span()).to_owned();
        self.ensure_lexical_name(&name, declaration.name.span())?;
        scope
            .declare_typed(name, mutability, value, value_type)
            .map_err(|error| {
                let span = if matches!(error, ScopeError::TypeMismatch { .. }) {
                    declaration.value.span()
                } else {
                    declaration.name.span()
                };
                self.binding_error(error, span)
            })
    }

    fn assignment(&mut self, assignment: &Assignment, scope: &mut ScopeStack) -> Eval<()> {
        let name = self.text(assignment.target.name.span()).to_owned();
        if DynamicBinding::lookup(&name).is_some() {
            return Err(self.error(
                RuntimeErrorKind::Scope(ScopeError::ImmutableBinding(name.clone())),
                assignment.target.span,
            ));
        }
        let value = self.expression(&assignment.value, scope)?;
        scope.assign(&name, value).map_err(|error| {
            let span = if matches!(error, ScopeError::TypeMismatch { .. }) {
                assignment.value.span()
            } else {
                assignment.target.span
            };
            self.binding_error(error, span)
        })
    }

    /// Applies an `export` or `unset` to the environment. `export` encodes its
    /// value with the canonical word encoding; `unset` removes the name and is a
    /// no-op when absent. Neither creates or removes a lexical binding.
    fn environment(
        &mut self,
        environment: &EnvironmentStatement,
        scope: &mut ScopeStack,
    ) -> Eval<()> {
        if self.host.policy() == EvaluationPolicy::PureV2 {
            let span = match environment {
                EnvironmentStatement::Export { name, value: _ }
                | EnvironmentStatement::Unset { name } => name.span(),
            };
            return Err(Abort::Refused(Refusal::new(
                RefusalReason::Unsupported,
                "environment mutation",
                span,
            )));
        }
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
                if encoded.as_encoded_bytes().contains(&0) {
                    return Err(self.error(
                        RuntimeErrorKind::EnvironmentValueContainsNul { name },
                        value.span(),
                    ));
                }
                self.host.environment().set(name, encoded);
            }
            EnvironmentStatement::Unset { name } => {
                let name = self.text(name.span()).to_owned();
                self.host.environment().remove(&name);
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
        let name = self.text(statement.binding.span()).to_owned();
        self.ensure_lexical_name(&name, statement.binding.span())?;
        let iterable = self.expression(&statement.iterable, scope)?;
        let boundary = statement.iterable.span();
        match iterable {
            Value::List(items) => {
                for item in items.iter() {
                    self.check_cancel(boundary)?;
                    match self.iteration(&name, item.clone(), &statement.body, scope)? {
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
                    match self.iteration(&name, Value::Int(current), &statement.body, scope)? {
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

    fn try_statement(&mut self, statement: &TryStatement, scope: &mut ScopeStack) -> Eval<Flow> {
        let scope_checkpoint = scope.clone();
        let host_checkpoint = self.host.language_state_checkpoint();
        match self.block(&statement.try_block, scope) {
            Ok(flow) => Ok(flow),
            Err(Abort::Error(error)) => {
                *scope = scope_checkpoint;
                self.host.restore_language_state(host_checkpoint);

                let name = self.text(statement.catch_binding.span()).to_owned();
                self.ensure_lexical_name(&name, statement.catch_binding.span())?;
                scope.push();
                let outcome = (|| {
                    scope
                        .declare(
                            name,
                            BindingMutability::Immutable,
                            Value::Error(Arc::new(error)),
                        )
                        .map_err(|error| {
                            self.error(
                                RuntimeErrorKind::Scope(error),
                                statement.catch_binding.span(),
                            )
                        })?;
                    self.statements(&statement.catch_block.statements, scope)
                })();
                scope.pop().expect("a catch block pushes exactly one frame");
                outcome
            }
            Err(other) => Err(other),
        }
    }

    fn throw(&mut self, expression: &Expression, scope: &mut ScopeStack) -> Eval<Flow> {
        match self.expression(expression, scope)? {
            Value::String(message) => Err(self.error(
                RuntimeErrorKind::UserThrown {
                    message: message.to_string(),
                },
                expression.span(),
            )),
            Value::Error(error) => Err(Abort::Error((*error).clone())),
            value => Err(self.error(
                RuntimeErrorKind::ThrowValueNotErrorOrString {
                    actual: value.family_name(),
                },
                expression.span(),
            )),
        }
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
        let subject_type = runtime_value_type(subject);
        if !self.pattern_matches(
            &arm.pattern,
            subject,
            scope,
            BindingMutability::Immutable,
            subject_type.as_ref(),
        )? {
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
        mutability: BindingMutability,
        expected_type: Option<&ValueType>,
    ) -> Eval<bool> {
        match pattern {
            Pattern::Wildcard(_) => Ok(true),
            Pattern::Literal(literal) => {
                let expected = self.literal(literal, scope)?;
                Ok(&expected == subject)
            }
            Pattern::Binding(identifier) => {
                let name = self.text(identifier.span());
                self.ensure_lexical_name(name, identifier.span())?;
                scope
                    .declare_typed(name, mutability, subject.clone(), expected_type.cloned())
                    .map_err(|error| {
                        self.error(RuntimeErrorKind::Scope(error), identifier.span())
                    })?;
                Ok(true)
            }
            Pattern::List(pattern) => {
                let Value::List(values) = subject else {
                    return Ok(false);
                };
                if values.len() < pattern.elements.len()
                    || (pattern.rest.is_none() && values.len() != pattern.elements.len())
                {
                    return Ok(false);
                }
                for (element, value) in pattern.elements.iter().zip(values.iter()) {
                    let element_type = match expected_type {
                        Some(ValueType::List(element)) => Some(element.as_ref()),
                        _ => None,
                    };
                    if !self.pattern_matches(element, value, scope, mutability, element_type)? {
                        return Ok(false);
                    }
                }
                if let Some(rest) = pattern.rest {
                    let retained = values.len() - pattern.elements.len();
                    if !self.budget.charge_collection_items(retained) {
                        return Err(
                            self.error(RuntimeErrorKind::ResourceBudgetExceeded, rest.span())
                        );
                    }
                    let name = self.text(rest.span());
                    self.ensure_lexical_name(name, rest.span())?;
                    let rest_type = match expected_type {
                        Some(ValueType::List(element)) => Some(ValueType::List(element.clone())),
                        _ => None,
                    };
                    scope
                        .declare_typed(
                            name,
                            mutability,
                            Value::list(values[pattern.elements.len()..].to_vec()),
                            rest_type,
                        )
                        .map_err(|error| self.error(RuntimeErrorKind::Scope(error), rest.span()))?;
                }
                Ok(true)
            }
            Pattern::NominalRecord(pattern) => {
                let Value::NominalRecord(value) = subject else {
                    return Ok(false);
                };
                let segments = pattern
                    .name
                    .segments
                    .iter()
                    .map(|segment| self.text(segment.span()))
                    .collect::<Vec<_>>();
                let Some(nominal) = self
                    .binding_types
                    .qualified_nominal(self.source.id(), &segments)
                    .cloned()
                else {
                    return Ok(false);
                };
                if nominal.id() != value.id() {
                    return Ok(false);
                }
                let substitutions = nominal
                    .type_parameters()
                    .iter()
                    .zip(value.type_arguments())
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone()))
                    .collect::<BTreeMap<_, _>>();
                for field in &pattern.fields {
                    let name = self.text(field.name.span());
                    let Some(subject) = value.get(name) else {
                        return Ok(false);
                    };
                    let expected = nominal
                        .fields()
                        .iter()
                        .find(|schema| schema.name() == name)
                        .map(|schema| {
                            crate::module::substitute_type(schema.value_type(), &substitutions)
                        });
                    if !self.pattern_matches(
                        &field.pattern,
                        subject,
                        scope,
                        mutability,
                        expected.as_ref(),
                    )? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Pattern::Variant(pattern) => {
                let Value::Variant(value) = subject else {
                    return Ok(false);
                };
                if pattern.constructor.segments.len() < 2 {
                    return Ok(false);
                }
                let segments = pattern
                    .constructor
                    .segments
                    .iter()
                    .map(|segment| self.text(segment.span()))
                    .collect::<Vec<_>>();
                let Some((nominal, constructor)) = self
                    .binding_types
                    .qualified_variant(self.source.id(), &segments)
                    .map(|(nominal, constructor)| (nominal.clone(), constructor.to_owned()))
                else {
                    return Ok(false);
                };
                if nominal.id() != value.id()
                    || constructor != value.constructor()
                    || pattern.payload.len() != value.payload().len()
                {
                    return Ok(false);
                }
                let substitutions = nominal
                    .type_parameters()
                    .iter()
                    .zip(value.type_arguments())
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone()))
                    .collect::<BTreeMap<_, _>>();
                let payload_types = nominal
                    .variants()
                    .iter()
                    .find(|variant| variant.name() == constructor)
                    .map_or(&[][..], |variant| variant.payload());
                for (index, (pattern, subject)) in
                    pattern.payload.iter().zip(value.payload()).enumerate()
                {
                    let expected = payload_types.get(index).map(|value_type| {
                        crate::module::substitute_type(value_type, &substitutions)
                    });
                    if !self.pattern_matches(
                        pattern,
                        subject,
                        scope,
                        mutability,
                        expected.as_ref(),
                    )? {
                        return Ok(false);
                    }
                }
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
                let result = match expression {
                    Some(expression) => {
                        let expected = self.current_result_type.clone();
                        FlowValue {
                            value: self.expression_with_expected(
                                expression,
                                scope,
                                expected.as_ref(),
                            )?,
                            span: expression.span(),
                        }
                    }
                    None => FlowValue {
                        value: Value::Null,
                        span,
                    },
                };
                Ok(Flow::Return(result, span))
            }
        }
    }

    fn condition(&mut self, chain: &ConditionalChain, scope: &mut ScopeStack) -> Eval<bool> {
        let value = self.eval_chain(chain, scope)?;
        self.expect_condition(&value, chain.span())
    }

    fn context(&self, manage_foreground: bool) -> EvaluationContext {
        EvaluationContext {
            source: Arc::clone(&self.source),
            binding_types: Arc::clone(&self.binding_types),
            cancel: self.cancel.clone(),
            manage_foreground,
        }
    }

    fn execute_chain(
        &mut self,
        chain: &ConditionalChain,
        scope: &mut ScopeStack,
        manage_foreground: bool,
    ) -> Eval<Status> {
        let context = self.context(manage_foreground);
        self.host.execute_chain(chain, scope, context)
    }

    fn capture_chain(
        &mut self,
        chain: &ConditionalChain,
        scope: &mut ScopeStack,
        span: Span,
        position: CapturePosition,
        capture: flash_syntax::CommandCaptureKind,
    ) -> Eval<Value> {
        let context = self.context(false);
        let CapturedChain { bytes, status } = self
            .host
            .capture_chain(chain, scope, span, position, context)?;
        let _ = status;
        match capture {
            flash_syntax::CommandCaptureKind::Text => {
                crate::execute::decode_text_bytes(bytes, span)
                    .map(Value::string)
                    .map_err(Abort::Error)
            }
            flash_syntax::CommandCaptureKind::Bytes => Ok(Value::bytes(bytes)),
        }
    }

    /// Evaluates a conditional chain to a value.
    ///
    /// A single-term chain is transparent. Multiple `||` terms return the last
    /// evaluated operand and branch over either `Bool` or `Status`.
    fn eval_chain(&mut self, chain: &ConditionalChain, scope: &mut ScopeStack) -> Eval<Value> {
        self.eval_chain_with_expected(chain, scope, None)
    }

    fn eval_chain_with_expected(
        &mut self,
        chain: &ConditionalChain,
        scope: &mut ScopeStack,
        expected: Option<&ValueType>,
    ) -> Eval<Value> {
        let manage_foreground =
            chain.or_terms().len() == 1 && chain.or_terms()[0].and_terms().len() == 1;
        let transparent = chain.or_terms().len() == 1;
        let mut terms = chain.or_terms().iter();
        let first = terms
            .next()
            .expect("a parsed conditional chain contains an operand");
        let mut value = self.eval_and_chain(
            first,
            scope,
            manage_foreground,
            transparent.then_some(expected).flatten(),
        )?;
        let mut value_span = first.span();
        for and_chain in terms {
            if self.expect_logic_condition(&value, value_span)? {
                return Ok(value);
            }
            value = self.eval_and_chain(and_chain, scope, false, None)?;
            value_span = and_chain.span();
        }
        if chain.or_terms().len() > 1 {
            self.expect_logic_condition(&value, value_span)?;
        }
        Ok(value)
    }

    /// Evaluates one `&&` chain and returns its last evaluated operand.
    fn eval_and_chain(
        &mut self,
        and_chain: &AndChain,
        scope: &mut ScopeStack,
        manage_foreground: bool,
        expected: Option<&ValueType>,
    ) -> Eval<Value> {
        let transparent = and_chain.and_terms().len() == 1;
        let mut terms = and_chain.and_terms().iter();
        let first = terms
            .next()
            .expect("a parsed and-chain contains an operand");
        let mut value = self.eval_pipeline(
            first,
            scope,
            manage_foreground,
            transparent.then_some(expected).flatten(),
        )?;
        let mut value_span = first.span();
        for pipeline in terms {
            if !self.expect_logic_condition(&value, value_span)? {
                return Ok(value);
            }
            value = self.eval_pipeline(pipeline, scope, false, None)?;
            value_span = pipeline.span();
        }
        if and_chain.and_terms().len() > 1 {
            self.expect_logic_condition(&value, value_span)?;
        }
        Ok(value)
    }

    /// Evaluates a pure single-stage expression or delegates one reached
    /// effectful pipeline through the active host.
    fn eval_pipeline(
        &mut self,
        pipeline: &Pipeline,
        scope: &mut ScopeStack,
        manage_foreground: bool,
        expected: Option<&ValueType>,
    ) -> Eval<Value> {
        if let [stage] = pipeline.stages()
            && let StageKind::Expression(expression) = stage.kind()
        {
            return self.expression_with_expected(expression, scope, expected);
        }
        if self.binding_types.language(self.source.id()) == Some(LanguageMajor::V2)
            && pipeline
                .stages()
                .iter()
                .all(|stage| matches!(stage.kind(), StageKind::Expression(_)))
        {
            let mut stages = pipeline.stages().iter();
            let first = stages
                .next()
                .expect("a parsed pipeline contains at least one stage");
            let StageKind::Expression(expression) = first.kind() else {
                unreachable!("the pure pipeline predicate accepts only expressions")
            };
            let mut value = self.expression(expression, scope)?;
            for stage in stages {
                self.check_cancel(stage.span())?;
                let StageKind::Expression(expression) = stage.kind() else {
                    unreachable!("the pure pipeline predicate accepts only expressions")
                };
                let ExpressionKind::Qualified(name) = expression.kind() else {
                    // flash-v1-boundary(carrier-refusal): V2 value pipelines accept only qualified operation stages.
                    return Err(
                        self.unsupported("non-operation value pipeline stage", stage.span())
                    );
                };
                let segments = name
                    .segments
                    .iter()
                    .map(|segment| self.text(segment.span()))
                    .collect::<Vec<_>>();
                let Some(operation) = self
                    .binding_types
                    .qualified_operation(self.source.id(), &segments)
                else {
                    // flash-v1-boundary(carrier-refusal): Unknown V2 operation stages cannot consume a value carrier.
                    return Err(self.unsupported("unknown value pipeline operation", stage.span()));
                };
                value = operation
                    .execute_value(value)
                    .map_err(|error| self.operation(error, stage.span()))?;
            }
            return Ok(value);
        }
        if self.host.policy() == EvaluationPolicy::Startup {
            return Err(self.error(
                RuntimeErrorKind::RestrictedStartup {
                    capability: RestrictedCapability::ProcessExecution,
                },
                pipeline.span(),
            ));
        }
        let chain = ConditionalChain::from_pipeline(pipeline.clone());
        self.execute_chain(&chain, scope, manage_foreground)
            .map(Value::Status)
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
        self.expression_with_expected(expression, scope, None)
    }

    fn expression_with_expected(
        &mut self,
        expression: &Expression,
        scope: &mut ScopeStack,
        expected: Option<&ValueType>,
    ) -> Eval<Value> {
        let span = expression.span();
        self.charge(span)?;
        match expression.kind() {
            ExpressionKind::Literal(literal) => self.literal(literal, scope),
            ExpressionKind::Variable(variable) => {
                let name = self.text(variable.name.span());
                self.binding_value(name, scope, span)
            }
            ExpressionKind::Symbol(_) => {
                // flash-v1-boundary(carrier-refusal): Bare symbols are patterns and names rather than runtime values.
                Err(self.unsupported("bare symbol", span))
            }
            ExpressionKind::Qualified(name) => {
                let binding = name
                    .segments
                    .iter()
                    .map(|segment| self.text(segment.span()))
                    .collect::<Vec<_>>()
                    .join("::");
                if let Some(value) = scope.get(&binding) {
                    Ok(value.clone())
                } else {
                    self.qualified_value(name, span, expected)
                }
            }
            ExpressionKind::List(elements) => {
                if !self.budget.charge_collection_items(elements.len()) {
                    return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
                }
                let mut values = Vec::with_capacity(elements.len());
                let expected_element = match expected {
                    Some(ValueType::List(element)) => Some(element.as_ref()),
                    _ => None,
                };
                for element in elements {
                    values.push(self.expression_with_expected(element, scope, expected_element)?);
                }
                Ok(Value::list(values))
            }
            ExpressionKind::Record(entries) => {
                if !self.budget.charge_collection_items(entries.len()) {
                    return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
                }
                let mut pairs = Vec::with_capacity(entries.len());
                for entry in entries {
                    let key = self.record_key(&entry.key, scope)?;
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
            ExpressionKind::NominalRecord(record) => {
                self.nominal_record(record, scope, span, expected)
            }
            ExpressionKind::Closure(closure) => self.make_closure(closure, scope),
            ExpressionKind::CommandSubstitution(_) | ExpressionKind::GroupedJob(_) => {
                self.grouped_or_substitution(expression, scope)
            }
            ExpressionKind::Call(call) => self.call(call, scope, span, expected),
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

    fn qualified_value(
        &self,
        name: &flash_syntax::QualifiedName,
        span: Span,
        expected: Option<&ValueType>,
    ) -> Eval<Value> {
        if name.segments.len() >= 2 {
            let segments = name
                .segments
                .iter()
                .map(|segment| self.text(segment.span()))
                .collect::<Vec<_>>();
            if let Some((nominal, constructor)) = self
                .binding_types
                .qualified_variant(self.source.id(), &segments)
                && nominal.kind() == crate::module::NominalTypeKind::Variant
                && let Some(variant) = nominal
                    .variants()
                    .iter()
                    .find(|variant| variant.name() == constructor)
                && variant.payload().is_empty()
            {
                let type_arguments = if nominal.type_parameters().is_empty() {
                    Vec::new()
                } else if let Some(ValueType::Nominal { id, arguments }) = expected
                    && id.as_ref() == nominal.id()
                    && arguments.len() == nominal.type_parameters().len()
                {
                    arguments.clone()
                } else {
                    return Err(self.error(
                        RuntimeErrorKind::GenericInstantiation {
                            message: format!(
                                "cannot infer type argument `{}`; provide it explicitly",
                                nominal.type_parameters()[0].name()
                            ),
                        },
                        span,
                    ));
                };
                self.validate_runtime_nominal_constraints(nominal, &type_arguments, span)?;
                return Ok(Value::Variant(Box::new(VariantValue::new(
                    nominal.id().clone(),
                    type_arguments,
                    constructor.to_owned(),
                    Vec::new(),
                ))));
            }
        }
        // flash-v1-boundary(carrier-refusal): Unresolved V2 qualified names are not runtime values.
        Err(self.unsupported("qualified value", span))
    }

    fn nominal_record(
        &mut self,
        record: &flash_syntax::NominalRecordExpression,
        scope: &mut ScopeStack,
        span: Span,
        expected_result: Option<&ValueType>,
    ) -> Eval<Value> {
        let Some(type_segment) = record.name.segments.last() else {
            // flash-v1-boundary(carrier-refusal): A malformed nominal constructor cannot produce a runtime value.
            return Err(self.unsupported("nominal record construction", span));
        };
        let type_name = self.text(type_segment.span()).to_owned();
        let segments = record
            .name
            .segments
            .iter()
            .map(|segment| self.text(segment.span()))
            .collect::<Vec<_>>();
        let Some(nominal) = self
            .binding_types
            .qualified_nominal(self.source.id(), &segments)
            .cloned()
        else {
            return Err(self.error(
                RuntimeErrorKind::NominalConstruction {
                    message: format!("unknown nominal record `{type_name}`"),
                },
                span,
            ));
        };
        if nominal.kind() != crate::module::NominalTypeKind::Record {
            return Err(self.error(
                RuntimeErrorKind::NominalConstruction {
                    message: format!("`{type_name}` is not a nominal record"),
                },
                span,
            ));
        }
        if !self.budget.charge_collection_items(record.fields.len()) {
            return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
        }
        let mut substitutions = BTreeMap::new();
        if let Some(ValueType::Nominal { id, arguments }) = expected_result
            && id.as_ref() == nominal.id()
            && arguments.len() == nominal.type_parameters().len()
        {
            substitutions.extend(
                nominal
                    .type_parameters()
                    .iter()
                    .zip(arguments)
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone())),
            );
        }
        let mut evaluated = BTreeMap::new();
        for field in &record.fields {
            let name = self.text(field.name.span()).to_owned();
            let expected = nominal
                .fields()
                .iter()
                .find(|schema| schema.name() == name)
                .map(|schema| crate::module::substitute_type(schema.value_type(), &substitutions));
            let value = self.expression_with_expected(&field.value, scope, expected.as_ref())?;
            if evaluated.insert(name.clone(), value).is_some() {
                return Err(self.error(
                    RuntimeErrorKind::NominalConstruction {
                        message: format!("nominal record field `{name}` is repeated"),
                    },
                    field.span,
                ));
            }
        }
        if let Some(unknown) = evaluated.keys().find(|name| {
            !nominal
                .fields()
                .iter()
                .any(|field| field.name() == name.as_str())
        }) {
            return Err(self.error(
                RuntimeErrorKind::NominalConstruction {
                    message: format!("nominal record `{type_name}` has no field `{unknown}`"),
                },
                span,
            ));
        }
        let mut fields = Vec::with_capacity(nominal.fields().len());
        for schema in nominal.fields() {
            let Some(value) = evaluated.remove(schema.name()) else {
                return Err(self.error(
                    RuntimeErrorKind::NominalConstruction {
                        message: format!(
                            "nominal record `{type_name}` requires field `{}`",
                            schema.name()
                        ),
                    },
                    span,
                ));
            };
            infer_runtime_substitution(schema.value_type(), &value, &mut substitutions);
            fields.push((Arc::from(schema.name()), value));
        }
        let mut type_arguments = Vec::with_capacity(nominal.type_parameters().len());
        for parameter in nominal.type_parameters() {
            let Some(value_type) = substitutions.get(parameter.name()).cloned() else {
                return Err(self.error(
                    RuntimeErrorKind::GenericInstantiation {
                        message: format!(
                            "cannot infer type argument `{}`; provide it explicitly",
                            parameter.name()
                        ),
                    },
                    span,
                ));
            };
            type_arguments.push(value_type);
        }
        for (schema, (_, value)) in nominal.fields().iter().zip(&fields) {
            let expected = crate::module::substitute_type(schema.value_type(), &substitutions);
            if !expected.accepts(value) {
                return Err(self.error(
                    RuntimeErrorKind::NominalConstruction {
                        message: format!(
                            "nominal record field `{}` expects `{expected}`, found {}",
                            schema.name(),
                            value.family_name()
                        ),
                    },
                    span,
                ));
            }
        }
        self.validate_runtime_nominal_constraints(&nominal, &type_arguments, span)?;
        Ok(Value::NominalRecord(Box::new(NominalRecordValue::new(
            nominal.id().clone(),
            type_arguments,
            fields,
        ))))
    }

    fn validate_runtime_nominal_constraints(
        &self,
        nominal: &crate::module::NominalType,
        arguments: &[ValueType],
        span: Span,
    ) -> Eval<()> {
        for (parameter, value_type) in nominal.type_parameters().iter().zip(arguments) {
            for constraint in parameter.constraints() {
                if !self
                    .binding_types
                    .type_satisfies_constraint(value_type, *constraint)
                {
                    return Err(self.error(
                        RuntimeErrorKind::GenericInstantiation {
                            message: format!(
                                "type `{value_type}` does not satisfy `{constraint:?}` for `{}`",
                                parameter.name()
                            ),
                        },
                        span,
                    ));
                }
            }
        }
        Ok(())
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
            ExpressionKind::CommandSubstitution(_)
                if self.host.policy() == EvaluationPolicy::Startup =>
            {
                Err(self.error(
                    RuntimeErrorKind::RestrictedStartup {
                        capability: RestrictedCapability::CommandSubstitution,
                    },
                    span,
                ))
            }
            ExpressionKind::CommandSubstitution(substitution) => self.capture_chain(
                substitution.chain(),
                scope,
                span,
                CapturePosition::Expression,
                substitution.capture(),
            ),
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

    fn literal(&mut self, literal: &Literal, scope: &mut ScopeStack) -> Eval<Value> {
        let span = literal.span();
        match literal.kind() {
            LiteralKind::Null => Ok(Value::Null),
            LiteralKind::Boolean(value) => Ok(Value::Bool(*value)),
            LiteralKind::Integer => self.integer(span),
            LiteralKind::Float => self.float(span),
            LiteralKind::SingleQuoted => {
                let retained = span.end().saturating_sub(span.start()).saturating_sub(2);
                self.retain_collection_bytes(retained, span)?;
                let raw = self.text(span);
                // The span includes both surrounding single quotes; content is exact.
                let inner = &raw[1..raw.len() - 1];
                Ok(Value::string(inner))
            }
            LiteralKind::DoubleQuoted(parts) => self.double_quoted_value(parts, scope, span),
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

    fn record_key(&mut self, key: &RecordKey, scope: &mut ScopeStack) -> Eval<String> {
        match key {
            RecordKey::Identifier(identifier) => Ok(self.text(identifier.span()).to_owned()),
            RecordKey::SingleQuoted(span) => {
                let raw = self.text(*span);
                Ok(raw[1..raw.len() - 1].to_owned())
            }
            RecordKey::DoubleQuoted(part) => self
                .double_quoted_value(std::slice::from_ref(part), scope, part.span())
                .map(|value| match value {
                    Value::String(key) => key.to_string(),
                    _ => unreachable!("double-quoted values always produce strings"),
                }),
        }
    }

    fn double_quoted_value(
        &mut self,
        parts: &[WordPart],
        scope: &mut ScopeStack,
        span: Span,
    ) -> Eval<Value> {
        let mut value = OsString::new();
        let mut provenance = Vec::new();
        for part in parts {
            self.expand_part(part, scope, &mut value, &mut provenance)?;
        }
        value
            .into_string()
            .map(Value::string)
            .map_err(|_| self.error(RuntimeErrorKind::StringInterpolationNotUtf8, span))
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
            WordPartKind::Bare | WordPartKind::DoubleText => {
                let retained = self.text(span).len();
                self.retain_collection_bytes(retained, span)?;
                value.push(self.text(span));
            }
            WordPartKind::SingleQuoted => {
                // The span includes both single quotes; the content is exact.
                let retained = self.text(span).len().saturating_sub(2);
                self.retain_collection_bytes(retained, span)?;
                let raw = self.text(span);
                value.push(&raw[1..raw.len() - 1]);
            }
            WordPartKind::BareEscape => {
                // A bare backslash quotes exactly the next scalar literally.
                let retained = self.text(span).len().saturating_sub(1);
                self.retain_collection_bytes(retained, span)?;
                let raw = self.text(span);
                value.push(&raw[1..]);
            }
            WordPartKind::DoubleEscape => {
                let retained = decode_double_escape(self.text(span)).len();
                self.retain_collection_bytes(retained, span)?;
                value.push(decode_double_escape(self.text(span)));
            }
            WordPartKind::Variable(identifier) => {
                let name = self.text(identifier.span());
                let resolved = self.binding_value(name, scope, span)?;
                value.push(self.encode_scalar(&resolved, span)?);
            }
            WordPartKind::BracedInterpolation(expression) => {
                let resolved = self.expression(expression, scope)?;
                value.push(self.encode_scalar(&resolved, span)?);
            }
            WordPartKind::CommandSubstitution(substitution) => {
                if self.host.policy() == EvaluationPolicy::PureV2 {
                    return Err(Abort::Refused(Refusal::new(
                        RefusalReason::Unsupported,
                        "process execution",
                        span,
                    )));
                }
                if self.host.policy() == EvaluationPolicy::Startup {
                    return Err(self.error(
                        RuntimeErrorKind::RestrictedStartup {
                            capability: RestrictedCapability::CommandSubstitution,
                        },
                        span,
                    ));
                }
                let captured = self.capture_chain(
                    substitution.chain(),
                    scope,
                    span,
                    CapturePosition::Word,
                    substitution.capture(),
                )?;
                value.push(self.encode_scalar(&captured, span)?);
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
    fn encode_scalar(&mut self, value: &Value, span: Span) -> Eval<OsString> {
        let existing_bytes = match value {
            Value::String(text) => Some(text.len()),
            Value::Path(path) => Some(path.as_os_str().as_encoded_bytes().len()),
            _ => None,
        };
        if let Some(bytes) = existing_bytes {
            self.retain_collection_bytes(bytes, span)?;
        }
        let encoded = word_encoding(value).ok_or_else(|| {
            self.error(
                RuntimeErrorKind::WordValueNotWordEligible {
                    actual: value.family_name(),
                },
                span,
            )
        })?;
        if existing_bytes.is_none() {
            self.retain_collection_bytes(encoded.len(), span)?;
        }
        Ok(encoded)
    }

    fn retain_collection_bytes(&mut self, amount: usize, span: Span) -> Eval<()> {
        if self.budget.charge_collection_bytes(amount) {
            Ok(())
        } else {
            Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span))
        }
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
        let resolved = self.binding_value(name, scope, item_span)?;
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

    fn text(&self, span: Span) -> &str {
        self.source
            .slice(span)
            .expect("ast spans always address their own source")
    }

    /// Builds a runtime-error abort anchored at `span`.
    fn error(&self, kind: RuntimeErrorKind, span: Span) -> Abort {
        Abort::Error(RuntimeError::new(kind, span).with_source(Arc::clone(&self.source)))
    }

    fn binding_error(&self, error: ScopeError, span: Span) -> Abort {
        let kind = match error {
            ScopeError::TypeMismatch { expected, actual } => {
                RuntimeErrorKind::BindingTypeMismatch { expected, actual }
            }
            error => RuntimeErrorKind::Scope(error),
        };
        self.error(kind, span)
    }

    fn unsupported(&self, feature: &'static str, span: Span) -> Abort {
        // flash-v1-boundary(carrier-refusal): Callers use this for explicit value and carrier refusals.
        self.error(RuntimeErrorKind::Unsupported { feature }, span)
    }

    fn operation(&self, error: OperationError, span: Span) -> Abort {
        self.error(RuntimeErrorKind::Operation(error), span)
    }

    fn ensure_lexical_name(&self, name: &str, span: Span) -> Eval<()> {
        if DynamicBinding::lookup(name).is_some() {
            Err(self.error(
                RuntimeErrorKind::Scope(ScopeError::ReservedBinding(name.to_owned())),
                span,
            ))
        } else {
            Ok(())
        }
    }

    fn binding_value(&self, name: &str, scope: &ScopeStack, span: Span) -> Eval<Value> {
        if let Some(dynamic) = DynamicBinding::lookup(name) {
            return Ok(match dynamic {
                DynamicBinding::CurrentStatus => self
                    .host
                    .current_status()
                    .cloned()
                    .map(Value::Status)
                    .unwrap_or(Value::Null),
            });
        }
        scope.get(name).cloned().ok_or_else(|| {
            self.error(
                RuntimeErrorKind::Scope(ScopeError::UnknownBinding(name.to_owned())),
                span,
            )
        })
    }

    fn function_definition(
        &self,
        definition: &FunctionDefinition,
        scope: &mut ScopeStack,
    ) -> Eval<()> {
        let name: Arc<str> = Arc::from(self.text(definition.name.span()));
        self.ensure_lexical_name(&name, definition.name.span())?;
        let signature = self
            .binding_types
            .function_signature(self.source.id(), definition.name.span())
            .cloned();
        let parameters = self.parameters(
            &definition.parameters,
            signature.as_ref().map(|signature| signature.parameters()),
        )?;
        let inspection = signature
            .clone()
            .map(|signature| crate::help::FunctionInspection::new(signature, &self.source));
        let callable = CallableValue {
            name: Some(Arc::clone(&name)),
            parameters,
            type_parameters: signature
                .as_ref()
                .map_or_else(Vec::new, |signature| signature.type_parameters().to_vec()),
            body: CallableBody::Block(definition.body.clone()),
            captured: scope.captured_snapshot(),
            source: Arc::clone(&self.source),
            binding_types: Arc::clone(&self.binding_types),
            result_type: Some(
                self.binding_types
                    .function_result_type(self.source.id(), definition.name.span())
                    .cloned()
                    .unwrap_or(ValueType::Any),
            ),
            location: self.location(definition.name.span()),
            inspection,
            origin_span: definition.name.span(),
        };
        let value = Value::Callable(Arc::new(callable));
        scope
            .declare(name.as_ref(), BindingMutability::Immutable, value)
            .map_err(|error| self.error(RuntimeErrorKind::Scope(error), definition.name.span()))
    }

    fn make_closure(&self, closure: &Closure, scope: &ScopeStack) -> Eval<Value> {
        let parameters = self.parameters(&closure.parameters, None)?;
        let callable = CallableValue {
            name: None,
            parameters,
            type_parameters: Vec::new(),
            body: CallableBody::Expression(closure.body.clone()),
            captured: scope.captured_snapshot(),
            source: Arc::clone(&self.source),
            binding_types: Arc::clone(&self.binding_types),
            result_type: closure.result_type.as_ref().and_then(|annotation| {
                self.binding_types
                    .annotation_type(self.source.id(), annotation.span)
                    .cloned()
            }),
            location: self.location(closure.span),
            inspection: None,
            origin_span: closure.span,
        };
        Ok(Value::Callable(Arc::new(callable)))
    }

    /// Collects parameter names, rejecting a repeated name at creation time.
    fn parameters(
        &self,
        parameters: &[Parameter],
        signatures: Option<&[crate::module::FunctionParameterSignature]>,
    ) -> Eval<Vec<CallableParameter>> {
        let mut resolved = Vec::with_capacity(parameters.len());
        for parameter in parameters {
            let name = self.text(parameter.name.span());
            self.ensure_lexical_name(name, parameter.name.span())?;
            if resolved
                .iter()
                .any(|existing: &CallableParameter| existing.name.as_ref() == name)
            {
                return Err(self.error(
                    RuntimeErrorKind::DuplicateParameter {
                        name: name.to_owned(),
                    },
                    parameter.span,
                ));
            }
            let value_type = signatures
                .and_then(|signatures| signatures.get(resolved.len()))
                .map(|signature| signature.value_type().clone())
                .or_else(|| {
                    parameter.type_annotation.as_ref().and_then(|annotation| {
                        self.binding_types
                            .annotation_type(self.source.id(), annotation.span)
                            .cloned()
                    })
                })
                .unwrap_or(ValueType::Any);
            resolved.push(CallableParameter {
                name: Arc::from(name),
                value_type,
                pattern: Some(parameter.pattern.clone()),
            });
        }
        Ok(resolved)
    }

    fn call(
        &mut self,
        call: &CallExpression,
        scope: &mut ScopeStack,
        span: Span,
        expected_result: Option<&ValueType>,
    ) -> Eval<Value> {
        // Cancellation is polled before entering any call.
        self.check_cancel(span)?;

        if let ExpressionKind::Qualified(name) = call.callee.kind()
            && let Some(value) = self.variant_value(name, call, scope, span, expected_result)?
        {
            return Ok(value);
        }

        if let ExpressionKind::Qualified(name) = call.callee.kind() {
            let segments = name
                .segments
                .iter()
                .map(|segment| self.text(segment.span()))
                .collect::<Vec<_>>();
            if let Some(operation) = self
                .binding_types
                .qualified_operation(self.source.id(), &segments)
            {
                if call.arguments.len() != 1 {
                    return Err(self.error(
                        RuntimeErrorKind::ArityMismatch {
                            expected: 1,
                            actual: call.arguments.len(),
                        },
                        span,
                    ));
                }
                if !call.type_arguments.is_empty()
                    && call.type_arguments.len() != operation.type_parameters().len()
                {
                    return Err(self.error(
                        RuntimeErrorKind::GenericInstantiation {
                            message: format!(
                                "expected {} type arguments, found {}",
                                operation.type_parameters().len(),
                                call.type_arguments.len()
                            ),
                        },
                        span,
                    ));
                }
                let type_arguments = call
                    .type_arguments
                    .iter()
                    .map(|argument| {
                        self.binding_types
                            .annotation_type(self.source.id(), argument.span)
                            .cloned()
                            .unwrap_or(ValueType::Any)
                    })
                    .collect::<Vec<_>>();
                let argument = self.expression(&call.arguments[0], scope)?;
                return operation
                    .execute_value_with_types(argument, &type_arguments)
                    .map_err(|error| self.operation(error, span));
            }
        }

        if let ExpressionKind::Symbol(identifier) = call.callee.kind() {
            let name = self.text(identifier.span());
            if let Some(intrinsic) = ExpressionIntrinsic::lookup(name)
                && scope.get(name).is_none()
            {
                if call.arguments.len() != intrinsic.arity() {
                    return Err(self.error(
                        RuntimeErrorKind::ArityMismatch {
                            expected: intrinsic.arity(),
                            actual: call.arguments.len(),
                        },
                        span,
                    ));
                }
                let argument = self.expression(&call.arguments[0], scope)?;
                match intrinsic {
                    ExpressionIntrinsic::Env => {
                        if self.host.policy() == EvaluationPolicy::PureV2 {
                            return Err(Abort::Refused(Refusal::new(
                                RefusalReason::Unsupported,
                                "environment read",
                                span,
                            )));
                        }
                        let Value::String(name) = argument else {
                            return Err(self.operation(
                                OperationError::UnsupportedOperands {
                                    operator: "env",
                                    operands: vec![argument.family_name()],
                                },
                                call.arguments[0].span(),
                            ));
                        };
                        let value = self.host.environment().get(&name).map(OsStr::to_os_string);
                        return value.map_or(Ok(Value::Null), |value| {
                            value.into_string().map(Value::string).map_err(|_| {
                                self.error(
                                    RuntimeErrorKind::EnvironmentValueNotUtf8 {
                                        name: name.to_string(),
                                    },
                                    span,
                                )
                            })
                        });
                    }
                    ExpressionIntrinsic::Glob => return self.glob(&argument, span),
                    ExpressionIntrinsic::Float | ExpressionIntrinsic::Int => {}
                }
                return intrinsic
                    .invoke(&argument)
                    .map_err(|error| self.operation(error, span));
            }
        }

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

        let explicit_type_arguments: Option<Vec<ValueType>> = if call.type_arguments.is_empty() {
            None
        } else {
            Some(
                call.type_arguments
                    .iter()
                    .map(|argument| {
                        self.binding_types
                            .annotation_type(self.source.id(), argument.span)
                            .cloned()
                            .unwrap_or(ValueType::Any)
                    })
                    .collect(),
            )
        };
        let mut expected_substitutions = BTreeMap::new();
        if let Some(explicit) = &explicit_type_arguments {
            for (parameter, value_type) in function.type_parameters.iter().zip(explicit) {
                expected_substitutions.insert(parameter.name().to_owned(), value_type.clone());
            }
        } else if let (Some(result_type), Some(expected_result)) =
            (&function.result_type, expected_result)
        {
            infer_runtime_type_value(result_type, expected_result, &mut expected_substitutions);
        }
        let mut arguments = Vec::with_capacity(call.arguments.len());
        for (argument, parameter) in call.arguments.iter().zip(&function.parameters) {
            let expected =
                crate::module::substitute_type(&parameter.value_type, &expected_substitutions);
            let value = self.expression_with_expected(argument, scope, Some(&expected))?;
            if explicit_type_arguments.is_none() {
                infer_runtime_substitution(
                    &parameter.value_type,
                    &value,
                    &mut expected_substitutions,
                );
            }
            arguments.push(RuntimeArgument {
                value,
                span: argument.span(),
            });
        }
        self.run_call(
            &callable,
            function,
            arguments,
            span,
            explicit_type_arguments,
            expected_result,
        )
    }

    fn variant_value(
        &mut self,
        name: &flash_syntax::QualifiedName,
        call: &CallExpression,
        scope: &mut ScopeStack,
        span: Span,
        expected_result: Option<&ValueType>,
    ) -> Eval<Option<Value>> {
        if name.segments.len() < 2 {
            return Ok(None);
        }
        let segments = name
            .segments
            .iter()
            .map(|segment| self.text(segment.span()))
            .collect::<Vec<_>>();
        let Some((nominal, constructor)) = self
            .binding_types
            .qualified_variant(self.source.id(), &segments)
            .map(|(nominal, constructor)| (nominal.clone(), constructor.to_owned()))
        else {
            return Ok(None);
        };
        let type_name = nominal.id().name().to_owned();
        if nominal.kind() != crate::module::NominalTypeKind::Variant {
            return Ok(None);
        }
        let Some(variant) = nominal
            .variants()
            .iter()
            .find(|variant| variant.name() == constructor)
            .cloned()
        else {
            return Err(self.error(
                RuntimeErrorKind::NominalConstruction {
                    message: format!(
                        "variant type `{type_name}` has no `{constructor}` constructor"
                    ),
                },
                span,
            ));
        };
        if call.arguments.len() != variant.payload().len() {
            return Err(self.error(
                RuntimeErrorKind::ArityMismatch {
                    expected: variant.payload().len(),
                    actual: call.arguments.len(),
                },
                span,
            ));
        }
        if !self.budget.charge_collection_items(call.arguments.len()) {
            return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
        }
        let mut substitutions = BTreeMap::new();
        if !call.type_arguments.is_empty() {
            if call.type_arguments.len() != nominal.type_parameters().len() {
                return Err(self.error(
                    RuntimeErrorKind::GenericInstantiation {
                        message: format!(
                            "expected {} type arguments, found {}",
                            nominal.type_parameters().len(),
                            call.type_arguments.len()
                        ),
                    },
                    span,
                ));
            }
            for (parameter, argument) in nominal.type_parameters().iter().zip(&call.type_arguments)
            {
                let value_type = self
                    .binding_types
                    .annotation_type(self.source.id(), argument.span)
                    .cloned()
                    .unwrap_or(ValueType::Any);
                substitutions.insert(parameter.name().to_owned(), value_type);
            }
        } else if let Some(ValueType::Nominal { id, arguments }) = expected_result
            && id.as_ref() == nominal.id()
            && arguments.len() == nominal.type_parameters().len()
        {
            substitutions.extend(
                nominal
                    .type_parameters()
                    .iter()
                    .zip(arguments)
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone())),
            );
        }
        let mut payload = Vec::with_capacity(call.arguments.len());
        for (argument, expected) in call.arguments.iter().zip(variant.payload()) {
            let expected_value = crate::module::substitute_type(expected, &substitutions);
            let value = self.expression_with_expected(argument, scope, Some(&expected_value))?;
            infer_runtime_substitution(expected, &value, &mut substitutions);
            payload.push(value);
        }
        let mut type_arguments = Vec::with_capacity(nominal.type_parameters().len());
        for parameter in nominal.type_parameters() {
            let Some(value_type) = substitutions.get(parameter.name()).cloned() else {
                return Err(self.error(
                    RuntimeErrorKind::GenericInstantiation {
                        message: format!(
                            "cannot infer type argument `{}`; provide it explicitly",
                            parameter.name()
                        ),
                    },
                    span,
                ));
            };
            type_arguments.push(value_type);
        }
        for (expected, value) in variant.payload().iter().zip(&payload) {
            let expected = crate::module::substitute_type(expected, &substitutions);
            if !expected.accepts(value) {
                return Err(self.error(
                    RuntimeErrorKind::NominalConstruction {
                        message: format!(
                            "variant payload expects `{expected}`, found {}",
                            value.family_name()
                        ),
                    },
                    span,
                ));
            }
        }
        self.validate_runtime_nominal_constraints(&nominal, &type_arguments, span)?;
        Ok(Some(Value::Variant(Box::new(VariantValue::new(
            nominal.id().clone(),
            type_arguments,
            constructor,
            payload,
        )))))
    }

    fn glob(&mut self, value: &Value, span: Span) -> Eval<Value> {
        if self.host.policy() == EvaluationPolicy::PureV2 {
            return Err(Abort::Refused(Refusal::new(
                RefusalReason::Unsupported,
                "filesystem read",
                span,
            )));
        }
        if self.host.policy() == EvaluationPolicy::Startup {
            return Err(self.error(
                RuntimeErrorKind::RestrictedStartup {
                    capability: RestrictedCapability::FilesystemRead,
                },
                span,
            ));
        }
        let pattern = match value {
            Value::String(pattern) => OsString::from(pattern.as_ref()),
            Value::Path(pattern) => pattern.as_os_str().to_os_string(),
            other => {
                return Err(self.operation(
                    OperationError::UnsupportedOperands {
                        operator: "glob",
                        operands: vec![other.family_name()],
                    },
                    span,
                ));
            }
        };
        let pattern = GlobPattern::parse(&pattern).map_err(|error| {
            self.error(
                RuntimeErrorKind::GlobPattern {
                    message: error.message().to_owned(),
                },
                span,
            )
        })?;
        let mut remaining = DEFAULT_GLOB_ENTRY_LIMIT;
        let paths = pattern.expand(|directory| {
            let entries = self.glob_directory(directory, span, remaining)?;
            remaining -= entries.len();
            Ok::<_, Abort>(entries)
        })?;
        Ok(Value::list(
            paths
                .into_iter()
                .map(|path| Value::Path(NativePath::new(path.into_os_string())))
                .collect(),
        ))
    }

    fn glob_directory(
        &mut self,
        path: &Path,
        span: Span,
        remaining: usize,
    ) -> Eval<Vec<DirectoryEntry>> {
        self.check_cancel(span)?;
        self.charge(span)?;
        let mut stream = self
            .host
            .read_directory(path)
            .map_err(|kind| self.error(kind, span))?;
        let mut entries = Vec::new();
        loop {
            self.check_cancel(span)?;
            self.charge(span)?;
            match stream.next_entry() {
                Ok(Some(_)) if entries.len() == remaining => {
                    return Err(self.error(
                        RuntimeErrorKind::GlobLimitExceeded {
                            limit: DEFAULT_GLOB_ENTRY_LIMIT,
                        },
                        span,
                    ));
                }
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => return Ok(entries),
                Err(error) => {
                    return Err(self.error(RuntimeErrorKind::DirectoryRead(error), span));
                }
            }
        }
    }

    /// Binds already-evaluated `arguments` to a callable's parameters and runs its
    /// body, recording a call frame at `span` on any unwinding error.
    ///
    /// The caller has already resolved the callee, checked it is callable, and
    /// matched its arity; this is the shared body used by an ordinary call
    /// expression and by [`apply_callable`], which applies a callable to runtime
    /// values with no call-expression AST.
    fn runtime_type_substitutions(
        &self,
        function: &CallableValue,
        arguments: &[RuntimeArgument],
        explicit: Option<&[ValueType]>,
        expected_result: Option<&ValueType>,
        span: Span,
    ) -> Eval<BTreeMap<String, ValueType>> {
        let mut substitutions = BTreeMap::new();
        if let Some(explicit) = explicit {
            if explicit.len() != function.type_parameters.len() {
                return Err(self.error(
                    RuntimeErrorKind::GenericInstantiation {
                        message: format!(
                            "expected {} type arguments, found {}",
                            function.type_parameters.len(),
                            explicit.len()
                        ),
                    },
                    span,
                ));
            }
            for (parameter, value_type) in function.type_parameters.iter().zip(explicit) {
                substitutions.insert(parameter.name().to_owned(), value_type.clone());
            }
        } else {
            for (parameter, argument) in function.parameters.iter().zip(arguments) {
                infer_runtime_substitution(
                    &parameter.value_type,
                    &argument.value,
                    &mut substitutions,
                );
            }
            if let (Some(result_type), Some(expected_result)) =
                (&function.result_type, expected_result)
            {
                infer_runtime_type_value(result_type, expected_result, &mut substitutions);
            }
        }

        for parameter in &function.type_parameters {
            let Some(value_type) = substitutions.get(parameter.name()) else {
                return Err(self.error(
                    RuntimeErrorKind::GenericInstantiation {
                        message: format!(
                            "cannot infer type argument `{}`; provide it explicitly",
                            parameter.name()
                        ),
                    },
                    span,
                ));
            };
            for constraint in parameter.constraints() {
                if !self
                    .binding_types
                    .type_satisfies_constraint(value_type, *constraint)
                {
                    return Err(self.error(
                        RuntimeErrorKind::GenericInstantiation {
                            message: format!(
                                "type `{value_type}` does not satisfy `{constraint:?}` for `{}`",
                                parameter.name()
                            ),
                        },
                        span,
                    ));
                }
            }
        }
        Ok(substitutions)
    }

    fn run_call(
        &mut self,
        callable: &Arc<dyn Callable>,
        function: &CallableValue,
        arguments: Vec<RuntimeArgument>,
        span: Span,
        explicit_type_arguments: Option<Vec<ValueType>>,
        expected_result: Option<&ValueType>,
    ) -> Eval<Value> {
        let substitutions = self.runtime_type_substitutions(
            function,
            &arguments,
            explicit_type_arguments.as_deref(),
            expected_result,
            span,
        )?;
        for (parameter, argument) in function.parameters.iter().zip(&arguments) {
            let expected = crate::module::substitute_type(&parameter.value_type, &substitutions);
            if !expected.accepts(&argument.value) {
                return Err(self.error(
                    RuntimeErrorKind::ParameterTypeMismatch {
                        parameter: parameter.name.to_string(),
                        expected,
                        actual: argument.value.family_name(),
                    },
                    argument.span,
                ));
            }
        }
        if !self.budget.enter_call() {
            return Err(self.error(RuntimeErrorKind::ResourceBudgetExceeded, span));
        }

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
        let caller_source = std::mem::replace(&mut self.source, Arc::clone(&function.source));
        let caller_binding_types =
            std::mem::replace(&mut self.binding_types, Arc::clone(&function.binding_types));
        let result = (|| {
            for (parameter, argument) in function.parameters.iter().zip(arguments) {
                let expected =
                    crate::module::substitute_type(&parameter.value_type, &substitutions);
                let matched = if let Some(pattern) = &parameter.pattern {
                    self.pattern_matches(
                        pattern,
                        &argument.value,
                        &mut call_scope,
                        BindingMutability::Mutable,
                        Some(&expected),
                    )?
                } else {
                    call_scope
                        .declare_typed(
                            parameter.name.as_ref(),
                            BindingMutability::Mutable,
                            argument.value,
                            Some(expected),
                        )
                        .expect("parameter names are unique by construction");
                    true
                };
                if !matched {
                    return Err(self.error(RuntimeErrorKind::PatternMismatch, argument.span));
                }
            }

            // The body is entered here, so any error unwinding out of it records a
            // call frame naming this callee and its call site. Everything above
            // (cancellation, argument, arity, not-callable resolution) ran in the
            // caller's context and is deliberately left unframed.
            let frame = CallFrame::new(function.name.as_deref(), span, Arc::clone(&self.source));
            let result_type = function
                .result_type
                .as_ref()
                .map(|result_type| crate::module::substitute_type(result_type, &substitutions));
            self.run_body_in_defining_source(function, result_type.as_ref(), &mut call_scope)
                .map_err(|abort| abort.with_frame(frame))
        })();
        self.source = caller_source;
        self.binding_types = caller_binding_types;
        self.budget.leave_call();
        result
    }

    fn run_body_in_defining_source(
        &mut self,
        function: &CallableValue,
        result_type: Option<&ValueType>,
        scope: &mut ScopeStack,
    ) -> Eval<Value> {
        let caller_result_type =
            std::mem::replace(&mut self.current_result_type, result_type.cloned());
        let outcome: Eval<Flow> = (|| {
            Ok(match &function.body {
                CallableBody::Block(block) => {
                    scope.push();
                    let flow = self.body_statements(&block.statements, scope);
                    scope.pop().expect("the body pushes exactly one frame");
                    flow?
                }
                CallableBody::Expression(chain) => Flow::Fallthrough(Some(FlowValue {
                    value: self.eval_chain_with_expected(chain, scope, result_type)?,
                    span: chain.span(),
                })),
            })
        })();
        self.current_result_type = caller_result_type;
        let outcome = outcome?;

        let result = match outcome {
            Flow::Return(result, _) | Flow::Fallthrough(Some(result)) => result,
            Flow::Fallthrough(None) => FlowValue {
                value: Value::Null,
                span: match &function.body {
                    CallableBody::Block(block) => block.span,
                    CallableBody::Expression(chain) => chain.span(),
                },
            },
            Flow::Break(span) => Err(self.error(
                RuntimeErrorKind::ControlOutsideLoop {
                    control: ControlKind::Break,
                },
                span,
            ))?,
            Flow::Continue(span) => Err(self.error(
                RuntimeErrorKind::ControlOutsideLoop {
                    control: ControlKind::Continue,
                },
                span,
            ))?,
        };

        if let Some(expected) = result_type
            && !expected.accepts(&result.value)
        {
            return Err(self.error(
                RuntimeErrorKind::FunctionResultTypeMismatch {
                    expected: expected.clone(),
                    actual: result.value.family_name(),
                },
                result.span,
            ));
        }
        Ok(result.value)
    }

    /// Resolves a callee. A bare name resolves in scope; any other form is an
    /// ordinary expression. This keeps `$name` the only value-position read.
    fn callee_value(&mut self, callee: &Expression, scope: &mut ScopeStack) -> Eval<Value> {
        if let ExpressionKind::Symbol(identifier) = callee.kind() {
            let name = self.text(identifier.span());
            return self.binding_value(name, scope, callee.span());
        }
        if let ExpressionKind::Qualified(name) = callee.kind() {
            let name = name
                .segments
                .iter()
                .map(|segment| self.text(segment.span()))
                .collect::<Vec<_>>()
                .join("::");
            if let Some(value) = scope.get(&name) {
                return Ok(value.clone());
            }
        }
        self.expression(callee, scope)
    }

    /// Runs a function body, tracking the last expression value as the result.
    fn body_statements(&mut self, statements: &[Statement], scope: &mut ScopeStack) -> Eval<Flow> {
        let mut last = None;
        for (index, statement) in statements.iter().enumerate() {
            let flow = if index + 1 == statements.len()
                && let StatementKind::Job(job) = statement.kind()
            {
                self.charge(statement.span())?;
                let expected = self.current_result_type.clone();
                self.job(job, scope, statement.span(), expected.as_ref())?
            } else {
                self.statement(statement, scope)?
            };
            match flow {
                Flow::Fallthrough(Some(result)) => last = Some(result),
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

fn chain_contains_command_stage(chain: &ConditionalChain) -> bool {
    chain.or_terms().iter().any(|and_chain| {
        and_chain.and_terms().iter().any(|pipeline| {
            pipeline
                .stages()
                .iter()
                .any(|stage| matches!(stage.kind(), StageKind::Command(_)))
        })
    })
}

fn infer_runtime_substitution(
    expected: &ValueType,
    value: &Value,
    substitutions: &mut BTreeMap<String, ValueType>,
) {
    match (expected, value) {
        (ValueType::TypeParameter(name), value) => {
            if let Some(actual) = runtime_value_type(value) {
                substitutions.entry(name.clone()).or_insert(actual);
            }
        }
        (ValueType::List(expected), Value::List(values)) => {
            for value in values.iter() {
                infer_runtime_substitution(expected, value, substitutions);
            }
        }
        (
            ValueType::Nominal {
                id: expected_id,
                arguments: expected_arguments,
            },
            Value::NominalRecord(value),
        ) if expected_id.as_ref() == value.id() => {
            for (expected, actual) in expected_arguments.iter().zip(value.type_arguments()) {
                infer_runtime_type_value(expected, actual, substitutions);
            }
        }
        (
            ValueType::Nominal {
                id: expected_id,
                arguments: expected_arguments,
            },
            Value::Variant(value),
        ) if expected_id.as_ref() == value.id() => {
            for (expected, actual) in expected_arguments.iter().zip(value.type_arguments()) {
                infer_runtime_type_value(expected, actual, substitutions);
            }
        }
        _ => {}
    }
}

fn infer_runtime_type_value(
    expected: &ValueType,
    actual: &ValueType,
    substitutions: &mut BTreeMap<String, ValueType>,
) {
    match (expected, actual) {
        (ValueType::TypeParameter(name), actual) => {
            substitutions
                .entry(name.clone())
                .or_insert_with(|| actual.clone());
        }
        (ValueType::List(expected), ValueType::List(actual)) => {
            infer_runtime_type_value(expected, actual, substitutions);
        }
        (
            ValueType::Nominal {
                id: expected_id,
                arguments: expected_arguments,
            },
            ValueType::Nominal {
                id: actual_id,
                arguments: actual_arguments,
            },
        ) if expected_id == actual_id && expected_arguments.len() == actual_arguments.len() => {
            for (expected, actual) in expected_arguments.iter().zip(actual_arguments) {
                infer_runtime_type_value(expected, actual, substitutions);
            }
        }
        _ => {}
    }
}

fn runtime_value_type(value: &Value) -> Option<ValueType> {
    Some(match value {
        Value::Null => ValueType::Null,
        Value::Bool(_) => ValueType::Bool,
        Value::Int(_) => ValueType::Int,
        Value::Float(_) => ValueType::Float,
        Value::String(_) => ValueType::String,
        Value::Bytes(_) => ValueType::Bytes,
        Value::Path(_) => ValueType::Path,
        Value::Duration(_) => ValueType::Duration,
        Value::ByteSize(_) => ValueType::ByteSize,
        Value::List(values) => ValueType::List(Box::new(runtime_value_type(values.first()?)?)),
        Value::Record(_) => ValueType::Record,
        Value::Table(_) => ValueType::Table,
        Value::Range(_) => ValueType::Range,
        Value::Status(_) => ValueType::Status,
        Value::Error(_) => ValueType::Error,
        Value::Callable(callable) if callable.family() == "closure" => ValueType::Closure,
        Value::Callable(_) => ValueType::Function,
        Value::NominalRecord(value) => ValueType::Nominal {
            id: Box::new(value.id().clone()),
            arguments: value.type_arguments().to_vec(),
        },
        Value::Variant(value) => ValueType::Nominal {
            id: Box::new(value.id().clone()),
            arguments: value.type_arguments().to_vec(),
        },
    })
}

/// The single runtime callable: a named function or an anonymous closure.
#[derive(Clone)]
struct CallableValue {
    /// `Some` for a `def` function, `None` for a closure.
    name: Option<Arc<str>>,
    parameters: Vec<CallableParameter>,
    type_parameters: Vec<ResolvedTypeParameter>,
    body: CallableBody,
    captured: ScopeStack,
    source: Arc<SourceFile>,
    binding_types: Arc<RuntimeBindingTypes>,
    /// `Some`, including `Any`, for a named function; `None` for a closure.
    result_type: Option<ValueType>,
    location: String,
    inspection: Option<crate::help::FunctionInspection>,
    origin_span: Span,
}

#[derive(Clone, Debug)]
pub(crate) struct CallableSnapshot {
    pub(crate) name: Option<String>,
    pub(crate) parameters: Vec<(String, ValueType)>,
    pub(crate) captured: ScopeStack,
    pub(crate) source: SourceFile,
    pub(crate) result_type: Option<ValueType>,
    pub(crate) location: String,
    pub(crate) origin_span: Span,
}

pub(crate) fn snapshot_callable(callable: &Arc<dyn Callable>) -> Option<CallableSnapshot> {
    let callable = callable.as_any().downcast_ref::<CallableValue>()?;
    Some(CallableSnapshot {
        name: callable.name.as_deref().map(str::to_owned),
        parameters: callable
            .parameters
            .iter()
            .map(|parameter| (parameter.name.to_string(), parameter.value_type.clone()))
            .collect(),
        captured: callable.captured.clone(),
        source: callable.source.as_ref().clone(),
        result_type: callable.result_type.clone(),
        location: callable.location.clone(),
        origin_span: callable.origin_span,
    })
}

pub(crate) fn restore_callable(snapshot: CallableSnapshot) -> Result<Arc<dyn Callable>, String> {
    let parsed = match flash_syntax::parse(&snapshot.source) {
        flash_syntax::ParseOutcome::Complete(script) => script,
        flash_syntax::ParseOutcome::Incomplete(_) => {
            return Err("callable source is incomplete".to_owned());
        }
        flash_syntax::ParseOutcome::Invalid(_) => {
            return Err("callable source is invalid".to_owned());
        }
    };
    let body = find_callable_body(&parsed, &snapshot)
        .ok_or_else(|| "callable definition is absent from its source".to_owned())?;
    let binding_types = RuntimeBindingTypes::analyze_source(&snapshot.source, &parsed)
        .map_err(|_| "callable source types cannot be restored".to_owned())?;
    let source = Arc::new(snapshot.source);
    let inspection = snapshot.name.as_deref().and_then(|_| {
        binding_types
            .function_signature(source.id(), snapshot.origin_span)
            .cloned()
            .map(|signature| crate::help::FunctionInspection::new(signature, &source))
    });
    let callable = CallableValue {
        name: snapshot.name.map(Arc::from),
        parameters: snapshot
            .parameters
            .into_iter()
            .map(|(name, value_type)| CallableParameter {
                name: Arc::from(name),
                value_type,
                pattern: None,
            })
            .collect(),
        type_parameters: Vec::new(),
        body,
        captured: snapshot.captured,
        source,
        binding_types: Arc::new(binding_types),
        result_type: snapshot.result_type,
        location: snapshot.location,
        inspection,
        origin_span: snapshot.origin_span,
    };
    Ok(Arc::new(callable))
}

fn find_callable_body(script: &Script, snapshot: &CallableSnapshot) -> Option<CallableBody> {
    script
        .statements()
        .iter()
        .find_map(|statement| find_callable_in_statement(statement, snapshot))
}

fn find_callable_in_statement(
    statement: &Statement,
    snapshot: &CallableSnapshot,
) -> Option<CallableBody> {
    match statement.kind() {
        StatementKind::Import(_)
        | StatementKind::ModuleImport(_)
        | StatementKind::ModuleExport(_)
        | StatementKind::NominalType(_)
        | StatementKind::VariantType(_) => None,
        StatementKind::Declaration(declaration) => {
            find_callable_in_expression(&declaration.value, snapshot)
        }
        StatementKind::Assignment(assignment) => {
            find_callable_in_expression(&assignment.value, snapshot)
        }
        StatementKind::Environment(EnvironmentStatement::Export { value, .. }) => {
            find_callable_in_expression(value, snapshot)
        }
        StatementKind::Environment(EnvironmentStatement::Unset { .. }) => None,
        StatementKind::Function(definition) => {
            let matches = snapshot.name.as_deref().is_some_and(|name| {
                definition.name.span() == snapshot.origin_span
                    && snapshot
                        .source
                        .slice(definition.name.span())
                        .is_ok_and(|candidate| candidate == name)
            });
            matches
                .then(|| CallableBody::Block(definition.body.clone()))
                .or_else(|| find_callable_in_block(&definition.body, snapshot))
        }
        StatementKind::If(statement) => find_callable_in_chain(&statement.condition, snapshot)
            .or_else(|| find_callable_in_block(&statement.then_block, snapshot))
            .or_else(|| {
                statement
                    .else_branch
                    .as_ref()
                    .and_then(|branch| match branch {
                        ElseBranch::Block(block) => find_callable_in_block(block, snapshot),
                        ElseBranch::If(statement) => {
                            find_callable_in_if(statement.kind(), snapshot)
                        }
                    })
            }),
        StatementKind::While(statement) => find_callable_in_chain(&statement.condition, snapshot)
            .or_else(|| find_callable_in_block(&statement.body, snapshot)),
        StatementKind::For(statement) => find_callable_in_expression(&statement.iterable, snapshot)
            .or_else(|| find_callable_in_block(&statement.body, snapshot)),
        StatementKind::Match(statement) => find_callable_in_expression(&statement.value, snapshot)
            .or_else(|| {
                statement.arms.iter().find_map(|arm| {
                    arm.guard
                        .as_ref()
                        .and_then(|guard| find_callable_in_expression(guard, snapshot))
                        .or_else(|| find_callable_in_block(&arm.body, snapshot))
                })
            }),
        StatementKind::Try(statement) => find_callable_in_block(&statement.try_block, snapshot)
            .or_else(|| find_callable_in_block(&statement.catch_block, snapshot)),
        StatementKind::Throw(expression) => find_callable_in_expression(expression, snapshot),
        StatementKind::Control(ControlTransfer::Return(Some(expression))) => {
            find_callable_in_expression(expression, snapshot)
        }
        StatementKind::Control(
            ControlTransfer::Break | ControlTransfer::Continue | ControlTransfer::Return(None),
        ) => None,
        StatementKind::Job(job) => find_callable_in_chain(&job.chain, snapshot),
    }
}

fn find_callable_in_if(
    statement: &IfStatement,
    snapshot: &CallableSnapshot,
) -> Option<CallableBody> {
    find_callable_in_chain(&statement.condition, snapshot)
        .or_else(|| find_callable_in_block(&statement.then_block, snapshot))
        .or_else(|| {
            statement
                .else_branch
                .as_ref()
                .and_then(|branch| match branch {
                    ElseBranch::Block(block) => find_callable_in_block(block, snapshot),
                    ElseBranch::If(statement) => find_callable_in_if(statement.kind(), snapshot),
                })
        })
}

fn find_callable_in_block(block: &Block, snapshot: &CallableSnapshot) -> Option<CallableBody> {
    block
        .statements
        .iter()
        .find_map(|statement| find_callable_in_statement(statement, snapshot))
}

fn find_callable_in_chain(
    chain: &ConditionalChain,
    snapshot: &CallableSnapshot,
) -> Option<CallableBody> {
    chain.or_terms().iter().find_map(|and_chain| {
        and_chain.and_terms().iter().find_map(|pipeline| {
            pipeline
                .stages()
                .iter()
                .find_map(|stage| match stage.kind() {
                    StageKind::Expression(expression) => {
                        find_callable_in_expression(expression, snapshot)
                    }
                    StageKind::Command(command) => {
                        find_callable_in_word(command.head.word(), snapshot).or_else(|| {
                            command.items.iter().find_map(|item| match item.kind() {
                                CommandItemKind::Word(word) => {
                                    find_callable_in_word(word, snapshot)
                                }
                                CommandItemKind::Closure(closure) => {
                                    find_callable_in_closure(closure, snapshot)
                                }
                                CommandItemKind::Redirection(redirection) => {
                                    match redirection.kind() {
                                        RedirectionKind::Input { target, .. }
                                        | RedirectionKind::File(flash_syntax::FileRedirection {
                                            target,
                                            ..
                                        }) => find_callable_in_word(target, snapshot),
                                        RedirectionKind::Duplicate { .. }
                                        | RedirectionKind::Close { .. } => None,
                                    }
                                }
                                CommandItemKind::Spread(_) => None,
                            })
                        })
                    }
                })
        })
    })
}

fn find_callable_in_closure(
    closure: &Closure,
    snapshot: &CallableSnapshot,
) -> Option<CallableBody> {
    (snapshot.name.is_none() && closure.span == snapshot.origin_span)
        .then(|| CallableBody::Expression(closure.body.clone()))
        .or_else(|| find_callable_in_chain(&closure.body, snapshot))
}

fn find_callable_in_expression(
    expression: &Expression,
    snapshot: &CallableSnapshot,
) -> Option<CallableBody> {
    match expression.kind() {
        ExpressionKind::Literal(literal) => match literal.kind() {
            LiteralKind::DoubleQuoted(parts) => parts
                .iter()
                .find_map(|part| find_callable_in_word_part(part, snapshot)),
            _ => None,
        },
        ExpressionKind::Variable(_) | ExpressionKind::Symbol(_) | ExpressionKind::Qualified(_) => {
            None
        }
        ExpressionKind::List(elements) => elements
            .iter()
            .find_map(|element| find_callable_in_expression(element, snapshot)),
        ExpressionKind::Record(entries) => entries.iter().find_map(|entry| {
            let key = match &entry.key {
                RecordKey::DoubleQuoted(part) => find_callable_in_word_part(part, snapshot),
                RecordKey::Identifier(_) | RecordKey::SingleQuoted(_) => None,
            };
            key.or_else(|| find_callable_in_expression(&entry.value, snapshot))
        }),
        ExpressionKind::NominalRecord(record) => record
            .fields
            .iter()
            .find_map(|field| find_callable_in_expression(&field.value, snapshot)),
        ExpressionKind::Closure(closure) => find_callable_in_closure(closure, snapshot),
        ExpressionKind::CommandSubstitution(substitution) => {
            find_callable_in_chain(substitution.chain(), snapshot)
        }
        ExpressionKind::GroupedJob(chain) => find_callable_in_chain(chain, snapshot),
        ExpressionKind::Call(call) => {
            find_callable_in_expression(&call.callee, snapshot).or_else(|| {
                call.arguments
                    .iter()
                    .find_map(|argument| find_callable_in_expression(argument, snapshot))
            })
        }
        ExpressionKind::Index(index) => find_callable_in_expression(&index.target, snapshot)
            .or_else(|| find_callable_in_expression(&index.index, snapshot)),
        ExpressionKind::Member(member) => find_callable_in_expression(&member.target, snapshot),
        ExpressionKind::Unary(unary) => find_callable_in_expression(&unary.operand, snapshot),
        ExpressionKind::Binary(binary) => find_callable_in_expression(&binary.left, snapshot)
            .or_else(|| find_callable_in_expression(&binary.right, snapshot)),
    }
}

fn find_callable_in_word(word: &Word, snapshot: &CallableSnapshot) -> Option<CallableBody> {
    word.parts()
        .iter()
        .find_map(|part| find_callable_in_word_part(part, snapshot))
}

fn find_callable_in_word_part(
    part: &WordPart,
    snapshot: &CallableSnapshot,
) -> Option<CallableBody> {
    match part.kind() {
        WordPartKind::DoubleQuoted(parts) => parts
            .iter()
            .find_map(|part| find_callable_in_word_part(part, snapshot)),
        WordPartKind::BracedInterpolation(expression) => {
            find_callable_in_expression(expression, snapshot)
        }
        WordPartKind::CommandSubstitution(substitution) => {
            find_callable_in_chain(substitution.chain(), snapshot)
        }
        WordPartKind::Bare
        | WordPartKind::BareEscape
        | WordPartKind::SingleQuoted
        | WordPartKind::DoubleText
        | WordPartKind::DoubleEscape
        | WordPartKind::Variable(_) => None,
    }
}

#[derive(Clone)]
struct CallableParameter {
    name: Arc<str>,
    value_type: ValueType,
    pattern: Option<Pattern>,
}

struct RuntimeArgument {
    value: Value,
    span: Span,
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

    fn inspection(&self) -> Option<&crate::help::FunctionInspection> {
        self.inspection.as_ref()
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flash_platform::{DirectoryEntry, DirectoryEntryKind, DirectoryReadError};
    use flash_syntax::{ParseOutcome, SourceId, parse};

    use super::*;

    struct DirectoryHost {
        environment: Environment,
        advances: Arc<AtomicUsize>,
    }

    impl EvaluationHost for DirectoryHost {
        fn environment(&mut self) -> &mut Environment {
            &mut self.environment
        }

        fn current_status(&self) -> Option<&Status> {
            None
        }

        fn policy(&self) -> EvaluationPolicy {
            EvaluationPolicy::General
        }

        fn read_directory(
            &mut self,
            _path: &Path,
        ) -> Result<Box<dyn DirectoryStream>, RuntimeErrorKind> {
            Ok(Box::new(CountingDirectoryStream {
                entries: vec![
                    DirectoryEntry::new("a.fsh".into(), DirectoryEntryKind::File, Some(0)),
                    DirectoryEntry::new("b.fsh".into(), DirectoryEntryKind::File, Some(0)),
                ]
                .into_iter(),
                advances: Arc::clone(&self.advances),
            }))
        }

        fn execute_chain(
            &mut self,
            _chain: &ConditionalChain,
            _scope: &mut ScopeStack,
            _context: EvaluationContext,
        ) -> Result<Status, Abort> {
            unreachable!("glob tests do not execute commands")
        }

        fn capture_chain(
            &mut self,
            _chain: &ConditionalChain,
            _scope: &mut ScopeStack,
            _span: Span,
            _position: CapturePosition,
            _context: EvaluationContext,
        ) -> Result<CapturedChain, Abort> {
            unreachable!("glob tests do not capture commands")
        }
    }

    struct CancellingCaptureHost {
        environment: Environment,
    }

    impl EvaluationHost for CancellingCaptureHost {
        fn environment(&mut self) -> &mut Environment {
            &mut self.environment
        }

        fn current_status(&self) -> Option<&Status> {
            None
        }

        fn policy(&self) -> EvaluationPolicy {
            EvaluationPolicy::General
        }

        fn read_directory(
            &mut self,
            _path: &Path,
        ) -> Result<Box<dyn DirectoryStream>, RuntimeErrorKind> {
            unreachable!("capture cancellation does not read directories")
        }

        fn execute_chain(
            &mut self,
            _chain: &ConditionalChain,
            _scope: &mut ScopeStack,
            _context: EvaluationContext,
        ) -> Result<Status, Abort> {
            unreachable!("capture cancellation does not execute an uncaptured chain")
        }

        fn capture_chain(
            &mut self,
            _chain: &ConditionalChain,
            _scope: &mut ScopeStack,
            span: Span,
            _position: CapturePosition,
            context: EvaluationContext,
        ) -> Result<CapturedChain, Abort> {
            assert!(
                context.cancel.is_cancelled(),
                "the active evaluator token reaches recursive capture"
            );
            Err(Abort::Cancelled(Cancellation::new(
                context.cancel.reason(),
                span,
            )))
        }
    }

    struct StoppedHost {
        environment: Environment,
    }

    impl EvaluationHost for StoppedHost {
        fn environment(&mut self) -> &mut Environment {
            &mut self.environment
        }

        fn current_status(&self) -> Option<&Status> {
            None
        }

        fn policy(&self) -> EvaluationPolicy {
            EvaluationPolicy::General
        }

        fn read_directory(
            &mut self,
            _path: &Path,
        ) -> Result<Box<dyn DirectoryStream>, RuntimeErrorKind> {
            unreachable!("stopped control test does not read directories")
        }

        fn execute_chain(
            &mut self,
            _chain: &ConditionalChain,
            _scope: &mut ScopeStack,
            _context: EvaluationContext,
        ) -> Result<Status, Abort> {
            Err(Abort::Stopped(
                crate::job::JobId::new(1).expect("test job identity is nonzero"),
            ))
        }

        fn capture_chain(
            &mut self,
            _chain: &ConditionalChain,
            _scope: &mut ScopeStack,
            _span: Span,
            _position: CapturePosition,
            _context: EvaluationContext,
        ) -> Result<CapturedChain, Abort> {
            unreachable!("stopped control test does not capture commands")
        }
    }

    #[derive(Debug)]
    struct CountingDirectoryStream {
        entries: std::vec::IntoIter<DirectoryEntry>,
        advances: Arc<AtomicUsize>,
    }

    impl DirectoryStream for CountingDirectoryStream {
        fn next_entry(&mut self) -> Result<Option<DirectoryEntry>, DirectoryReadError> {
            self.advances.fetch_add(1, Ordering::SeqCst);
            Ok(self.entries.next())
        }
    }

    fn evaluate_glob(
        limits: &EvalLimits,
        advances: Arc<AtomicUsize>,
    ) -> Result<HostedEvaluationOutcome, HostedEvaluationFailure> {
        let source = Arc::new(SourceFile::new(
            SourceId::new(1),
            "glob.fsh",
            "glob('*.fsh')",
        ));
        let script = match parse(&source) {
            ParseOutcome::Complete(script) => script,
            other => panic!("glob fixture did not parse: {other:?}"),
        };
        let mut scope = ScopeStack::new();
        let mut host = DirectoryHost {
            environment: Environment::new(),
            advances,
        };
        evaluate_with_host(
            &script,
            source,
            &mut scope,
            limits,
            Arc::new(RuntimeBindingTypes::default()),
            &mut host,
        )
    }

    #[test]
    fn glob_cancellation_aborts_mid_walk_without_returning_a_partial_list() {
        let checks = Arc::new(AtomicUsize::new(0));
        let token_checks = Arc::clone(&checks);
        let token =
            CancellationToken::from_fn(move || token_checks.fetch_add(1, Ordering::SeqCst) >= 3);
        let advances = Arc::new(AtomicUsize::new(0));
        let limits = EvalLimits::new(token, ResourceBudget::unlimited());

        assert!(matches!(
            evaluate_glob(&limits, Arc::clone(&advances)),
            Ok(HostedEvaluationOutcome::Cancelled(_))
        ));
        assert_eq!(advances.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn recursive_capture_cancellation_remains_a_distinct_hosted_outcome() {
        let source = Arc::new(SourceFile::new(
            SourceId::new(1),
            "capture.fsh",
            "def capture() { return $(^tool) }\ncapture()",
        ));
        let script = match parse(&source) {
            ParseOutcome::Complete(script) => script,
            other => panic!("capture fixture did not parse: {other:?}"),
        };
        let polls = Arc::new(AtomicUsize::new(0));
        let token_polls = Arc::clone(&polls);
        let limits = EvalLimits::new(
            CancellationToken::from_fn(move || token_polls.fetch_add(1, Ordering::SeqCst) >= 1),
            ResourceBudget::unlimited(),
        );
        let mut scope = ScopeStack::new();
        let mut host = CancellingCaptureHost {
            environment: Environment::new(),
        };

        let outcome = match evaluate_with_host(
            &script,
            Arc::clone(&source),
            &mut scope,
            &limits,
            Arc::new(RuntimeBindingTypes::default()),
            &mut host,
        ) {
            Ok(outcome) => outcome,
            Err(_) => panic!("cancellation must not become a hosted evaluation failure"),
        };
        let HostedEvaluationOutcome::Cancelled(cancellation) = outcome else {
            panic!("recursive capture should preserve cancellation");
        };
        assert_eq!(cancellation.reason(), CancelReason::Requested);
        assert_eq!(source.slice(cancellation.span()).unwrap(), "$(^tool)");
        assert_eq!(polls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stopped_job_control_bypasses_language_catch() {
        let source = Arc::new(SourceFile::new(
            SourceId::new(1),
            "stopped.fsh",
            "try { pwd } catch error { throw \"caught\" }",
        ));
        let script = match parse(&source) {
            ParseOutcome::Complete(script) => script,
            other => panic!("stopped fixture did not parse: {other:?}"),
        };
        let mut scope = ScopeStack::new();
        let mut host = StoppedHost {
            environment: Environment::new(),
        };

        let outcome = evaluate_with_host(
            &script,
            source,
            &mut scope,
            &EvalLimits::default(),
            Arc::new(RuntimeBindingTypes::default()),
            &mut host,
        )
        .unwrap_or_else(|_| panic!("stopped control must not become a runtime failure"));
        assert!(matches!(outcome, HostedEvaluationOutcome::Stopped(job) if job.get() == 1));
    }

    #[test]
    fn glob_charges_each_walk_step_to_the_evaluation_budget() {
        let advances = Arc::new(AtomicUsize::new(0));
        let limits = EvalLimits::new(CancellationToken::never(), ResourceBudget::steps(5));

        assert!(matches!(
            evaluate_glob(&limits, Arc::clone(&advances)),
            Err(HostedEvaluationFailure::Runtime(error))
                if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded)
        ));
        assert_eq!(advances.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn glob_entry_limit_fails_before_a_partial_batch_can_escape() {
        let source = Arc::new(SourceFile::new(SourceId::new(1), "glob.fsh", "glob('*')"));
        let span = source.span(0..source.text().len()).unwrap();
        let advances = Arc::new(AtomicUsize::new(0));
        let mut host = DirectoryHost {
            environment: Environment::new(),
            advances: Arc::clone(&advances),
        };
        let mut budget = ResourceBudget::unlimited();
        let mut evaluator = Evaluator {
            source,
            binding_types: Arc::new(RuntimeBindingTypes::default()),
            current_result_type: None,
            cancel: CancellationToken::never(),
            budget: &mut budget,
            host: &mut host,
        };

        assert!(matches!(
            evaluator.glob_directory(Path::new("."), span, 1),
            Err(Abort::Error(error))
                if matches!(
                    error.kind(),
                    RuntimeErrorKind::GlobLimitExceeded {
                        limit: DEFAULT_GLOB_ENTRY_LIMIT
                    }
                )
        ));
        assert_eq!(advances.load(Ordering::SeqCst), 2);
    }
}
