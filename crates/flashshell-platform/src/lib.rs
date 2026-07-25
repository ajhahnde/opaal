#![forbid(unsafe_code)]

//! Platform-neutral capability contracts for FlashShell.
//!
//! Host adapters and the future FlashOS adapter implement [`Platform`] behind
//! this boundary. The engine never assumes a POSIX host: it asks a platform
//! which capabilities it supports and receives a precise [`PlatformError`] when
//! a capability is absent, rather than silently emulating unsafe behaviour.
//!
//! Platform calls are synchronous and blocking; concurrency (for example
//! draining a pipe while a child runs) is arranged by the caller with threads,
//! not by an async runtime. Byte-preserving data crosses the boundary as the
//! standard [`std::ffi::OsStr`] / [`std::path::Path`] family so native argv,
//! environment, and path bytes survive without lossy UTF-8 conversion.

use std::any::Any;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};

/// One platform capability group an adapter either supports or does not.
///
/// A platform that cannot honour a capability (for example a bare-metal target
/// without process groups) reports it as unsupported, and the runtime turns the
/// resulting [`PlatformError::Unsupported`] into a precise feature diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Environment snapshot and per-child mutation.
    Environment,
    /// Working directory and path resolution.
    WorkingDirectory,
    /// File open, duplication, and close actions on child descriptors.
    FileActions,
    /// Anonymous pipe creation for pipeline plumbing.
    Pipes,
    /// Direct argv process spawn, exec, and wait.
    ProcessSpawn,
    /// Process-group creation and membership.
    ProcessGroups,
    /// Foreground terminal ownership handoff.
    ForegroundTerminal,
    /// Signal delivery and cancellation.
    Signals,
    /// Terminal size and TTY detection.
    TerminalInfo,
    /// A monotonic clock source.
    MonotonicClock,
    /// Home, config, and cache directory discovery.
    StandardDirectories,
}

impl Capability {
    /// Every capability, in declaration order.
    pub const ALL: [Capability; 11] = [
        Capability::Environment,
        Capability::WorkingDirectory,
        Capability::FileActions,
        Capability::Pipes,
        Capability::ProcessSpawn,
        Capability::ProcessGroups,
        Capability::ForegroundTerminal,
        Capability::Signals,
        Capability::TerminalInfo,
        Capability::MonotonicClock,
        Capability::StandardDirectories,
    ];

    /// The single set bit that represents this capability.
    const fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

/// A set of supported [`Capability`] values.
///
/// Backed by a fixed-width bitset so it is `Copy`, allocation-free, and usable
/// in `const` contexts — the shape the future bare-metal FlashOS adapter needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Capabilities {
    bits: u16,
}

impl Capabilities {
    /// The empty set — nothing supported.
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    /// The full set — every capability supported.
    pub const fn full() -> Self {
        let mut bits = 0u16;
        let mut index = 0;
        while index < Capability::ALL.len() {
            bits |= Capability::ALL[index].bit();
            index += 1;
        }
        Self { bits }
    }

    /// This set with `capability` added; adding a present capability is a no-op.
    #[must_use]
    pub const fn with(self, capability: Capability) -> Self {
        Self {
            bits: self.bits | capability.bit(),
        }
    }

    /// Whether `capability` is in the set.
    pub const fn supports(self, capability: Capability) -> bool {
        self.bits & capability.bit() != 0
    }
}

/// A platform capability that failed to be satisfied.
///
/// [`Unsupported`](PlatformError::Unsupported) is a permanent gap: the platform
/// can never provide the capability, and the runtime reports a feature
/// diagnostic. [`Unavailable`](PlatformError::Unavailable) is transient: the
/// capability exists in principle but cannot be used right now (for example a
/// clock source that has not started), and carries a human-readable reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlatformError {
    /// The platform does not provide `capability` at all.
    Unsupported {
        /// The capability that is permanently absent.
        capability: Capability,
    },
    /// The platform provides `capability` but it cannot be used right now.
    Unavailable {
        /// The capability that is temporarily unusable.
        capability: Capability,
        /// A human-readable reason the capability is unavailable.
        reason: String,
    },
}

impl fmt::Display for PlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlatformError::Unsupported { capability } => {
                write!(
                    formatter,
                    "platform capability {capability:?} is not supported"
                )
            }
            PlatformError::Unavailable { capability, reason } => write!(
                formatter,
                "platform capability {capability:?} is unavailable: {reason}",
            ),
        }
    }
}

