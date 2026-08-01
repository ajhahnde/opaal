#![deny(unsafe_code)]

//! POSIX platform adapter for FlashShell.
//!
//! macOS and Linux provide every FlashShell platform capability. The concrete
//! descriptor and direct-spawn primitives are implemented here; pipeline,
//! redirection, process-group, and terminal behavior is built out as the
//! features that need it land. The adapter reports the full capability set so
//! the runtime resolves internal-vs-external and plan preflight against a
//! truthful host profile.

use std::any::Any;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use flashshell_platform::{
    Capabilities, Capability, ChildProcess, DescriptorEndpoint, DescriptorReadError,
    DescriptorWriteError, DirectoryEntry, DirectoryEntryKind, DirectoryReadError,
    DirectoryReadRequest, DirectoryStream, FileActionError, FileIoEndpoint, FileOpenMode,
    FileOpenRequest, ForegroundTerminalGuard, JobControlSignalGuard, JobSignal, PipeEndpoints,
    PipeError, Platform, PlatformError, ProcessGroupId, ProcessStatus, ProcessTransition,
    SignalError, SpawnError, SpawnRequest, TerminalModeGuard, TerminalSize, TerminateError,
    WaitError, WorkingDirectoryError, WorkingDirectoryRequest,
};

/// A uniquely owned POSIX descriptor with close-on-exec discipline.
///
/// The wrapper is intentionally not `Clone`: another owner requires the
/// fallible [`try_clone`](OwnedDescriptor::try_clone) operation. Normal release
/// happens only through `Drop` or an explicit transfer back into [`OwnedFd`].
#[derive(Debug)]
pub struct OwnedDescriptor {
    descriptor: File,
}

impl OwnedDescriptor {
    /// Take ownership and atomically duplicate the descriptor with close-on-exec.
    ///
    /// The supplied owner is released whether duplication succeeds or fails.
    pub fn adopt(descriptor: OwnedFd) -> io::Result<Self> {
        let cloexec_descriptor = descriptor.try_clone()?;
        Ok(Self {
            descriptor: File::from(cloexec_descriptor),
        })
    }

    /// Create another close-on-exec owner of the same open file description.
    pub fn try_clone(&self) -> io::Result<Self> {
        self.descriptor
            .try_clone()
            .map(|descriptor| Self { descriptor })
    }

    /// Borrow the descriptor without transferring ownership.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.descriptor.as_fd()
    }

    /// Transfer ownership to a standard-library descriptor owner.
    pub fn into_owned_fd(self) -> OwnedFd {
        self.descriptor.into()
    }
}

impl DescriptorEndpoint for OwnedDescriptor {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, DescriptorReadError> {
        self.descriptor
            .read(buffer)
            .map_err(|error| DescriptorReadError::Operation {
                kind: error.kind(),
                message: error.to_string(),
            })
    }

    fn write(&mut self, buffer: &[u8]) -> Result<usize, DescriptorWriteError> {
        self.descriptor
            .write(buffer)
            .map_err(|error| DescriptorWriteError::Operation {
                kind: error.kind(),
                message: error.to_string(),
            })
    }
}

impl FileIoEndpoint for OwnedDescriptor {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, FileActionError> {
        self.descriptor.read(buffer).map_err(file_action_error)
    }

    fn write(&mut self, buffer: &[u8]) -> Result<usize, FileActionError> {
        self.descriptor.write(buffer).map_err(file_action_error)
    }
}

/// POSIX adapter for process and terminal capabilities.
#[derive(Debug, Default, Clone, Copy)]
pub struct PosixPlatform;

/// Owned POSIX child process handle.
#[derive(Debug)]
pub struct PosixChild {
    child: Child,
    completed: Option<ProcessStatus>,
    process_group: Option<ProcessGroupId>,
}

impl ChildProcess for PosixChild {
    fn id(&self) -> u64 {
        u64::from(self.child.id())
    }

    fn process_group(&self) -> Option<ProcessGroupId> {
        self.process_group
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        if let Some(status) = self.completed {
            return Ok(status);
        }
        match self.observe(0)? {
            ProcessTransition::Completed(status) => Ok(status),
            // Unreachable without WUNTRACED or WCONTINUED: a plain wait never
            // reports a nonterminal transition. If a host ever did, report an
            // adapter failure rather than unwinding or fabricating completion.
            ProcessTransition::Stopped { signal } => Err(WaitError::new(
                io::ErrorKind::Other,
                format!("the host reported a stop by signal {signal} without being asked for one"),
            )),
            ProcessTransition::Continued => Err(WaitError::new(
                io::ErrorKind::Other,
                "the host reported a continuation without being asked for one",
            )),
        }
    }

    fn wait_for_transition(&mut self) -> Result<ProcessTransition, WaitError> {
        if let Some(status) = self.completed {
            return Ok(ProcessTransition::Completed(status));
        }
        self.observe(libc::WUNTRACED | libc::WCONTINUED)
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        // Once the adapter has reaped, std does not know the child is gone, so
        // `Child::kill` would signal a pid the host may have already reused.
        if self.completed.is_some() {
            return Ok(());
        }
        self.child
            .kill()
            .map_err(|error| TerminateError::new(error.kind(), error.to_string()))
    }
}

