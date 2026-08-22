#![forbid(unsafe_code)]

//! Platform-neutral capability contracts for Flash.
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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
    /// The path of the running shell executable, for re-execution.
    ShellExecutable,
    /// Installing this process's own hang-up disposition.
    HangupDisposition,
}

impl Capability {
    /// Every capability, in declaration order.
    pub const ALL: [Capability; 14] = [
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
        Capability::ShellExecutable,
        Capability::HangupDisposition,
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

    /// The full set minus `capability`.
    ///
    /// A host that provides everything except one capability is the shape worth
    /// naming directly: spelling it as a list of the other eleven would drift
    /// silently the next time a capability is added.
    #[must_use]
    pub const fn full_without(capability: Capability) -> Self {
        Self {
            bits: Self::full().bits & !capability.bit(),
        }
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

/// Native environment lookup used by platform-owned directory policy.
///
/// The caller supplies the process or session environment, while each adapter
/// owns the variable names, validation, and fallback convention it understands.
/// This keeps XDG, macOS, and FlashOS policy out of portable runtime code.
pub trait StandardDirectoryEnvironment {
    /// Return one native environment value without lossy text conversion.
    fn value(&self, name: &OsStr) -> Option<OsString>;
}

/// The native per-user roots selected by one platform adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StandardDirectories {
    home: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    state: PathBuf,
}

impl StandardDirectories {
    /// Build one complete standard-directory selection.
    #[must_use]
    pub fn new(home: PathBuf, config: PathBuf, cache: PathBuf, state: PathBuf) -> Self {
        Self {
            home,
            config,
            cache,
            state,
        }
    }

    /// The selected home directory.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The selected per-user configuration root.
    #[must_use]
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// The selected per-user cache root.
    #[must_use]
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// The selected per-user persistent state root.
    #[must_use]
    pub fn state(&self) -> &Path {
        &self.state
    }
}

/// One uniquely owned descriptor resource returned by a platform adapter.
///
/// Child descriptor maps borrow endpoints opaquely. An anonymous-pipe endpoint
/// may additionally be pulled or pushed by the in-process pipeline bridge; file
/// endpoints used by internal commands remain represented by [`FileIoEndpoint`].
pub trait DescriptorEndpoint: Send + fmt::Debug {
    /// Adapter-private concrete endpoint access for the matching spawn adapter.
    fn as_any(&self) -> &dyn Any;

    /// Read at most `buffer.len()` bytes from an owned pipe reader.
    fn read(&mut self, _buffer: &mut [u8]) -> Result<usize, DescriptorReadError> {
        Err(DescriptorReadError::InvalidEndpoint)
    }

    /// Write bytes to an owned pipe writer.
    fn write(&mut self, _buffer: &[u8]) -> Result<usize, DescriptorWriteError> {
        Err(DescriptorWriteError::InvalidEndpoint)
    }
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

/// Failure while feeding bytes through an owned descriptor endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DescriptorWriteError {
    /// The platform cannot satisfy pipe-backed descriptor writes.
    Platform(PlatformError),
    /// The endpoint was not created as a writable pipe endpoint.
    InvalidEndpoint,
    /// The host write operation failed.
    Operation {
        /// Stable I/O error category from the host adapter.
        kind: io::ErrorKind,
        /// Human-readable host error text.
        message: String,
    },
}

impl fmt::Display for DescriptorWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => error.fmt(formatter),
            Self::InvalidEndpoint => {
                formatter.write_str("descriptor endpoint is not a writable pipe endpoint")
            }
            Self::Operation { message, .. } => {
                write!(formatter, "descriptor write failed: {message}")
            }
        }
    }
}

impl std::error::Error for DescriptorWriteError {}