impl std::error::Error for PlatformError {}

/// One uniquely owned descriptor resource returned by a platform adapter.
///
/// The runtime treats endpoints as opaque resources and can only move, borrow,
/// or drop them. An adapter downcasts endpoints it created when installing the
/// final descriptor map for a child.
pub trait DescriptorEndpoint: Send + fmt::Debug {
    /// Adapter-private concrete endpoint access for the matching spawn adapter.
    fn as_any(&self) -> &dyn Any;
}

/// The two uniquely owned endpoints of one anonymous byte pipe.
#[derive(Debug)]
pub struct PipeEndpoints {
    reader: Box<dyn DescriptorEndpoint>,
    writer: Box<dyn DescriptorEndpoint>,
}

impl PipeEndpoints {
    /// Build one pipe from its read and write endpoint owners.
    pub fn new(reader: Box<dyn DescriptorEndpoint>, writer: Box<dyn DescriptorEndpoint>) -> Self {
        Self { reader, writer }
    }

    /// Split the pipe into its unique read and write endpoint owners.
    pub fn into_parts(self) -> (Box<dyn DescriptorEndpoint>, Box<dyn DescriptorEndpoint>) {
        (self.reader, self.writer)
    }
}

/// Failure while creating an anonymous byte pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PipeError {
    /// The platform cannot satisfy the pipe capability.
    Platform(PlatformError),
    /// The host rejected or could not complete pipe creation.
    Operation {
        /// Stable I/O error category from the host adapter.
        kind: io::ErrorKind,
        /// Human-readable host error text.
        message: String,
    },
}

impl fmt::Display for PipeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => error.fmt(formatter),
            Self::Operation { message, .. } => write!(formatter, "pipe creation failed: {message}"),
        }
    }
}

impl std::error::Error for PipeError {}

impl From<PlatformError> for PipeError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

/// Failure while draining bytes from an owned descriptor endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DescriptorReadError {
    /// The platform cannot satisfy pipe-backed descriptor reads.
    Platform(PlatformError),
    /// The endpoint was not created by the adapter asked to read it.
    InvalidEndpoint,
    /// The host read operation failed.
    Operation {
        /// Stable I/O error category from the host adapter.
        kind: io::ErrorKind,
        /// Human-readable host error text.
        message: String,
    },
}

impl fmt::Display for DescriptorReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => error.fmt(formatter),
            Self::InvalidEndpoint => {
                formatter.write_str("descriptor endpoint belongs to another platform adapter")
            }
            Self::Operation { message, .. } => {
                write!(formatter, "descriptor read failed: {message}")
            }
        }
    }
}

impl std::error::Error for DescriptorReadError {}

impl From<PlatformError> for DescriptorReadError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

/// One byte-preserving logical working-directory resolution request.
#[derive(Clone, Copy, Debug)]
pub struct WorkingDirectoryRequest<'a> {
    path: &'a Path,
    cwd: &'a Path,
}

impl<'a> WorkingDirectoryRequest<'a> {
    /// Resolve `path`, interpreting a relative path against `cwd`.
    pub const fn new(path: &'a Path, cwd: &'a Path) -> Self {
        Self { path, cwd }
    }

    /// The requested native path.
    pub const fn path(self) -> &'a Path {
        self.path
    }

    /// The current logical working directory.
    pub const fn cwd(self) -> &'a Path {
        self.cwd
    }
}

/// Failure while resolving and validating a logical working directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkingDirectoryError {
    /// The platform cannot satisfy working-directory resolution.
    Platform(PlatformError),
    /// The host rejected the path or it did not name a directory.
    Operation {
        /// Stable I/O error category from the host adapter.
        kind: io::ErrorKind,
        /// Human-readable host error text.
        message: String,
    },
}

impl fmt::Display for WorkingDirectoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => error.fmt(formatter),
            Self::Operation { message, .. } => {
                write!(formatter, "working-directory resolution failed: {message}")
            }
        }
    }
}

impl std::error::Error for WorkingDirectoryError {}

impl From<PlatformError> for WorkingDirectoryError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

/// How a redirection target is opened for one child descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOpenMode {
    /// Open an existing file for reading.
    Read,
    /// Create or replace a file for writing.
    WriteTruncate,
    /// Create or append to a file for writing.
    WriteAppend,
}

/// One byte-preserving file-open request for redirection setup.
#[derive(Clone, Copy, Debug)]
pub struct FileOpenRequest<'a> {
    path: &'a Path,
    cwd: &'a Path,
    mode: FileOpenMode,
}

