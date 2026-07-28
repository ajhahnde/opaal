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
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};

use flashshell_platform::{
    Capabilities, Capability, ChildProcess, DescriptorEndpoint, DescriptorReadError,
    DirectoryEntry, DirectoryEntryKind, DirectoryReadError, DirectoryReadRequest, DirectoryStream,
    FileActionError, FileOpenMode, FileOpenRequest, PipeEndpoints, PipeError, Platform,
    PlatformError, ProcessStatus, SpawnError, SpawnRequest, TerminalModeGuard, TerminalSize,
    TerminateError, WaitError, WorkingDirectoryError, WorkingDirectoryRequest,
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
}

/// POSIX adapter for process and terminal capabilities.
#[derive(Debug, Default, Clone, Copy)]
pub struct PosixPlatform;

/// Owned POSIX child process handle.
#[derive(Debug)]
pub struct PosixChild {
    child: Child,
    completed: Option<ProcessStatus>,
}

impl ChildProcess for PosixChild {
    fn id(&self) -> u64 {
        u64::from(self.child.id())
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        if let Some(status) = self.completed {
            return Ok(status);
        }

        let status = self
            .child
            .wait()
            .map_err(|error| WaitError::new(error.kind(), error.to_string()))?;
        let status = status
            .code()
            .map(ProcessStatus::Exited)
            .or_else(|| status.signal().map(ProcessStatus::Signaled))
            .expect("POSIX exit status has either a code or a signal");
        self.completed = Some(status);
        Ok(status)
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        self.child
            .kill()
            .map_err(|error| TerminateError::new(error.kind(), error.to_string()))
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
        self.require(Capability::FileActions)?;
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
        let descriptor = OwnedDescriptor::adopt(descriptor).map_err(file_action_error)?;
        Ok(Box::new(descriptor))
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
        child_descriptors::configure(&mut command, extra_mappings, closes);

        command
            .spawn()
            .map(|child| {
                Box::new(PosixChild {
                    child,
                    completed: None,
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

fn working_directory_error(error: io::Error) -> WorkingDirectoryError {
    WorkingDirectoryError::Operation {
        kind: error.kind(),
        message: error.to_string(),
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