impl From<PlatformError> for DescriptorWriteError {
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

/// An adapter-native process-group identifier, widened for a portable boundary.
///
/// Zero is rejected because POSIX spells "the calling process" as `0` in
/// `setpgid`; a group identifier that could be zero would make "join this
/// existing group" and "become a new leader" indistinguishable at the seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessGroupId(u64);

impl ProcessGroupId {
    /// Build an identifier, rejecting the reserved zero value.
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// The adapter-native identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ProcessGroupId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Where a spawned child's process group comes from.
///
/// The runtime places every member of one pipeline in a single group so a later
/// slice can signal, stop, and hand the terminal to the job as a unit. Group
/// placement is decided before the first spawn and never changes afterwards.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ProcessGroup {
    /// Stay in the shell's own group, as an unmanaged child.
    #[default]
    Inherit,
    /// Become the leader of a new group named after the child itself.
    New,
    /// Join an existing group established by an earlier pipeline member.
    Join(ProcessGroupId),
}

impl ProcessGroup {
    /// Whether honouring this placement needs [`Capability::ProcessGroups`].
    pub const fn requires_capability(self) -> bool {
        !matches!(self, ProcessGroup::Inherit)
    }
}

/// One signal the shell deliberately sends to a job's process group.
///
/// A closed set rather than a raw number: the shell only ever chooses from
/// signals it must be able to honour on every target, and a platform-independent
/// caller must not name a host signal number it did not receive from an adapter.
/// Inbound observations keep their raw number, because they report what the host
/// already did and must not be narrowed to what the shell happens to model.
///
/// `Stop` and the termination signals become runtime callers when the `kill`
/// built-in lands; the shell resumes and hangs up stopped jobs before then.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobSignal {
    /// Stop the group unconditionally.
    Stop,
    /// Resume a stopped group.
    Continue,
    /// Hang up the group because its session is ending.
    ///
    /// Sent only after the shell has resumed the group, because a stopped
    /// process cannot act on a hang-up it never receives.
    Hangup,
    /// Interrupt the group as if requested from its controlling terminal.
    Interrupt,
    /// Ask the group to terminate cleanly.
    Terminate,
    /// Terminate the group unconditionally.
    Kill,
}

/// Failure while delivering a signal to a process group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalError {
    /// The platform cannot satisfy the signal capability.
    Platform(PlatformError),
    /// The host rejected or could not complete the delivery.
    Operation {
        /// Stable I/O error category from the host adapter.
        kind: io::ErrorKind,
        /// A human-readable description of the host failure.
        message: String,
    },
}

impl From<PlatformError> for SignalError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl fmt::Display for SignalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => error.fmt(formatter),
            Self::Operation { message, .. } => {
                write!(formatter, "signal delivery failed: {message}")
            }
        }
    }
}

impl std::error::Error for SignalError {}

/// An active job-control signal arrangement owned by an interactive shell.
///
/// The dispositions are process-wide, so restoration must also run in the
/// implementor's `Drop`: an early exit or a panic would otherwise leave a
/// process whose interrupts are permanently ignored. `restore` is idempotent, so
/// an explicit call followed by the drop is well defined.
pub trait JobControlSignalGuard: fmt::Debug {
    /// Put the previous dispositions back now, before the guard drops.
    fn restore(&mut self) -> Result<(), PlatformError>;
}

/// A guard that arranged nothing; restoring it always succeeds.
#[derive(Debug)]
pub struct NoopJobControlSignalGuard;

