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
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    /// Terminal size, TTY detection, and canonical/raw input mode.
    TerminalInfo,
    /// A monotonic clock source.
    MonotonicClock,
    /// Home, config, and cache directory discovery.
    StandardDirectories,
    /// Directory entry enumeration.
    ///
    /// Deliberately separate from [`FileActions`](Capability::FileActions),
    /// which covers opening, duplicating, and closing child descriptors —
    /// plumbing, not enumeration. A capability is supported or it is not, with
    /// no partial support, so an adapter that can open a file but cannot list a
    /// directory needs its own bit to say so.
    DirectoryRead,
}

impl Capability {
    /// Every capability, in declaration order.
    pub const ALL: [Capability; 12] = [
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
        Capability::DirectoryRead,
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

/// Failure while opening or transferring through an owned file endpoint.
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
                write!(formatter, "file action failed: {message}")
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

/// One owned file endpoint that can outlive command dispatch.
///
/// Redirection endpoints remain opaque because the spawn adapter installs
/// them into a child descriptor map. `open` and `save` instead need an owned
/// object they can read or write lazily after the platform call returns.
pub trait FileIoEndpoint: Send + fmt::Debug {
    /// Read at most `buffer.len()` bytes, returning zero at EOF.
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, FileActionError>;

    /// Write bytes from `buffer`, returning the number accepted.
    fn write(&mut self, buffer: &[u8]) -> Result<usize, FileActionError>;
}

/// One byte-preserving directory-enumeration request.
#[derive(Clone, Copy, Debug)]
pub struct DirectoryReadRequest<'a> {
    path: &'a Path,
    cwd: &'a Path,
}

impl<'a> DirectoryReadRequest<'a> {
    /// Build an enumeration request relative to the stage working directory.
    pub const fn new(path: &'a Path, cwd: &'a Path) -> Self {
        Self { path, cwd }
    }

    /// The native directory to enumerate.
    pub const fn path(self) -> &'a Path {
        self.path
    }

    /// The working directory used for a relative target.
    pub const fn cwd(self) -> &'a Path {
        self.cwd
    }
}

/// What one directory entry is, without following a symbolic link.
///
/// A link reports itself as a link rather than as its target: resolving it
/// would be a second host access the caller did not ask for, and a dangling
/// link has no target to report at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link, unresolved.
    Symlink,
    /// Anything else the host distinguishes but the shell does not.
    Other,
}

/// One entry of an enumerated directory.
///
/// The name is the bare entry name, not a joined path, and stays an
/// [`OsString`] so native bytes survive without a lossy conversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    name: OsString,
    kind: DirectoryEntryKind,
    size: Option<u64>,
}

impl DirectoryEntry {
    /// Build one entry from its native name, kind, and regular-file byte length.
    ///
    /// `size` is `None` for everything that is not a regular file: a directory
    /// or a link has no byte length the shell could report without inventing
    /// one.
    pub const fn new(name: OsString, kind: DirectoryEntryKind, size: Option<u64>) -> Self {
        Self { name, kind, size }
    }

    /// The bare native entry name.
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// What this entry is, with links left unresolved.
    pub const fn kind(&self) -> DirectoryEntryKind {
        self.kind
    }

    /// The byte length of a regular file, else `None`.
    pub const fn size(&self) -> Option<u64> {
        self.size
    }
}

/// Failure while enumerating a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryReadError {
    /// The platform cannot satisfy directory reads.
    Platform(PlatformError),
    /// The host rejected the enumeration or failed mid-walk.
    Operation {
        /// Stable I/O error category from the host adapter.
        kind: io::ErrorKind,
        /// Human-readable host error text.
        message: String,
    },
}

impl fmt::Display for DirectoryReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => error.fmt(formatter),
            Self::Operation { message, .. } => {
                write!(formatter, "directory read failed: {message}")
            }
        }
    }
}

impl std::error::Error for DirectoryReadError {}

impl From<PlatformError> for DirectoryReadError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

/// An owned, pull-driven walk over one directory's entries.
///
/// Enumeration is lazy rather than a materialized vector so a structured
/// pipeline that stops early — `ls | first 1` — stops the host walk with it,
/// which is what the milestone's no-full-materialization rule requires.
/// Exhaustion is `Ok(None)` and is idempotent.
pub trait DirectoryStream: Send + fmt::Debug {
    /// Advance the walk by one entry; `Ok(None)` is exhaustion.
    fn next_entry(&mut self) -> Result<Option<DirectoryEntry>, DirectoryReadError>;
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

/// The character-cell dimensions of a terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    columns: u16,
    rows: u16,
}