impl PosixChild {
    /// Reap the child for one transition, caching a completion.
    ///
    /// The adapter reaps for itself rather than through `std::process::Child`,
    /// so the completion is recorded here and every later call short-circuits on
    /// it. Nothing else reaps the pid, so the recorded status is the only one
    /// the child ever produces.
    fn observe(&mut self, flags: libc::c_int) -> Result<ProcessTransition, WaitError> {
        let pid = libc::pid_t::try_from(self.child.id()).map_err(|_| {
            WaitError::new(
                io::ErrorKind::InvalidInput,
                "the child identifier exceeds the POSIX process identifier range",
            )
        })?;
        let transition = child_wait::observe(pid, flags)?;
        if let ProcessTransition::Completed(status) = transition {
            self.completed = Some(status);
        }
        Ok(transition)
    }
}

impl Platform for PosixPlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities::full()
    }

    fn is_terminal(&self) -> bool {
        terminal_mode::is_terminal(io::stdin().as_fd())
    }

    fn is_output_terminal(&self) -> bool {
        terminal_mode::is_terminal(io::stdout().as_fd())
    }

    fn terminal_size(&self) -> Result<TerminalSize, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        match terminal_mode::window_size(io::stdin().as_fd()) {
            // A pty can legitimately report zero before a size is set; the
            // documented fallback keeps the renderer's arithmetic valid.
            Ok((columns, rows)) if columns > 0 && rows > 0 => Ok(TerminalSize::new(columns, rows)),
            Ok(_) | Err(_) => Ok(TerminalSize::new(80, 24)),
        }
    }

    fn enter_raw_mode(&self) -> Result<Box<dyn TerminalModeGuard>, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        let stdin = io::stdin();
        if !terminal_mode::is_terminal(stdin.as_fd()) {
            return Err(PlatformError::Unavailable {
                capability: Capability::TerminalInfo,
                reason: "standard input is not a terminal".to_owned(),
            });
        }
        let saved = terminal_mode::current_attributes(stdin.as_fd()).map_err(|error| {
            PlatformError::Unavailable {
                capability: Capability::TerminalInfo,
                reason: format!("reading terminal attributes failed: {error}"),
            }
        })?;
        let raw = terminal_mode::raw_from(&saved);
        terminal_mode::apply(stdin.as_fd(), &raw).map_err(|error| PlatformError::Unavailable {
            capability: Capability::TerminalInfo,
            reason: format!("entering raw mode failed: {error}"),
        })?;
        Ok(Box::new(PosixTerminalModeGuard {
            saved,
            restored: false,
        }))
    }

    fn foreground_process_group(&self) -> Result<Option<ProcessGroupId>, PlatformError> {
        self.require(Capability::ForegroundTerminal)?;
        let stdin = io::stdin();
        if !terminal_mode::is_terminal(stdin.as_fd()) {
            return Ok(None);
        }
        foreground_terminal::owner(stdin.as_fd())
            .map_err(foreground_unavailable)
            .map(ProcessGroupId::new)
    }

    fn enter_foreground(
        &self,
        group: ProcessGroupId,
    ) -> Result<Box<dyn ForegroundTerminalGuard>, PlatformError> {
        self.require(Capability::ForegroundTerminal)?;
        let stdin = io::stdin();
        if !terminal_mode::is_terminal(stdin.as_fd()) {
            return Err(PlatformError::Unavailable {
                capability: Capability::ForegroundTerminal,
                reason: "standard input is not a terminal".to_owned(),
            });
        }
        let previous = foreground_terminal::owner(stdin.as_fd())
            .map_err(foreground_unavailable)
            .map(ProcessGroupId::new)?;
        foreground_terminal::hand_over(stdin.as_fd(), group).map_err(foreground_unavailable)?;
        Ok(Box::new(PosixForegroundTerminalGuard {
            previous,
            restored: false,
        }))
    }

    fn install_job_control_signals(&self) -> Result<Box<dyn JobControlSignalGuard>, PlatformError> {
        self.require(Capability::Signals)?;
        job_control_signals::install()
            .map(|guard| Box::new(guard) as Box<dyn JobControlSignalGuard>)
    }

    fn shell_executable(&self) -> Result<std::path::PathBuf, PlatformError> {
        self.require(Capability::ShellExecutable)?;
        std::env::current_exe().map_err(|error| PlatformError::Unavailable {
            capability: Capability::ShellExecutable,
            reason: format!("determining the running executable failed: {error}"),
        })
    }

    fn ignore_hangup(&self) -> Result<(), PlatformError> {
        self.require(Capability::HangupDisposition)?;
        hangup_disposition::ignore()
    }

    fn signal_process_group(
        &self,
        group: ProcessGroupId,
        signal: JobSignal,
    ) -> Result<(), SignalError> {
        self.require(Capability::Signals)?;
        process_signals::deliver(group, signal)
    }

    fn resolve_working_directory(
        &self,
        request: WorkingDirectoryRequest<'_>,
    ) -> Result<std::path::PathBuf, WorkingDirectoryError> {
        self.require(Capability::WorkingDirectory)?;
        let candidate = if request.path().is_absolute() {
            request.path().to_owned()
        } else {
            request.cwd().join(request.path())
        };
        let resolved = std::fs::canonicalize(candidate).map_err(working_directory_error)?;
        let metadata = std::fs::metadata(&resolved).map_err(working_directory_error)?;
        if !metadata.is_dir() {
            return Err(WorkingDirectoryError::Operation {
                kind: io::ErrorKind::NotADirectory,
                message: format!("{} is not a directory", resolved.display()),
            });
        }
        Ok(resolved)
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        self.require(Capability::Pipes)?;
        let (reader, writer) = io::pipe().map_err(pipe_error)?;
        let reader = OwnedDescriptor::adopt(OwnedFd::from(reader)).map_err(pipe_error)?;
        let writer = OwnedDescriptor::adopt(OwnedFd::from(writer)).map_err(pipe_error)?;
        Ok(PipeEndpoints::new(Box::new(reader), Box::new(writer)))
    }

    fn open_file(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        open_owned_file(self, request).map(|endpoint| Box::new(endpoint) as _)
    }

    fn open_file_io(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn FileIoEndpoint>, FileActionError> {
        open_owned_file(self, request).map(|endpoint| Box::new(endpoint) as _)
    }

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        let descriptor = match descriptor {
            0 => io::stdin().as_fd().try_clone_to_owned(),
            1 => io::stdout().as_fd().try_clone_to_owned(),
            2 => io::stderr().as_fd().try_clone_to_owned(),
            _ => {
                return Err(FileActionError::Operation {
                    kind: io::ErrorKind::Unsupported,
                    message: format!(
                        "inherited child descriptor {descriptor} is not part of the session map"
                    ),
                });
            }
        }
        .map_err(file_action_error)?;
        Ok(Box::new(OwnedDescriptor {
            descriptor: File::from(descriptor),
        }))
    }

    fn read_directory(
        &self,
        request: DirectoryReadRequest<'_>,
    ) -> Result<Box<dyn DirectoryStream>, DirectoryReadError> {
        self.require(Capability::DirectoryRead)?;
        let cwd = std::path::absolute(request.cwd()).map_err(directory_read_error)?;
        let path = if request.path().is_relative() {
            cwd.join(request.path())
        } else {
            request.path().to_owned()
        };
        // `read_dir` reports a missing or non-directory target here, so an
        // unreadable target fails before any entry is handed out.
        let entries = fs::read_dir(path).map_err(directory_read_error)?;
        Ok(Box::new(PosixDirectoryStream { entries }))
    }

    fn read_descriptor(
        &self,
        endpoint: &dyn DescriptorEndpoint,
        buffer: &mut [u8],
    ) -> Result<usize, DescriptorReadError> {
        self.require(Capability::Pipes)?;
        let endpoint = endpoint
            .as_any()
            .downcast_ref::<OwnedDescriptor>()
            .ok_or(DescriptorReadError::InvalidEndpoint)?;
        let mut descriptor = &endpoint.descriptor;
        descriptor
            .read(buffer)
            .map_err(|error| DescriptorReadError::Operation {
                kind: error.kind(),
                message: error.to_string(),
            })
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        self.require(Capability::ProcessSpawn)?;
        if request.process_group().requires_capability() {
            self.require(Capability::ProcessGroups)?;
        }

        let cwd = std::path::absolute(request.cwd()).map_err(spawn_error)?;
        let executable = if request.executable().is_relative() {
            cwd.join(request.executable())
        } else {
            request.executable().to_owned()
        };
        let mut command = Command::new(executable);
        command
            .arg0(&request.argv()[0])
            .args(&request.argv()[1..])
            .env_clear()
            .envs(
                request
                    .environment()
                    .iter()
                    .map(|(name, value)| (name, value)),
            )
            .current_dir(cwd);

        let action_targets = request
            .descriptors()
            .iter()
            .map(|mapping| descriptor_number(mapping.target()))
            .chain(
                request
                    .closed_descriptors()
                    .iter()
                    .copied()
                    .map(descriptor_number),
            )
            .collect::<Result<BTreeSet<_>, _>>()?;
        let mut extra_mappings = Vec::new();
        let mut reservations = Vec::new();

        for mapping in request.descriptors() {
            let endpoint = mapping
                .endpoint()
                .as_any()
                .downcast_ref::<OwnedDescriptor>()
                .ok_or_else(|| SpawnError::Operation {
                    kind: io::ErrorKind::InvalidInput,
                    message: "descriptor endpoint belongs to another platform adapter".to_owned(),
                })?;
            let target = descriptor_number(mapping.target())?;
            match mapping.target() {
                0 => {
                    command.stdin(Stdio::from(
                        endpoint.try_clone().map_err(spawn_error)?.into_owned_fd(),
                    ));
                }
                1 => {
                    command.stdout(Stdio::from(
                        endpoint.try_clone().map_err(spawn_error)?.into_owned_fd(),
                    ));
                }
                2 => {
                    command.stderr(Stdio::from(
                        endpoint.try_clone().map_err(spawn_error)?.into_owned_fd(),
                    ));
                }
                _ => {
                    let descriptor =
                        clone_avoiding_targets(endpoint, &action_targets, &mut reservations)?;
                    extra_mappings.push((descriptor, target));
                }
            }
        }

        let closes = request
            .closed_descriptors()
            .iter()
            .copied()
            .map(descriptor_number)
            .collect::<Result<Vec<_>, _>>()?;
        // The disposition reset is installed first so a child that fails any
        // later hook is already killable by a default-disposition interrupt.
        child_signal_dispositions::configure(&mut command);
        // Group placement is installed before the descriptor hook so a child
        // that fails its descriptor setup is already signallable as a member of
        // its job's group rather than of the shell's own group.
        child_process_group::configure(&mut command, request.process_group())?;
        child_descriptors::configure(&mut command, extra_mappings, closes);

        command
            .spawn()
            .map(|child| {
                let process_group = child_process_group::adopt(&child, request.process_group());
                Box::new(PosixChild {
                    child,
                    completed: None,
                    process_group,
                }) as Box<dyn ChildProcess>
            })
            .map_err(spawn_error)
    }
}