impl JobControlSignalGuard for NoopJobControlSignalGuard {
    fn restore(&mut self) -> Result<(), PlatformError> {
        Ok(())
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
    process_group: ProcessGroup,
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
            process_group: ProcessGroup::Inherit,
        })
    }

    /// Place the child in `process_group` instead of the shell's own group.
    #[must_use]
    pub const fn in_process_group(mut self, process_group: ProcessGroup) -> Self {
        self.process_group = process_group;
        self
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

    /// The group placement the adapter must establish before execution.
    pub const fn process_group(self) -> ProcessGroup {
        self.process_group
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

/// What a blocking observation of one child reported.
///
/// Kept separate from [`ProcessStatus`], which is a completion type: a stopped
/// child has not completed, and folding a non-completion state into that enum
/// would force every site that reasons about a finished process to account for
/// one that is still alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessTransition {
    /// The child stopped under this platform signal number and can be resumed.
    Stopped {
        /// The platform signal number that stopped the child.
        signal: i32,
    },
    /// The child continued after a job-control stop.
    Continued,
    /// The child reached a terminal status, which the platform has consumed.
    Completed(ProcessStatus),
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

    /// The group this child was actually placed in, when it has one.
    ///
    /// Reported by the adapter rather than assumed by the caller: a child
    /// spawned as [`ProcessGroup::New`] leads a group the caller cannot name in
    /// advance, and an adapter without process groups reports `None`.
    fn process_group(&self) -> Option<ProcessGroupId> {
        None
    }

    /// Block until the child completes and return its low-level status.
    ///
    /// Calling this more than once returns the same completed status.
    fn wait(&mut self) -> Result<ProcessStatus, WaitError>;

    /// Block until the child completes or stops, reporting which happened.
    ///
    /// Defaulted to delegate to [`wait`](ChildProcess::wait), which never
    /// reports a stop. An adapter that cannot observe a stop is also one that
    /// cannot deliver one, so the default is correct rather than lossy.
    fn wait_for_transition(&mut self) -> Result<ProcessTransition, WaitError> {
        self.wait().map(ProcessTransition::Completed)
    }

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

/// Opaque portable snapshot of terminal attributes retained with a stopped job.
pub trait TerminalModeToken: Send + fmt::Debug {
    /// Adapter-private concrete token access.
    fn as_any(&self) -> &dyn Any;
}

/// An active handover of the terminal to a foreground job.
///
/// The shell must get the terminal back whatever happens to the job, so
/// restoration also runs in the implementor's `Drop`: a runtime error, an early
/// return, or a panic during the wait would otherwise leave the terminal owned
/// by a process that has exited, where the next read raises `SIGTTIN` instead of
/// reaching the prompt. `restore` is idempotent, so an explicit call followed by
/// the drop is well defined.
pub trait ForegroundTerminalGuard: fmt::Debug {
    /// Return terminal ownership to the shell now, before the guard drops.
    fn restore(&mut self) -> Result<(), PlatformError>;

    /// The group that held the terminal when this handover began.
    fn previous_owner(&self) -> Option<ProcessGroupId>;
}

/// Implemented by Flash platform adapters.
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

    /// Snapshot the terminal attributes currently established by its owner.
    fn snapshot_terminal_mode(&self) -> Result<Box<dyn TerminalModeToken>, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        Ok(Box::new(NoopTerminalModeToken))
    }

    /// Reapply a terminal-attribute token previously returned by this adapter.
    fn apply_terminal_mode(&self, _token: &dyn TerminalModeToken) -> Result<(), PlatformError> {
        self.require(Capability::TerminalInfo)
    }

    /// The process group that currently owns the terminal, if there is one.
    fn foreground_process_group(&self) -> Result<Option<ProcessGroupId>, PlatformError> {
        self.require(Capability::ForegroundTerminal)?;
        Ok(None)
    }

    /// Give the terminal to `group` until the returned guard is dropped.
    ///
    /// The guard, not the caller, remembers which group held the terminal
    /// before: only the adapter can read that owner without racing the handover
    /// it is about to perform.
    fn enter_foreground(
        &self,
        group: ProcessGroupId,
    ) -> Result<Box<dyn ForegroundTerminalGuard>, PlatformError> {
        self.require(Capability::ForegroundTerminal)?;
        let _ = group;
        Ok(Box::new(NoopForegroundTerminalGuard))
    }

    /// Deliver `signal` to every member of `group`.
    ///
    /// Group-directed because the terminal stops or interrupts a whole job at
    /// once; a per-process loop would race source-ordered waiting.
    fn signal_process_group(
        &self,
        group: ProcessGroupId,
        signal: JobSignal,
    ) -> Result<(), SignalError> {
        self.require(Capability::Signals)?;
        let _ = (group, signal);
        Ok(())
    }

    /// Arrange the shell's own job-control signal dispositions.
    ///
    /// Called only by an interactive client. A shell running a script keeps the
    /// default dispositions, because a script host that ignores an interrupt
    /// cannot be stopped from the keyboard.
    fn install_job_control_signals(&self) -> Result<Box<dyn JobControlSignalGuard>, PlatformError> {
        self.require(Capability::Signals)?;
        Ok(Box::new(NoopJobControlSignalGuard))
    }

    /// The path of the running shell executable.
    ///
    /// A background conditional chain re-executes the shell as its own
    /// supervisor, so the shell must be able to name itself. Deliberately not
    /// derived from argv zero, which a caller controls.
    fn shell_executable(&self) -> Result<PathBuf, PlatformError> {
        Err(PlatformError::Unsupported {
            capability: Capability::ShellExecutable,
        })
    }

    /// Ignore hang-up for this process from here on.
    ///
    /// Installed by a chain subshell on itself. A group-directed hang-up
    /// reaches the subshell and its current child together; the subshell must
    /// survive it to reap that child, or the wait returns while a grandchild is
    /// still alive.
    fn ignore_hangup(&self) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported {
            capability: Capability::HangupDisposition,
        })
    }

    /// Select native per-user home, configuration, cache, and state roots.
    fn standard_directories(
        &self,
        environment: &dyn StandardDirectoryEnvironment,
    ) -> Result<StandardDirectories, PlatformError> {
        self.require(Capability::StandardDirectories)?;
        let _ = environment;
        Err(PlatformError::Unavailable {
            capability: Capability::StandardDirectories,
            reason: "the adapter does not implement standard-directory discovery".to_owned(),
        })
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
    child_stops: u32,
}