impl TerminalSize {
    #[must_use]
    pub const fn new(columns: u16, rows: u16) -> Self {
        Self { columns, rows }
    }

    #[must_use]
    pub const fn columns(&self) -> u16 {
        self.columns
    }

    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.rows
    }
}

/// An active raw-mode acquisition that restores the saved attributes on drop.
///
/// Restoration must also run in the implementor's `Drop`, so a panic or an
/// early `exit` cannot leave the console unusable. `restore` is idempotent so
/// an explicit call followed by the drop is well defined.
pub trait TerminalModeGuard: fmt::Debug {
    /// Restore the saved terminal attributes now, before the guard drops.
    fn restore(&mut self) -> Result<(), PlatformError>;
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

    /// Whether the standard input of this process is a terminal.
    fn is_terminal(&self) -> bool {
        false
    }

    /// Whether the standard output of this process is a terminal.
    ///
    /// Deliberately separate from [`Platform::is_terminal`]: a session can be
    /// typed at from a keyboard while its output is redirected, and an editor
    /// that redraws with cursor escapes must not write them into the redirect
    /// target.
    fn is_output_terminal(&self) -> bool {
        false
    }

    /// The current terminal dimensions.
    fn terminal_size(&self) -> Result<TerminalSize, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        Ok(TerminalSize::new(80, 24))
    }

    /// Put the terminal into raw mode until the returned guard is dropped.
    fn enter_raw_mode(&self) -> Result<Box<dyn TerminalModeGuard>, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        Ok(Box::new(NoopTerminalModeGuard))
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

    /// Open one owned endpoint for lazy in-process file transfer.
    fn open_file_io(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn FileIoEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        let _ = request;
        Err(FileActionError::Operation {
            kind: io::ErrorKind::Unsupported,
            message: "the adapter does not implement in-process file transfer".to_owned(),
        })
    }

    /// Duplicate one deliberate inherited descriptor into an owned endpoint.
    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError>;

    /// Enumerate one directory as an owned, lazily advanced walk.
    fn read_directory(
        &self,
        request: DirectoryReadRequest<'_>,
    ) -> Result<Box<dyn DirectoryStream>, DirectoryReadError> {
        self.require(Capability::DirectoryRead)?;
        let _ = request;
        Err(DirectoryReadError::Operation {
            kind: io::ErrorKind::Unsupported,
            message: "the adapter does not implement directory enumeration".to_owned(),
        })
    }

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
    is_terminal: bool,
    is_output_terminal: bool,
    terminal_size: TerminalSize,
}

impl FakePlatform {
    /// A fake platform supporting exactly `capabilities`, reporting no terminal.
    pub const fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            is_terminal: false,
            is_output_terminal: false,
            terminal_size: TerminalSize::new(80, 24),
        }
    }

    /// A fake platform supporting every capability.
    pub const fn full() -> Self {
        Self::new(Capabilities::full())
    }

    /// A fake platform supporting no capability.
    pub const fn none() -> Self {
        Self::new(Capabilities::empty())
    }

    /// A fake platform with scripted terminal answers on both ends alike.
    pub const fn with_terminal(
        capabilities: Capabilities,
        is_terminal: bool,
        size: TerminalSize,
    ) -> Self {
        Self::with_terminal_ends(capabilities, is_terminal, is_terminal, size)
    }

    /// A fake platform whose input and output ends answer independently.
    ///
    /// The mixed case is the one worth representing: a session typed at from a
    /// keyboard with its output redirected to a file is exactly what
    /// [`Platform::is_output_terminal`] exists to detect, and a double backed
    /// by a single flag cannot express it.
    pub const fn with_terminal_ends(
        capabilities: Capabilities,
        is_terminal: bool,
        is_output_terminal: bool,
        size: TerminalSize,
    ) -> Self {
        Self {
            capabilities,
            is_terminal,
            is_output_terminal,
            terminal_size: size,
        }
    }
}

impl Platform for FakePlatform {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn is_terminal(&self) -> bool {
        self.is_terminal
    }

    fn is_output_terminal(&self) -> bool {
        self.is_output_terminal
    }

    fn terminal_size(&self) -> Result<TerminalSize, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        Ok(self.terminal_size)
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

    fn open_file_io(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn FileIoEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        Ok(Box::new(FakeFileIoEndpoint))
    }

    fn inherit_descriptor(
        &self,
        _descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        Ok(Box::new(FakeDescriptorEndpoint))
    }