fn clone_avoiding_targets(
    endpoint: &OwnedDescriptor,
    targets: &BTreeSet<i32>,
    reservations: &mut Vec<OwnedFd>,
) -> Result<OwnedFd, SpawnError> {
    loop {
        let descriptor = endpoint.try_clone().map_err(spawn_error)?.into_owned_fd();
        if targets.contains(&descriptor.as_raw_fd()) {
            reservations.push(descriptor);
        } else {
            return Ok(descriptor);
        }
    }
}

fn descriptor_number(descriptor: u32) -> Result<i32, SpawnError> {
    i32::try_from(descriptor).map_err(|_| SpawnError::Operation {
        kind: io::ErrorKind::InvalidInput,
        message: format!("child descriptor {descriptor} exceeds the POSIX descriptor range"),
    })
}

/// A lazily advanced walk over one host directory.
///
/// It holds the host iterator rather than a materialized vector, so a caller
/// that stops early stops the walk with it.
#[derive(Debug)]
struct PosixDirectoryStream {
    entries: fs::ReadDir,
}

impl DirectoryStream for PosixDirectoryStream {
    fn next_entry(&mut self) -> Result<Option<DirectoryEntry>, DirectoryReadError> {
        let Some(entry) = self.entries.next() else {
            return Ok(None);
        };
        let entry = entry.map_err(directory_read_error)?;
        // `symlink_metadata` does not follow the link, so a link reports itself
        // and a dangling link is still enumerable.
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(directory_read_error)?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            DirectoryEntryKind::Symlink
        } else if file_type.is_dir() {
            DirectoryEntryKind::Directory
        } else if file_type.is_file() {
            DirectoryEntryKind::File
        } else {
            DirectoryEntryKind::Other
        };
        let size = matches!(kind, DirectoryEntryKind::File).then(|| metadata.len());
        Ok(Some(DirectoryEntry::new(entry.file_name(), kind, size)))
    }
}