impl<'a> FileOpenRequest<'a> {
    /// Build a file-open request relative to the stage working directory.
    pub const fn new(path: &'a Path, cwd: &'a Path, mode: FileOpenMode) -> Self {
        Self { path, cwd, mode }
    }

    /// The native redirection target.
    pub const fn path(self) -> &'a Path {
        self.path
    }

    /// The working directory used for a relative target.
    pub const fn cwd(self) -> &'a Path {
        self.cwd
    }

    /// The requested read, truncate, or append semantics.
    pub const fn mode(self) -> FileOpenMode {
        self.mode
    }
}

/// Failure while preparing an owned descriptor for a redirection action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileActionError {
    /// The platform cannot satisfy file actions.
    Platform(PlatformError),
    /// The host rejected an open or inherited-descriptor duplication.
    Operation {
        /// Stable I/O error category from the host adapter.
        kind: io::ErrorKind,
        /// Human-readable host error text.
        message: String,
    },
}

impl fmt::Display for FileActionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => error.fmt(formatter),
            Self::Operation { message, .. } => {
                write!(formatter, "redirection setup failed: {message}")
            }
        }
    }
}

impl std::error::Error for FileActionError {}

impl From<PlatformError> for FileActionError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

/// One entry in the final logical descriptor map passed to a child.
#[derive(Clone, Copy, Debug)]
pub struct ChildDescriptor<'a> {
    target: u32,
    endpoint: &'a dyn DescriptorEndpoint,
}

impl<'a> ChildDescriptor<'a> {
    /// Map `endpoint` onto the child's descriptor number `target`.
    pub const fn new(target: u32, endpoint: &'a dyn DescriptorEndpoint) -> Self {
        Self { target, endpoint }
    }

    /// The descriptor number visible in the child.
    pub const fn target(self) -> u32 {
        self.target
    }

    /// The opaque owned resource borrowed for this spawn.
    pub const fn endpoint(self) -> &'a dyn DescriptorEndpoint {
        self.endpoint
    }
}

/// A structurally valid request to execute one program directly with argv.
///
/// The first argv entry is explicit rather than inferred from `executable`.
/// Environment entries describe the complete child environment, not a delta;
/// adapters clear their inherited environment before installing these entries.
#[derive(Clone, Copy, Debug)]
pub struct SpawnRequest<'a> {
    executable: &'a Path,
    argv: &'a [OsString],
    environment: &'a [(OsString, OsString)],
    cwd: &'a Path,
    descriptors: &'a [ChildDescriptor<'a>],
    closed_descriptors: &'a [u32],
}

impl<'a> SpawnRequest<'a> {
    /// Build a direct-spawn request, rejecting an absent argv zero.
    pub fn new(
        executable: &'a Path,
        argv: &'a [OsString],
        environment: &'a [(OsString, OsString)],
        cwd: &'a Path,
    ) -> Result<Self, SpawnRequestError> {
        if argv.is_empty() {
            return Err(SpawnRequestError::EmptyArgv);
        }

        Ok(Self {
            executable,
            argv,
            environment,
            cwd,
            descriptors: &[],
            closed_descriptors: &[],
        })
    }

    /// Attach the final child descriptor mappings to this request.
    ///
    /// A target may occur only once. Two targets may deliberately borrow the
    /// same endpoint, as required for a merged stdout-and-stderr pipeline.
    pub fn with_descriptors(
        mut self,
        descriptors: &'a [ChildDescriptor<'a>],
    ) -> Result<Self, SpawnRequestError> {
        for (index, mapping) in descriptors.iter().enumerate() {
            if descriptors[..index]
                .iter()
                .any(|prior| prior.target == mapping.target)
            {
                return Err(SpawnRequestError::DuplicateDescriptor(mapping.target));
            }
            if self.closed_descriptors.contains(&mapping.target) {
                return Err(SpawnRequestError::MappedAndClosedDescriptor(mapping.target));
            }
        }
        self.descriptors = descriptors;
        Ok(self)
    }

    /// Attach descriptor numbers that must be closed in the child.
    pub fn with_closed_descriptors(
        mut self,
        closed_descriptors: &'a [u32],
    ) -> Result<Self, SpawnRequestError> {
        for (index, descriptor) in closed_descriptors.iter().enumerate() {
            if closed_descriptors[..index].contains(descriptor) {
                return Err(SpawnRequestError::DuplicateClosedDescriptor(*descriptor));
            }
            if self
                .descriptors
                .iter()
                .any(|mapping| mapping.target == *descriptor)
            {
                return Err(SpawnRequestError::MappedAndClosedDescriptor(*descriptor));
            }
        }
        self.closed_descriptors = closed_descriptors;
        Ok(self)
    }

    /// The resolved executable path passed directly to the host.
    pub const fn executable(self) -> &'a Path {
        self.executable
    }

    /// The complete native argv, including explicit argv zero.
    pub const fn argv(self) -> &'a [OsString] {
        self.argv
    }

    /// The complete native child environment.
    pub const fn environment(self) -> &'a [(OsString, OsString)] {
        self.environment
    }

    /// The child's working directory.
    pub const fn cwd(self) -> &'a Path {
        self.cwd
    }

    /// The final logical descriptors installed for this child.
    pub const fn descriptors(self) -> &'a [ChildDescriptor<'a>] {
        self.descriptors
    }

    /// The child descriptor numbers explicitly closed before execution.
    pub const fn closed_descriptors(self) -> &'a [u32] {
        self.closed_descriptors
    }
}