impl FakePlatform {
    /// A fake platform supporting exactly `capabilities`, reporting no terminal.
    pub const fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            is_terminal: false,
            is_output_terminal: false,
            terminal_size: TerminalSize::new(80, 24),
            child_stops: 0,
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
            child_stops: 0,
        }
    }

    /// A fake platform whose children each report `stops` stops before completing.
    ///
    /// Spelled out as a full struct literal rather than built from
    /// [`FakePlatform::new`] with a functional-update tail: that shorthand is
    /// not allowed in a `const fn`, and this constructor stays `const` so it
    /// composes with the same call sites the other constructors do.
    pub const fn with_stopping_children(capabilities: Capabilities, stops: u32) -> Self {
        Self {
            capabilities,
            is_terminal: false,
            is_output_terminal: false,
            terminal_size: TerminalSize::new(80, 24),
            child_stops: stops,
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

    fn shell_executable(&self) -> Result<PathBuf, PlatformError> {
        self.require(Capability::ShellExecutable)?;
        Ok(PathBuf::from("/fake/fsh"))
    }

    fn ignore_hangup(&self) -> Result<(), PlatformError> {
        self.require(Capability::HangupDisposition)
    }

    fn standard_directories(
        &self,
        _environment: &dyn StandardDirectoryEnvironment,
    ) -> Result<StandardDirectories, PlatformError> {
        self.require(Capability::StandardDirectories)?;
        let home = PathBuf::from("/home/fake");
        Ok(StandardDirectories::new(
            home.clone(),
            home.join(".config"),
            home.join(".cache"),
            home.join(".local/state"),
        ))
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

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        self.require(Capability::ProcessSpawn)?;
        if request.process_group().requires_capability() {
            self.require(Capability::ProcessGroups)?;
        }
        Ok(Box::new(FakeChild::with_stops(
            request.process_group(),
            self.child_stops,
        )))
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
    foreground_handovers: Arc<Mutex<Vec<ProcessGroupId>>>,
    foreground_releases: Arc<AtomicUsize>,
    terminal_mode_snapshots: Arc<AtomicUsize>,
    terminal_mode_applications: Arc<AtomicUsize>,
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

    /// Every group the terminal was handed to, in call order.
    #[must_use]
    pub fn foreground_handovers(&self) -> Vec<ProcessGroupId> {
        self.foreground_handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How often the terminal came back, whether restored explicitly or dropped.
    ///
    /// Counted at the guard rather than at the platform: restoration on drop is
    /// the path that a runtime error or a panic takes, and a tally that missed
    /// it could not tell a returned terminal from a leaked one.
    #[must_use]
    pub fn foreground_releases(&self) -> usize {
        self.foreground_releases.load(Ordering::Relaxed)
    }

    /// How often a foreground job's terminal attributes were captured.
    #[must_use]
    pub fn terminal_mode_snapshots(&self) -> usize {
        self.terminal_mode_snapshots.load(Ordering::Relaxed)
    }

    /// How often retained job terminal attributes were reapplied.
    #[must_use]
    pub fn terminal_mode_applications(&self) -> usize {
        self.terminal_mode_applications.load(Ordering::Relaxed)
    }
}

/// A [`ForegroundTerminalGuard`] that tallies its release exactly once.
#[derive(Debug)]
struct RecordingForegroundGuard {
    inner: Box<dyn ForegroundTerminalGuard>,
    releases: Arc<AtomicUsize>,
    released: bool,
}

impl ForegroundTerminalGuard for RecordingForegroundGuard {
    fn restore(&mut self) -> Result<(), PlatformError> {
        let result = self.inner.restore();
        if !self.released {
            self.released = true;
            self.releases.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn previous_owner(&self) -> Option<ProcessGroupId> {
        self.inner.previous_owner()
    }
}

impl Drop for RecordingForegroundGuard {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.releases.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One spawn a [`RecordingPlatform`] observed, in call order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRecord {
    executable: PathBuf,
    argv: Vec<OsString>,
    requested: ProcessGroup,
    process_group: Option<ProcessGroupId>,
}

impl SpawnRecord {
    /// The executable path passed to the platform.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// The complete native argv passed to the platform.
    #[must_use]
    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    /// The placement the caller asked for.
    #[must_use]
    pub const fn requested(&self) -> ProcessGroup {
        self.requested
    }

    /// The group the resulting child reported, when it has one.
    #[must_use]
    pub const fn process_group(&self) -> Option<ProcessGroupId> {
        self.process_group
    }
}

/// The spawns a [`RecordingPlatform`] received, in order.
///
/// Job control is decided before a child exists and is invisible in its exit
/// status, so a test that only sees statuses cannot tell one process group per
/// pipeline from none at all. The log makes the placement itself observable.
#[derive(Clone, Debug, Default)]
pub struct SpawnCallLog {
    records: Arc<Mutex<Vec<SpawnRecord>>>,
}

impl SpawnCallLog {
    /// Every spawn recorded so far, in call order.
    #[must_use]
    pub fn records(&self) -> Vec<SpawnRecord> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn push(&self, record: SpawnRecord) {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
    }
}

/// One group-directed signal a [`RecordingPlatform`] observed, in call order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalRecord {
    group: ProcessGroupId,
    signal: JobSignal,
}

impl SignalRecord {
    /// The group the signal was addressed to.
    #[must_use]
    pub const fn group(&self) -> ProcessGroupId {
        self.group
    }

    /// The signal the caller chose.
    #[must_use]
    pub const fn signal(&self) -> JobSignal {
        self.signal
    }
}

/// The group-directed signals a [`RecordingPlatform`] received, in order.
///
/// A resume that works and a resume that was never attempted produce the same
/// completed job, so the log is the only way to assert that the executor
/// actually sent one.
#[derive(Clone, Debug, Default)]
pub struct SignalCallLog {
    records: Arc<Mutex<Vec<SignalRecord>>>,
}

impl SignalCallLog {
    /// Every signal recorded so far, in call order.
    #[must_use]
    pub fn records(&self) -> Vec<SignalRecord> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn push(&self, record: SignalRecord) {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
    }
}

/// A [`FakePlatform`] that records the terminal and spawn calls it receives.
///
/// Every method delegates, so the wrapped platform's scripted capability set
/// still decides whether a call succeeds; recording adds only the fact that the
/// call was made.
#[derive(Clone, Debug)]
pub struct RecordingPlatform {
    inner: FakePlatform,
    log: TerminalCallLog,
    spawns: SpawnCallLog,
    signals: SignalCallLog,
}

impl RecordingPlatform {
    /// Record the terminal calls made against `inner`.
    #[must_use]
    pub fn new(inner: FakePlatform) -> Self {
        Self {
            inner,
            log: TerminalCallLog::default(),
            spawns: SpawnCallLog::default(),
            signals: SignalCallLog::default(),
        }
    }

    /// A handle to this platform's log, still readable after the platform moves.
    #[must_use]
    pub fn log(&self) -> TerminalCallLog {
        self.log.clone()
    }

    /// A handle to this platform's spawn log, readable after the platform moves.
    #[must_use]
    pub fn spawn_log(&self) -> SpawnCallLog {
        self.spawns.clone()
    }

    /// A handle to this platform's signal log, readable after the platform moves.
    #[must_use]
    pub fn signal_log(&self) -> SignalCallLog {
        self.signals.clone()
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

    fn snapshot_terminal_mode(&self) -> Result<Box<dyn TerminalModeToken>, PlatformError> {
        let token = self.inner.snapshot_terminal_mode()?;
        self.log
            .terminal_mode_snapshots
            .fetch_add(1, Ordering::Relaxed);
        Ok(token)
    }

    fn apply_terminal_mode(&self, token: &dyn TerminalModeToken) -> Result<(), PlatformError> {
        self.inner.apply_terminal_mode(token)?;
        self.log
            .terminal_mode_applications
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn foreground_process_group(&self) -> Result<Option<ProcessGroupId>, PlatformError> {
        self.inner.foreground_process_group()
    }

    fn enter_foreground(
        &self,
        group: ProcessGroupId,
    ) -> Result<Box<dyn ForegroundTerminalGuard>, PlatformError> {
        let guard = self.inner.enter_foreground(group)?;
        self.log
            .foreground_handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(group);
        Ok(Box::new(RecordingForegroundGuard {
            inner: guard,
            releases: Arc::clone(&self.log.foreground_releases),
            released: false,
        }))
    }

    fn shell_executable(&self) -> Result<PathBuf, PlatformError> {
        self.inner.shell_executable()
    }

    fn ignore_hangup(&self) -> Result<(), PlatformError> {
        self.inner.ignore_hangup()
    }

    fn standard_directories(
        &self,
        environment: &dyn StandardDirectoryEnvironment,
    ) -> Result<StandardDirectories, PlatformError> {
        self.inner.standard_directories(environment)
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
        let child = self.inner.spawn(request)?;
        self.spawns.push(SpawnRecord {
            executable: request.executable().to_owned(),
            argv: request.argv().to_vec(),
            requested: request.process_group(),
            process_group: child.process_group(),
        });
        Ok(child)
    }

    fn signal_process_group(
        &self,
        group: ProcessGroupId,
        signal: JobSignal,
    ) -> Result<(), SignalError> {
        self.signals.push(SignalRecord { group, signal });
        self.inner.signal_process_group(group, signal)
    }

    fn install_job_control_signals(&self) -> Result<Box<dyn JobControlSignalGuard>, PlatformError> {
        self.inner.install_job_control_signals()
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

    fn write(&mut self, buffer: &[u8]) -> Result<usize, DescriptorWriteError> {
        Ok(buffer.len())
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

/// The signal number [`FakeChild`] reports for a scripted stop.
///
/// Exported so a test asserts against this name rather than repeating a bare
/// number that reads like a host signal and is not one.
pub const FAKE_STOP_SIGNAL: i32 = 18;

/// Deterministic host-free child returned by [`FakePlatform`].
///
/// Identities are unique and nonzero because job control is built on them: a
/// fake whose children all answered the same identifier could not distinguish
/// "every pipeline member joined one group" from "no member moved at all".
#[derive(Clone, Copy, Debug)]
pub struct FakeChild {
    id: u64,
    process_group: Option<ProcessGroupId>,
    stops_remaining: u32,
}

impl FakeChild {
    /// A child with a fresh identity and the group `placement` asks for.
    #[must_use]
    pub fn new(placement: ProcessGroup) -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u64;
        let process_group = match placement {
            ProcessGroup::Inherit => None,
            ProcessGroup::New => ProcessGroupId::new(id),
            ProcessGroup::Join(group) => Some(group),
        };
        Self {
            id,
            process_group,
            stops_remaining: 0,
        }
    }

    /// A child that reports `stops` stops before it completes.
    ///
    /// The reported number is a fixed stand-in, not a host stop signal. This
    /// crate has no access to the host's signal numbering, and that numbering
    /// differs between the supported hosts, so a fake that claimed to reproduce
    /// it would be wrong on one of them. Only the adapter that observes a real
    /// stop can report a real number.
    #[must_use]
    pub fn with_stops(placement: ProcessGroup, stops: u32) -> Self {
        Self {
            stops_remaining: stops,
            ..Self::new(placement)
        }
    }
}

impl Default for FakeChild {
    fn default() -> Self {
        Self::new(ProcessGroup::Inherit)
    }
}

impl ChildProcess for FakeChild {
    fn id(&self) -> u64 {
        self.id
    }

    fn process_group(&self) -> Option<ProcessGroupId> {
        self.process_group
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        Ok(ProcessStatus::Exited(0))
    }

    fn wait_for_transition(&mut self) -> Result<ProcessTransition, WaitError> {
        if self.stops_remaining > 0 {
            self.stops_remaining -= 1;
            return Ok(ProcessTransition::Stopped {
                signal: FAKE_STOP_SIGNAL,
            });
        }
        self.wait().map(ProcessTransition::Completed)
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        Ok(())
    }
}

/// A handover that moved no terminal; restoring it always succeeds.
#[derive(Debug)]
pub struct NoopForegroundTerminalGuard;

impl ForegroundTerminalGuard for NoopForegroundTerminalGuard {
    fn restore(&mut self) -> Result<(), PlatformError> {
        Ok(())
    }

    fn previous_owner(&self) -> Option<ProcessGroupId> {
        None
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

/// Opaque terminal-mode token used by capability-only adapters and tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopTerminalModeToken;

impl TerminalModeToken for NoopTerminalModeToken {
    fn as_any(&self) -> &dyn Any {
        self
    }
}