fn directory_read_error(error: io::Error) -> DirectoryReadError {
    DirectoryReadError::Operation {
        kind: error.kind(),
        message: error.to_string(),
    }
}

fn pipe_error(error: io::Error) -> PipeError {
    PipeError::Operation {
        kind: error.kind(),
        message: error.to_string(),
    }
}

fn spawn_error(error: io::Error) -> SpawnError {
    SpawnError::Operation {
        kind: error.kind(),
        message: error.to_string(),
    }
}

fn file_action_error(error: io::Error) -> FileActionError {
    FileActionError::Operation {
        kind: error.kind(),
        message: error.to_string(),
    }
}

fn open_owned_file(
    platform: &PosixPlatform,
    request: FileOpenRequest<'_>,
) -> Result<OwnedDescriptor, FileActionError> {
    platform.require(Capability::FileActions)?;
    let cwd = std::path::absolute(request.cwd()).map_err(file_action_error)?;
    let path = if request.path().is_relative() {
        cwd.join(request.path())
    } else {
        request.path().to_owned()
    };
    let mut options = OpenOptions::new();
    match request.mode() {
        FileOpenMode::Read => {
            options.read(true);
        }
        FileOpenMode::WriteTruncate => {
            options.write(true).create(true).truncate(true);
        }
        FileOpenMode::WriteAppend => {
            options.write(true).create(true).append(true);
        }
    }
    let descriptor = options
        .open(path)
        .map(OwnedFd::from)
        .map_err(file_action_error)?;
    OwnedDescriptor::adopt(descriptor).map_err(file_action_error)
}

fn working_directory_error(error: io::Error) -> WorkingDirectoryError {
    WorkingDirectoryError::Operation {
        kind: error.kind(),
        message: error.to_string(),
    }
}

/// Default signal dispositions and an empty mask for a child that has not
/// executed yet.
///
/// Dispositions and the signal mask are both inherited across `fork`. An
/// interactive shell ignores the job-control signals for its own survival, so
/// without this reset every job it starts would inherit that ignore and stop
/// responding to the keyboard. The mask is cleared for the same reason: `fork`
/// copies the calling thread's mask, and an executed program is entitled to a
/// clear one.
#[allow(unsafe_code)]
mod child_signal_dispositions {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// Signal dispositions a spawned program must not inherit from its shell.
    ///
    /// The interactive shell arranges the five job-control signals for its own
    /// survival. A reserved background-chain supervisor additionally ignores
    /// hang-up so it can wait for its descendants after the parent signals the
    /// group; ordinary external descendants must restore the default instead.
    const RESET: [libc::c_int; 6] = [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTSTP,
        libc::SIGTTOU,
        libc::SIGTTIN,
    ];