    fn read_directory(
        &self,
        _request: DirectoryReadRequest<'_>,
    ) -> Result<Box<dyn DirectoryStream>, DirectoryReadError> {
        self.require(Capability::DirectoryRead)?;
        Ok(Box::new(EmptyDirectoryStream))
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

/// A shared tally of the terminal calls a [`RecordingPlatform`] received.
///
/// [`FakePlatform`] is `Copy` and carries no interior mutability, so a caller
/// that takes its platform by value leaves the test no way to observe what was
/// asked of it. A log is cloned off the platform before it is handed over and
/// read back afterwards, which makes "raw mode was requested" an assertion
/// rather than an inference from the outcome.
#[derive(Clone, Debug, Default)]
pub struct TerminalCallLog {
    raw_mode_entries: Arc<AtomicUsize>,
    terminal_size_queries: Arc<AtomicUsize>,
}

impl TerminalCallLog {
    /// How often raw mode was requested, whether or not the request succeeded.
    #[must_use]
    pub fn raw_mode_entries(&self) -> usize {
        self.raw_mode_entries.load(Ordering::Relaxed)
    }

    /// How often the terminal size was queried.
    #[must_use]
    pub fn terminal_size_queries(&self) -> usize {
        self.terminal_size_queries.load(Ordering::Relaxed)
    }
}

/// A [`FakePlatform`] that records the terminal calls it receives.
///
/// Every method delegates, so the wrapped platform's scripted capability set
/// still decides whether a call succeeds; recording adds only the fact that the
/// call was made.
#[derive(Clone, Debug)]
pub struct RecordingPlatform {
    inner: FakePlatform,
    log: TerminalCallLog,
}

impl RecordingPlatform {
    /// Record the terminal calls made against `inner`.
    #[must_use]
    pub fn new(inner: FakePlatform) -> Self {
        Self {
            inner,
            log: TerminalCallLog::default(),
        }
    }

    /// A handle to this platform's log, still readable after the platform moves.
    #[must_use]
    pub fn log(&self) -> TerminalCallLog {
        self.log.clone()
    }
}

impl Platform for RecordingPlatform {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn is_terminal(&self) -> bool {
        self.inner.is_terminal()
    }

    fn is_output_terminal(&self) -> bool {
        self.inner.is_output_terminal()
    }

    fn terminal_size(&self) -> Result<TerminalSize, PlatformError> {
        self.log
            .terminal_size_queries
            .fetch_add(1, Ordering::Relaxed);
        self.inner.terminal_size()
    }

    fn enter_raw_mode(&self) -> Result<Box<dyn TerminalModeGuard>, PlatformError> {
        self.log.raw_mode_entries.fetch_add(1, Ordering::Relaxed);
        self.inner.enter_raw_mode()
    }

    fn resolve_working_directory(
        &self,
        request: WorkingDirectoryRequest<'_>,
    ) -> Result<PathBuf, WorkingDirectoryError> {
        self.inner.resolve_working_directory(request)
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        self.inner.pipe()
    }

    fn open_file(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.inner.open_file(request)
    }

    fn open_file_io(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn FileIoEndpoint>, FileActionError> {
        self.inner.open_file_io(request)
    }

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.inner.inherit_descriptor(descriptor)
    }

    fn read_directory(
        &self,
        request: DirectoryReadRequest<'_>,
    ) -> Result<Box<dyn DirectoryStream>, DirectoryReadError> {
        self.inner.read_directory(request)
    }

    fn read_descriptor(
        &self,
        endpoint: &dyn DescriptorEndpoint,
        buffer: &mut [u8],
    ) -> Result<usize, DescriptorReadError> {
        self.inner.read_descriptor(endpoint, buffer)
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        self.inner.spawn(request)
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

/// Empty in-process file endpoint used by [`FakePlatform`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeFileIoEndpoint;

impl FileIoEndpoint for FakeFileIoEndpoint {
    fn read(&mut self, _buffer: &mut [u8]) -> Result<usize, FileActionError> {
        Ok(0)
    }

    fn write(&mut self, buffer: &[u8]) -> Result<usize, FileActionError> {
        Ok(buffer.len())
    }
}

/// Deterministic host-free directory walk returned by [`FakePlatform`].
///
/// It reports an empty directory rather than scripted entries: the fake never
/// touches a host, and a runtime test that needs entries drives the value
/// mapping directly instead of going through a platform.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyDirectoryStream;

impl DirectoryStream for EmptyDirectoryStream {
    fn next_entry(&mut self) -> Result<Option<DirectoryEntry>, DirectoryReadError> {
        Ok(None)
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

/// A guard that owns no terminal state; restoring it always succeeds.
#[derive(Debug)]
pub struct NoopTerminalModeGuard;

impl TerminalModeGuard for NoopTerminalModeGuard {
    fn restore(&mut self) -> Result<(), PlatformError> {
        Ok(())
    }
}