/// A spawn request that cannot represent a process invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnRequestError {
    /// Direct process execution requires an explicit argv zero.
    EmptyArgv,
    /// A final child descriptor map named the same target twice.
    DuplicateDescriptor(u32),
    /// A descriptor was named more than once in the final close set.
    DuplicateClosedDescriptor(u32),
    /// A descriptor was both mapped and closed in the final request.
    MappedAndClosedDescriptor(u32),
}

impl fmt::Display for SpawnRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyArgv => formatter.write_str("a spawn request requires argv zero"),
            Self::DuplicateDescriptor(descriptor) => {
                write!(
                    formatter,
                    "child descriptor {descriptor} is mapped more than once"
                )
            }
            Self::DuplicateClosedDescriptor(descriptor) => {
                write!(
                    formatter,
                    "child descriptor {descriptor} is closed more than once"
                )
            }
            Self::MappedAndClosedDescriptor(descriptor) => {
                write!(
                    formatter,
                    "child descriptor {descriptor} is both mapped and closed"
                )
            }
        }
    }
}

impl std::error::Error for SpawnRequestError {}

/// A direct process spawn failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// The platform cannot satisfy the process-spawn capability.
    Platform(PlatformError),
    /// The host rejected or could not complete the spawn operation.
    Operation {
        /// Stable I/O error category from the host adapter.
        kind: io::ErrorKind,
        /// Human-readable host error text.
        message: String,
    },
}

impl fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => error.fmt(formatter),
            Self::Operation { message, .. } => write!(formatter, "process spawn failed: {message}"),
        }
    }
}

impl std::error::Error for SpawnError {}

impl From<PlatformError> for SpawnError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

/// The low-level completion state of one child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStatus {
    /// The process called an exit path with this code.
    Exited(i32),
    /// The process was terminated by this platform signal number.
    Signaled(i32),
}

/// Failure while waiting for an already-spawned child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitError {
    kind: io::ErrorKind,
    message: String,
}

impl WaitError {
    /// Build a wait error from a host I/O failure.
    pub fn new(kind: io::ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Stable I/O error category from the host adapter.
    pub const fn kind(&self) -> io::ErrorKind {
        self.kind
    }
}

impl fmt::Display for WaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "process wait failed: {}", self.message)
    }
}

impl std::error::Error for WaitError {}

/// Failure while terminating a child during execution cleanup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminateError {
    kind: io::ErrorKind,
    message: String,
}

impl TerminateError {
    /// Build a termination error from a host I/O failure.
    pub fn new(kind: io::ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Stable I/O error category from the host adapter.
    pub const fn kind(&self) -> io::ErrorKind {
        self.kind
    }
}

impl fmt::Display for TerminateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "process termination failed: {}", self.message)
    }
}

impl std::error::Error for TerminateError {}

/// One owned child process returned by a platform adapter.
pub trait ChildProcess: Send + fmt::Debug {
    /// Adapter-native process identifier, widened for a portable boundary.
    fn id(&self) -> u64;

    /// Block until the child completes and return its low-level status.
    ///
    /// Calling this more than once returns the same completed status.
    fn wait(&mut self) -> Result<ProcessStatus, WaitError>;

    /// Request immediate termination during failure cleanup.
    fn terminate(&mut self) -> Result<(), TerminateError>;
}