    /// Install the pre-exec hook that restores the child's default handling.
    ///
    /// Registered before the process-group and descriptor hooks so a child that
    /// fails a later hook is already killable by a default-disposition
    /// interrupt.
    pub(super) fn configure(command: &mut Command) {
        // SAFETY: this runs after `fork` and calls only sigaction, sigemptyset,
        // and sigprocmask, each of which POSIX lists among the functions usable
        // after `fork` in a multithreaded process. The closure captures nothing,
        // `zeroed` is a valid starting pattern for both `sigaction` and
        // `sigset_t`, and every pointer targets a live, correctly typed local.
        unsafe {
            command.pre_exec(|| {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_DFL;
                if libc::sigemptyset(&raw mut action.sa_mask) == -1 {
                    return Err(io::Error::last_os_error());
                }
                for signal in RESET {
                    if libc::sigaction(signal, &raw const action, std::ptr::null_mut()) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                let mut empty: libc::sigset_t = std::mem::zeroed();
                if libc::sigemptyset(&raw mut empty) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::sigprocmask(libc::SIG_SETMASK, &raw const empty, std::ptr::null_mut())
                    == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

/// Process-group placement for a child that has not executed yet.
///
/// `setpgid` is applied twice on purpose: once in the child before `exec` and
/// once in the parent after `fork`. Either call alone leaves a window in which
/// the other side observes the wrong group — the parent may signal the job
/// before the child has moved itself, and the child may `exec` before the
/// parent's call lands. Both calls are idempotent, so the redundant one is a
/// no-op rather than a correction.
#[allow(unsafe_code)]
mod child_process_group {
    use std::io;
    use std::process::Child;

    use flashshell_platform::{ProcessGroup, ProcessGroupId, SpawnError};

    use super::spawn_error;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// Install the pre-exec hook that moves the child into its group.
    pub(super) fn configure(
        command: &mut Command,
        placement: ProcessGroup,
    ) -> Result<(), SpawnError> {
        let target = match placement {
            // The child is already in the shell's group; leave it there rather
            // than calling setpgid with the shell's own identifier.
            ProcessGroup::Inherit => return Ok(()),
            // POSIX spells "make the caller its own leader" as pgid zero.
            ProcessGroup::New => 0,
            ProcessGroup::Join(group) => group_number(group)?,
        };

        // SAFETY: the hook captures one integer and calls only setpgid, which
        // POSIX lists as async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, target) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    /// Close the parent-side race and report the group the child now belongs to.
    pub(super) fn adopt(child: &Child, placement: ProcessGroup) -> Option<ProcessGroupId> {
        let group = match placement {
            ProcessGroup::Inherit => return None,
            // A new leader names its group after itself, so the identifier is
            // knowable only once the child exists.
            ProcessGroup::New => ProcessGroupId::new(u64::from(child.id()))?,
            ProcessGroup::Join(group) => group,
        };
        let target = group_number(group).ok()?;
        let pid = i32::try_from(child.id()).ok()?;

        // SAFETY: setpgid takes two scalars and dereferences nothing.
        //
        // A failure here is not an error: EACCES means the child already
        // executed, and ESRCH means it already exited. In both cases the
        // child's own pre-exec call decided the group, so the parent's
        // duplicate has nothing left to do.
        unsafe {
            libc::setpgid(pid, target);
        }
        Some(group)
    }

    fn group_number(group: ProcessGroupId) -> Result<libc::pid_t, SpawnError> {
        libc::pid_t::try_from(group.get()).map_err(|_| {
            spawn_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("process group {group} exceeds the POSIX process identifier range"),
            ))
        })
    }
}

#[allow(unsafe_code)]
mod child_descriptors {
    use std::ffi::c_int;
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    unsafe extern "C" {
        fn dup2(source: c_int, target: c_int) -> c_int;
        fn close(descriptor: c_int) -> c_int;
    }

    pub(super) fn configure(
        command: &mut Command,
        mappings: Vec<(OwnedFd, c_int)>,
        closes: Vec<c_int>,
    ) {
        if mappings.is_empty() && closes.is_empty() {
            return;
        }

        // SAFETY: the hook captures only preallocated descriptor owners and
        // integers, then calls the async-signal-safe POSIX dup2/close functions.
        unsafe {
            command.pre_exec(move || {
                for (source, target) in &mappings {
                    if dup2(source.as_raw_fd(), *target) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                for descriptor in &closes {
                    if close(*descriptor) == -1 {
                        let error = io::Error::last_os_error();
                        // EBADF means the requested descriptor was already
                        // absent, which is the specified close no-op.
                        if error.raw_os_error() != Some(9) {
                            return Err(error);
                        }
                    }
                }
                Ok(())
            });
        }
    }
}

/// Foreground terminal ownership.
///
/// `tcsetpgrp` raises `SIGTTOU` at the caller when the calling process is not
/// in the terminal's current foreground group, whose default action stops the
/// shell. That is exactly the situation restoration runs in: the job owns the
/// terminal, and the shell taking it back is a background write. The signal is
/// therefore blocked for the calling thread across the call and the previous
/// mask is restored afterwards.
///
/// The mask is per-thread (`pthread_sigmask`), not a process-wide disposition
/// change: an installed `SIGTTOU` handler stays installed, other threads keep
/// their own masks, and a concurrent handover cannot observe a half-changed
/// global state.
#[allow(unsafe_code)]
mod foreground_terminal {
    use std::io;
    use std::os::fd::{AsRawFd, BorrowedFd};

    use flashshell_platform::ProcessGroupId;

    pub(super) fn owner(fd: BorrowedFd<'_>) -> io::Result<u64> {
        // SAFETY: tcgetpgrp only reads the terminal state behind a valid
        // descriptor and dereferences no caller memory.
        let group = unsafe { libc::tcgetpgrp(fd.as_raw_fd()) };
        if group == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(u64::try_from(group).unwrap_or_default())
    }

    pub(super) fn hand_over(fd: BorrowedFd<'_>, group: ProcessGroupId) -> io::Result<()> {
        let group = libc::pid_t::try_from(group.get()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "process group exceeds the POSIX process identifier range",
            )
        })?;

        let blocked = block_terminal_stops()?;
        // SAFETY: tcsetpgrp takes two scalars and dereferences no memory.
        let result = unsafe { libc::tcsetpgrp(fd.as_raw_fd(), group) };
        let outcome = if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        // The mask is restored even when the handover failed, so a failed
        // transfer cannot leave the shell deaf to terminal stop signals.
        restore_mask(&blocked)?;
        outcome
    }

    /// Block the terminal stop signals for this thread, returning the old mask.
    fn block_terminal_stops() -> io::Result<libc::sigset_t> {
        // SAFETY: `zeroed` is a valid starting bit pattern for `sigset_t`, and
        // sigemptyset overwrites it before any read.
        let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `previous` is written by pthread_sigmask before it is read.
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };

        // SAFETY: every pointer below targets a live, correctly typed local.
        unsafe {
            if libc::sigemptyset(&raw mut blocked) == -1
                || libc::sigaddset(&raw mut blocked, libc::SIGTTOU) == -1
                || libc::sigaddset(&raw mut blocked, libc::SIGTTIN) == -1
                || libc::sigaddset(&raw mut blocked, libc::SIGTSTP) == -1
            {
                return Err(io::Error::last_os_error());
            }
            let result =
                libc::pthread_sigmask(libc::SIG_BLOCK, &raw const blocked, &raw mut previous);
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
        }
        Ok(previous)
    }

    fn restore_mask(previous: &libc::sigset_t) -> io::Result<()> {
        // SAFETY: the pointer targets a live mask that pthread_sigmask reads.
        let result =
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, previous, std::ptr::null_mut()) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(())
    }
}

/// The asynchronous-chain subshell's own hang-up disposition.
///
/// This is separate from the interactive shell's job-control arrangement:
/// only the re-executed child ignores hang-up, so it can survive long enough to
/// reap a grandchild that receives the same group-directed signal.
#[allow(unsafe_code)]
mod hangup_disposition {
    use std::io;

    use flashshell_platform::{Capability, PlatformError};

    pub(super) fn ignore() -> Result<(), PlatformError> {
        // SAFETY: an all-zero pattern is valid for `sigaction`, whose fields are
        // integers and a function pointer; sigemptyset initializes the live
        // mask field before sigaction reads the complete disposition.
        let mut ignore: libc::sigaction = unsafe { std::mem::zeroed() };
        ignore.sa_sigaction = libc::SIG_IGN;
        // SAFETY: the pointer targets the live mask field of `ignore`.
        if unsafe { libc::sigemptyset(&raw mut ignore.sa_mask) } == -1 {
            return Err(unavailable(io::Error::last_os_error()));
        }
        // SAFETY: the input pointer targets the fully initialized disposition
        // above, and the null output pointer asks for no previous disposition.
        if unsafe { libc::sigaction(libc::SIGHUP, &raw const ignore, std::ptr::null_mut()) } == -1 {
            return Err(unavailable(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn unavailable(error: io::Error) -> PlatformError {
        PlatformError::Unavailable {
            capability: Capability::HangupDisposition,
            reason: error.to_string(),
        }
    }
}

/// The interactive shell's own job-control signal dispositions.
///
/// The shell ignores the five signals a terminal can generate for it. Without
/// that, an interrupt aimed at a job the shell still owns the terminal for, or
/// at a shell whose platform cannot hand the terminal over, kills the shell.
/// Ignoring `SIGTTOU` and `SIGTTIN` does not replace the per-thread mask this
/// adapter takes around `tcsetpgrp`: the two mechanisms cover the same hazard
/// from different sides, and the mask still holds if a handler ever replaces
/// the ignore. One consequence is deliberate — a background read now fails with
/// `EIO` instead of stopping the shell.
///
/// The previous disposition is captured as a whole `sigaction`, not just a
/// handler pointer, so restoring reinstates whatever flags and mask were in
/// force before, and matches the reset the child hook installs on the other
/// side of `fork`.
#[allow(unsafe_code)]
mod job_control_signals {
    use std::io;

    use flashshell_platform::{Capability, JobControlSignalGuard, PlatformError};

    /// The signals an interactive shell arranges for the life of the session.
    const ARRANGED: [libc::c_int; 5] = [
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTSTP,
        libc::SIGTTOU,
        libc::SIGTTIN,
    ];

    /// The previous dispositions, restored explicitly or on drop.
    pub(super) struct PosixJobControlSignalGuard {
        previous: [libc::sigaction; 5],
        restored: bool,
    }

    // `libc::sigaction` carries no `Debug` without the `extra_traits` feature,
    // and its contents are opaque host state not worth formatting; the guard's
    // observable field is whether it has already been restored.
    impl std::fmt::Debug for PosixJobControlSignalGuard {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("PosixJobControlSignalGuard")
                .field("restored", &self.restored)
                .finish_non_exhaustive()
        }
    }

    /// Ignore the job-control signals until the returned guard is released.
    pub(super) fn install() -> Result<PosixJobControlSignalGuard, PlatformError> {
        // SAFETY: an all-zero pattern is valid for `sigaction`, whose fields are
        // integers and a function pointer; the mask is initialized by
        // sigemptyset before any read.
        let mut ignore: libc::sigaction = unsafe { std::mem::zeroed() };
        ignore.sa_sigaction = libc::SIG_IGN;
        // SAFETY: the pointer targets the live mask field of `ignore`.
        if unsafe { libc::sigemptyset(&raw mut ignore.sa_mask) } == -1 {
            return Err(unavailable(io::Error::last_os_error()));
        }

        // SAFETY: an all-zero pattern is valid for every element of the array;
        // each slot is overwritten by sigaction below before it is ever read.
        let mut previous: [libc::sigaction; 5] = unsafe { std::mem::zeroed() };
        for index in 0..ARRANGED.len() {
            // SAFETY: the index is bounded by the array it iterates, and both
            // pointers target live, correctly typed locals; sigaction installs
            // `ignore` for the arranged signal and writes the prior disposition
            // into that signal's slot.
            let installed = unsafe {
                libc::sigaction(ARRANGED[index], &raw const ignore, &raw mut previous[index])
            };
            if installed == -1 {
                let failure = io::Error::last_os_error();
                undo(&previous, index);
                return Err(unavailable(failure));
            }
        }
        Ok(PosixJobControlSignalGuard {
            previous,
            restored: false,
        })
    }

    impl PosixJobControlSignalGuard {
        fn restore_now(&mut self) -> Result<(), PlatformError> {
            if self.restored {
                return Ok(());
            }
            // Marked before the loop rather than after it, so a failure partway
            // is reported once instead of being reattempted by the drop against
            // a host that has already refused it.
            self.restored = true;
            for (previous, signal) in self.previous.iter().zip(ARRANGED) {
                // SAFETY: `previous` is a disposition this process produced in
                // `install`; sigaction reinstates it for `signal` and writes no
                // other memory.
                if unsafe { libc::sigaction(signal, &raw const *previous, std::ptr::null_mut()) }
                    == -1
                {
                    return Err(unavailable(io::Error::last_os_error()));
                }
            }
            Ok(())
        }
    }

    impl JobControlSignalGuard for PosixJobControlSignalGuard {
        fn restore(&mut self) -> Result<(), PlatformError> {
            self.restore_now()
        }
    }

    impl Drop for PosixJobControlSignalGuard {
        fn drop(&mut self) {
            // A process that exits with its interrupts still ignored cannot be
            // stopped from the keyboard, so the drop is the backstop for every
            // path that skipped the explicit restore.
            let _ = self.restore_now();
        }
    }

    /// Reinstate the first `installed` dispositions after a failed install.
    ///
    /// An install that gave up partway would otherwise leave the signals it had
    /// already changed ignored, with no guard in existence to change them back —
    /// the one unrecoverable state this guard is built to prevent. Failures here
    /// are discarded because the caller is already returning the error that
    /// matters and has nothing better to attempt.
    fn undo(previous: &[libc::sigaction; 5], installed: usize) {
        for index in 0..installed {
            // SAFETY: the disposition being reinstated is one this process
            // captured moments earlier in `install`, the pointer targets a live
            // element of the caller's array, and a null out-pointer asks for no
            // previous value to be written back.
            unsafe {
                libc::sigaction(
                    ARRANGED[index],
                    &raw const previous[index],
                    std::ptr::null_mut(),
                );
            }
        }
    }

    fn unavailable(error: io::Error) -> PlatformError {
        PlatformError::Unavailable {
            capability: Capability::Signals,
            reason: error.to_string(),
        }
    }
}

/// Group-directed signal delivery.
///
/// POSIX spells a group target as a negated identifier, which is why the
/// adapter and not the runtime performs the negation: a platform-independent
/// caller must never encode a host signalling convention.
#[allow(unsafe_code)]
mod process_signals {
    use std::io;

    use flashshell_platform::{JobSignal, ProcessGroupId, SignalError};

    pub(super) fn deliver(group: ProcessGroupId, signal: JobSignal) -> Result<(), SignalError> {
        let number = match signal {
            JobSignal::Stop => libc::SIGSTOP,
            JobSignal::Continue => libc::SIGCONT,
            JobSignal::Hangup => libc::SIGHUP,
            JobSignal::Interrupt => libc::SIGINT,
            JobSignal::Terminate => libc::SIGTERM,
            JobSignal::Kill => libc::SIGKILL,
        };
        let target = libc::pid_t::try_from(group.get()).map_err(|_| SignalError::Operation {
            kind: io::ErrorKind::InvalidInput,
            message: format!("process group {group} exceeds the POSIX process identifier range"),
        })?;
        // Group 1 negates to the target POSIX reserves for "every process the
        // caller may signal". Delivering a stop there would freeze the whole
        // session, so the one identifier whose negation changes meaning is
        // refused rather than sent.
        if target == 1 {
            return Err(SignalError::Operation {
                kind: io::ErrorKind::InvalidInput,
                message: "process group 1 is not addressable as a group".to_owned(),
            });
        }
        let target = target.checked_neg().ok_or_else(|| SignalError::Operation {
            kind: io::ErrorKind::InvalidInput,
            message: format!("process group {group} has no negated group target"),
        })?;

        // SAFETY: kill takes two scalars and dereferences nothing.
        if unsafe { libc::kill(target, number) } == -1 {
            let error = io::Error::last_os_error();
            return Err(SignalError::Operation {
                kind: error.kind(),
                message: error.to_string(),
            });
        }
        Ok(())
    }
}

/// Blocking child observation.
///
/// `std::process::Child::wait` is `waitpid` without `WUNTRACED`, so a stopped
/// child reports nothing and the caller blocks until it dies. The adapter
/// therefore reaps for itself. `Child` is kept only to own the pid and its
/// standard streams; std installs no `Drop` that reaps, so nothing the adapter
/// has already reaped is ever waited a second time.
///
/// One thing std's own wait does that this does not is close the child's input
/// handle before blocking, which is what stops a parent still holding that
/// handle from waiting on a child that is waiting on the parent. Nothing here
/// holds one: every spawned stream is handed over as an owned descriptor rather
/// than kept as a pipe, so the handles are already absent. A change that starts
/// retaining a child's input handle has to close it here before waiting.
#[allow(unsafe_code)]
mod child_wait {
    use std::ffi::c_int;
    use std::io;

    use flashshell_platform::{ProcessStatus, ProcessTransition, WaitError};

    pub(super) fn observe(pid: c_int, flags: c_int) -> Result<ProcessTransition, WaitError> {
        let mut status: c_int = 0;
        loop {
            // SAFETY: waitpid writes only through the status pointer, which
            // points at a live local for the whole call.
            let result = unsafe { libc::waitpid(pid, &raw mut status, flags) };
            if result != -1 {
                break;
            }
            let error = io::Error::last_os_error();
            // A signal delivered to the shell while it waits is not a wait
            // failure; the observation is simply not finished yet.
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(WaitError::new(error.kind(), error.to_string()));
        }
        decode(status)
    }

    /// Classify one raw wait status.
    ///
    /// Exit is tested rather than inferred from the other two failing. Only
    /// Continued is checked explicitly before the terminal classifications so
    /// no host representation can reach an exit-code macro and produce a
    /// plausible-looking wrong number.
    fn decode(status: c_int) -> Result<ProcessTransition, WaitError> {
        if libc::WIFCONTINUED(status) {
            return Ok(ProcessTransition::Continued);
        }
        if libc::WIFSTOPPED(status) {
            return Ok(ProcessTransition::Stopped {
                signal: libc::WSTOPSIG(status),
            });
        }
        if libc::WIFSIGNALED(status) {
            return Ok(ProcessTransition::Completed(ProcessStatus::Signaled(
                libc::WTERMSIG(status),
            )));
        }
        if libc::WIFEXITED(status) {
            return Ok(ProcessTransition::Completed(ProcessStatus::Exited(
                libc::WEXITSTATUS(status),
            )));
        }
        Err(WaitError::new(
            io::ErrorKind::Other,
            format!("the host reported an unrecognized wait status {status:#x}"),
        ))
    }
}

/// Terminal attribute and size primitives.
///
/// `dup2`/`close` in `child_descriptors` take only `c_int` and are declared
/// by hand. `tcgetattr`/`tcsetattr` take `*mut termios`, whose layout differs
/// across macOS, Linux and Redox, so the layouts come from `libc` rather than
/// being restated here where a wrong field would corrupt memory silently.
#[allow(unsafe_code)]
mod terminal_mode {
    use std::io;
    use std::os::fd::{AsRawFd, BorrowedFd};

    pub(super) fn is_terminal(fd: BorrowedFd<'_>) -> bool {
        // SAFETY: isatty only inspects the descriptor's kind and never
        // dereferences memory; a borrowed fd is valid for the call.
        unsafe { libc::isatty(fd.as_raw_fd()) == 1 }
    }

    pub(super) fn window_size(fd: BorrowedFd<'_>) -> io::Result<(u16, u16)> {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCGWINSZ writes exactly one `winsize` through the pointer,
        // which points at a live local of that type.
        let result = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &raw mut size) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok((size.ws_col, size.ws_row))
    }

    pub(super) fn current_attributes(fd: BorrowedFd<'_>) -> io::Result<libc::termios> {
        // SAFETY: `zeroed` is a valid bit pattern for `termios` because every
        // field is an integer or an array of integers, and tcgetattr
        // overwrites the whole struct before it is read.
        let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: the pointer targets a live, correctly typed local.
        let result = unsafe { libc::tcgetattr(fd.as_raw_fd(), &raw mut attributes) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(attributes)
    }

    pub(super) fn apply(fd: BorrowedFd<'_>, attributes: &libc::termios) -> io::Result<()> {
        // SAFETY: the pointer targets a live, correctly typed value that
        // tcsetattr only reads.
        let result = unsafe { libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, attributes) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn raw_from(saved: &libc::termios) -> libc::termios {
        let mut raw = *saved;
        // SAFETY: cfmakeraw only writes the flag fields of a live struct.
        unsafe { libc::cfmakeraw(&raw mut raw) };
        // One byte at a time, no inter-byte timer: the decoder is incremental
        // and must not wait for a full escape sequence to arrive.
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        raw
    }
}

/// A raw-mode acquisition on the process's standard input.
#[derive(Debug)]
struct PosixTerminalModeGuard {
    saved: libc::termios,
    restored: bool,
}

impl TerminalModeGuard for PosixTerminalModeGuard {
    fn restore(&mut self) -> Result<(), PlatformError> {
        if self.restored {
            return Ok(());
        }
        let stdin = io::stdin();
        terminal_mode::apply(stdin.as_fd(), &self.saved).map_err(|error| {
            PlatformError::Unavailable {
                capability: Capability::TerminalInfo,
                reason: format!("restoring terminal attributes failed: {error}"),
            }
        })?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for PosixTerminalModeGuard {
    fn drop(&mut self) {
        // A failure here cannot be reported and must not panic during unwind.
        let _ = self.restore();
    }
}

/// Terminal ownership held by a foreground job on the process's standard input.
#[derive(Debug)]
struct PosixForegroundTerminalGuard {
    previous: Option<ProcessGroupId>,
    restored: bool,
}

impl ForegroundTerminalGuard for PosixForegroundTerminalGuard {
    fn restore(&mut self) -> Result<(), PlatformError> {
        if self.restored {
            return Ok(());
        }
        // Nothing owned the terminal when the handover began, so there is no
        // owner to give it back to and the guard is already settled.
        let Some(previous) = self.previous else {
            self.restored = true;
            return Ok(());
        };
        foreground_terminal::hand_over(io::stdin().as_fd(), previous)
            .map_err(foreground_unavailable)?;
        self.restored = true;
        Ok(())
    }

    fn previous_owner(&self) -> Option<ProcessGroupId> {
        self.previous
    }
}

impl Drop for PosixForegroundTerminalGuard {
    fn drop(&mut self) {
        // A failure here cannot be reported and must not panic during unwind.
        let _ = self.restore();
    }
}

fn foreground_unavailable(error: io::Error) -> PlatformError {
    PlatformError::Unavailable {
        capability: Capability::ForegroundTerminal,
        reason: format!("terminal ownership operation failed: {error}"),
    }
}