/// Implemented by FlashShell platform adapters.
///
/// Capability methods (spawn, pipes, file actions, …) are added to this trait
/// as the features that need them are built; the foundation is the capability
/// query plus the [`require`](Platform::require) guard every capability method
/// calls before touching the host.
pub trait Platform: Send + Sync {
    /// The capabilities this platform supports.
    fn capabilities(&self) -> Capabilities;

    /// Return `Ok(())` when `capability` is supported, else
    /// [`PlatformError::Unsupported`] naming it.
    fn require(&self, capability: Capability) -> Result<(), PlatformError> {
        if self.capabilities().supports(capability) {
            Ok(())
        } else {
            Err(PlatformError::Unsupported { capability })
        }
    }

    /// Resolve and validate one logical working directory.
    fn resolve_working_directory(
        &self,
        request: WorkingDirectoryRequest<'_>,
    ) -> Result<PathBuf, WorkingDirectoryError> {
        self.require(Capability::WorkingDirectory)?;
        let _ = request;
        Err(WorkingDirectoryError::Operation {
            kind: io::ErrorKind::Unsupported,
            message: "the adapter does not implement working-directory resolution".to_owned(),
        })
    }

    /// Create one anonymous byte pipe with uniquely owned endpoints.
    fn pipe(&self) -> Result<PipeEndpoints, PipeError>;

    /// Open one redirection target as an opaque owned endpoint.
    fn open_file(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError>;

    /// Duplicate one deliberate inherited descriptor into an owned endpoint.
    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError>;

    /// Read one chunk from an owned pipe endpoint, returning zero at EOF.
    fn read_descriptor(
        &self,
        endpoint: &dyn DescriptorEndpoint,
        buffer: &mut [u8],
    ) -> Result<usize, DescriptorReadError> {
        self.require(Capability::Pipes)?;
        let _ = (endpoint, buffer);
        Err(DescriptorReadError::InvalidEndpoint)
    }

    /// Execute `request.executable` directly with its explicit native argv.
    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError>;
}

/// A deterministic in-process [`Platform`] for tests.
///
/// It performs no real host access; its capability set is scripted at
/// construction so runtime tests can drive both the supported and the
/// unsupported branches without a filesystem, process, or clock.
#[derive(Clone, Copy, Debug)]
pub struct FakePlatform {
    capabilities: Capabilities,
}

impl FakePlatform {
    /// A fake platform supporting exactly `capabilities`.
    pub const fn new(capabilities: Capabilities) -> Self {
        Self { capabilities }
    }

    /// A fake platform supporting every capability.
    pub const fn full() -> Self {
        Self::new(Capabilities::full())
    }

    /// A fake platform supporting no capability.
    pub const fn none() -> Self {
        Self::new(Capabilities::empty())
    }
}

impl Platform for FakePlatform {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn resolve_working_directory(
        &self,
        request: WorkingDirectoryRequest<'_>,
    ) -> Result<PathBuf, WorkingDirectoryError> {
        self.require(Capability::WorkingDirectory)?;
        let path = if request.path().is_absolute() {
            request.path().to_owned()
        } else {
            request.cwd().join(request.path())
        };
        Ok(normalize_path(&path))
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        self.require(Capability::Pipes)?;
        Ok(PipeEndpoints::new(
            Box::new(FakeDescriptorEndpoint),
            Box::new(FakeDescriptorEndpoint),
        ))
    }

    fn open_file(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        Ok(Box::new(FakeDescriptorEndpoint))
    }

    fn inherit_descriptor(
        &self,
        _descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        Ok(Box::new(FakeDescriptorEndpoint))
    }

    fn read_descriptor(
        &self,
        _endpoint: &dyn DescriptorEndpoint,
        _buffer: &mut [u8],
    ) -> Result<usize, DescriptorReadError> {
        self.require(Capability::Pipes)?;
        Ok(0)
    }

    fn spawn(&self, _request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        self.require(Capability::ProcessSpawn)?;
        Ok(Box::new(FakeChild))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

/// Opaque endpoint used by [`FakePlatform`] without touching a host resource.
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeDescriptorEndpoint;

impl DescriptorEndpoint for FakeDescriptorEndpoint {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Deterministic host-free child returned by [`FakePlatform`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeChild;

impl ChildProcess for FakeChild {
    fn id(&self) -> u64 {
        0
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        Ok(ProcessStatus::Exited(0))
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        Ok(())
    }
}
