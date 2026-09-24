#![deny(unsafe_code)]

//! Unix-like platform primitives for OPAAL.
//!
//! [`PosixPlatform`] is the concrete macOS/Linux adapter. Maintained process
//! execution is available only on Linux because macOS cannot preserve the
//! verified executable identity through process creation. Target-specific
//! adapters remain separate and own their own policy and qualification state.

pub mod operational;

use std::any::Any;
use std::collections::BTreeSet;
use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use opaal_platform::{
    AuthorityEnforcement, AuthorityQuery, Capabilities, Capability, ChildProcess,
    DescriptorEndpoint, DescriptorReadError, DescriptorWriteError, DirectoryEntry,
    DirectoryEntryKind, DirectoryReadError, DirectoryReadRequest, DirectoryStream, ExecutableProbe,
    FileActionError, FileIoEndpoint, FileOpenMode, FileOpenRequest, ForegroundSignalGuard,
    ForegroundTerminalGuard, JobControlSignalGuard, JobSignal, OwnedProcessGroup, PipeEndpoints,
    PipeError, Platform, PlatformError, ProcessGroupError, ProcessGroupId, ProcessStatus,
    ProcessTransition, SignalError, SpawnError, SpawnRequest, StandardDirectories,
    StandardDirectoryEnvironment, TerminalModeGuard, TerminalModeToken, TerminalSize,
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

/// Open one private file relative to a trusted directory descriptor.
///
/// The final component is never followed through a symlink, the returned
/// descriptor is close-on-exec, and creation is exclusive when requested.
/// Keeping this primitive in the POSIX adapter also supplies Redox's `openat`
/// declaration, which is implemented by relibc but not exposed by `rustix`.
pub fn open_private_file_at(
    directory: BorrowedFd<'_>,
    name: &OsStr,
    create_new: bool,
    mode: u32,
) -> io::Result<OwnedFd> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relative file name must contain exactly one normal component",
        ));
    }
    let name = CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "relative file name contains a NUL byte",
        )
    })?;
    let mut flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if create_new {
        flags |= libc::O_CREAT | libc::O_EXCL;
    }
    openat_owned(directory, &name, flags, mode)
}

#[allow(unsafe_code)]
fn openat_owned(
    directory: BorrowedFd<'_>,
    name: &std::ffi::CStr,
    flags: libc::c_int,
    mode: u32,
) -> io::Result<OwnedFd> {
    #[cfg(target_os = "redox")]
    // SAFETY: this declaration matches relibc's POSIX `openat` symbol and ABI;
    // the call below supplies the pointer and variadic-mode invariants.
    unsafe extern "C" {
        #[link_name = "openat"]
        fn redox_openat(
            directory: libc::c_int,
            path: *const libc::c_char,
            flags: libc::c_int,
            mode: libc::mode_t,
        ) -> libc::c_int;
    }

    // SAFETY: `directory` remains borrowed for the call, `name` is a live
    // NUL-terminated C string, and the variadic openat mode is supplied because
    // `O_CREAT` may be present. A nonnegative result transfers one owned fd.
    let descriptor = unsafe {
        #[cfg(target_os = "redox")]
        {
            redox_openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                flags,
                mode as libc::mode_t,
            )
        }
        #[cfg(not(target_os = "redox"))]
        {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                flags,
                mode as libc::c_uint,
            )
        }
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful `openat` returned a new descriptor whose ownership is
    // transferred exactly once to `OwnedFd`.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
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

impl ExecutableProbe for PosixPlatform {
    fn is_executable(&self, path: &OsStr) -> bool {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        metadata.is_file()
            && rustix::fs::accessat(
                rustix::fs::CWD,
                path,
                rustix::fs::Access::EXEC_OK,
                rustix::fs::AtFlags::EACCESS,
            )
            .is_ok()
    }
}

/// Owned POSIX child process handle.
#[derive(Debug)]
pub struct PosixChild {
    child: Child,
    completed: Option<ProcessStatus>,
    process_group: Option<ProcessGroupId>,
}

/// A POSIX process group anchored by one private, unreaped child.
///
/// The anchor starts before any user process or pipeline descriptor exists and
/// stays waitable until release. Its pid therefore reserves the equal process-
/// group identifier even after every user member exits, closing the numeric
/// reuse race around final group cleanup.
#[derive(Debug)]
struct PosixOwnedProcessGroup {
    group: ProcessGroupId,
    anchor: libc::pid_t,
    control: Option<OwnedFd>,
    watchdog: Option<process_group_owner::SignalWatchdog>,
    released: bool,
}

impl OwnedProcessGroup for PosixOwnedProcessGroup {
    fn id(&self) -> ProcessGroupId {
        self.group
    }

    fn release(&mut self) -> Result<(), ProcessGroupError> {
        if self.released {
            return Ok(());
        }
        // The signal guard can outlive this owner while the runtime restores
        // terminal and process signal state. Drain any handler that already
        // loaded this group before its identifier becomes reusable.
        foreground_signals::deactivate_group(self.group);
        self.released = true;
        let watchdog_result = self
            .watchdog
            .take()
            .map(process_group_owner::SignalWatchdog::stop);
        drop(self.control.take());
        let anchor_result = process_group_owner::terminate_and_reap(self.anchor);
        watchdog_result
            .transpose()
            .and(anchor_result)
            .map_err(process_group_operation_error)
    }
}

impl Drop for PosixOwnedProcessGroup {
    fn drop(&mut self) {
        let _ = self.release();
    }
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
        #[cfg(target_os = "macos")]
        let transition = child_wait::observe_transition(self.pid()?)?;
        #[cfg(not(target_os = "macos"))]
        let transition = child_wait::observe(self.pid()?, libc::WUNTRACED | libc::WCONTINUED)?;
        if let ProcessTransition::Completed(status) = transition {
            self.completed = Some(status);
        }
        Ok(transition)
    }

    fn try_wait_for_transition(&mut self) -> Result<Option<ProcessTransition>, WaitError> {
        if let Some(status) = self.completed {
            return Ok(Some(ProcessTransition::Completed(status)));
        }
        let transition = child_wait::try_observe(self.pid()?, libc::WUNTRACED | libc::WCONTINUED)?;
        if let Some(ProcessTransition::Completed(status)) = transition {
            self.completed = Some(status);
        }
        Ok(transition)
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
    fn pid(&self) -> Result<libc::pid_t, WaitError> {
        libc::pid_t::try_from(self.child.id()).map_err(|_| {
            WaitError::new(
                io::ErrorKind::InvalidInput,
                "the child identifier exceeds the POSIX process identifier range",
            )
        })
    }

    /// Reap the child for one transition, caching a completion.
    ///
    /// The adapter reaps for itself rather than through `std::process::Child`,
    /// so the completion is recorded here and every later call short-circuits on
    /// it. Nothing else reaps the pid, so the recorded status is the only one
    /// the child ever produces.
    fn observe(&mut self, flags: libc::c_int) -> Result<ProcessTransition, WaitError> {
        let pid = self.pid()?;
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

    fn authority_enforcement(&self, query: AuthorityQuery<'_>) -> AuthorityEnforcement {
        match query.effect() {
            opaal_platform::AuthorityEffect::ProcessRun if cfg!(target_os = "linux") => {
                AuthorityEnforcement::Unenforced
            }
            opaal_platform::AuthorityEffect::ProcessRun => AuthorityEnforcement::Unsupported,
            opaal_platform::AuthorityEffect::FilesystemRead
            | opaal_platform::AuthorityEffect::FilesystemWrite
            | opaal_platform::AuthorityEffect::NetworkHttp
            | opaal_platform::AuthorityEffect::SecretReveal
            | opaal_platform::AuthorityEffect::ClockWall
            | opaal_platform::AuthorityEffect::ClockMonotonic => AuthorityEnforcement::Enforced,
        }
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
        let saved_status = terminal_mode::status_flags(stdin.as_fd()).map_err(|error| {
            let _ = terminal_mode::apply(stdin.as_fd(), &saved);
            PlatformError::Unavailable {
                capability: Capability::TerminalInfo,
                reason: format!("reading terminal status flags failed: {error}"),
            }
        })?;
        terminal_mode::apply_status_flags(stdin.as_fd(), saved_status | libc::O_NONBLOCK).map_err(
            |error| {
                let _ = terminal_mode::apply(stdin.as_fd(), &saved);
                PlatformError::Unavailable {
                    capability: Capability::TerminalInfo,
                    reason: format!("entering nonblocking terminal mode failed: {error}"),
                }
            },
        )?;
        Ok(Box::new(PosixTerminalModeGuard {
            saved,
            saved_status,
            restored: false,
        }))
    }

    fn read_terminal_input(
        &self,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> Result<Option<usize>, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        terminal_input::read(buffer, timeout).map_err(|error| PlatformError::Unavailable {
            capability: Capability::TerminalInfo,
            reason: format!("reading terminal input failed: {error}"),
        })
    }

    fn snapshot_terminal_mode(&self) -> Result<Box<dyn TerminalModeToken>, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        let stdin = io::stdin();
        if !terminal_mode::is_terminal(stdin.as_fd()) {
            return Err(PlatformError::Unavailable {
                capability: Capability::TerminalInfo,
                reason: "standard input is not a terminal".to_owned(),
            });
        }
        terminal_mode::current_attributes(stdin.as_fd())
            .map(|attributes| {
                Box::new(PosixTerminalModeToken { attributes }) as Box<dyn TerminalModeToken>
            })
            .map_err(|error| PlatformError::Unavailable {
                capability: Capability::TerminalInfo,
                reason: format!("reading terminal attributes failed: {error}"),
            })
    }

    fn apply_terminal_mode(&self, token: &dyn TerminalModeToken) -> Result<(), PlatformError> {
        self.require(Capability::TerminalInfo)?;
        let token = token
            .as_any()
            .downcast_ref::<PosixTerminalModeToken>()
            .ok_or_else(|| PlatformError::Unavailable {
                capability: Capability::TerminalInfo,
                reason: "terminal mode token belongs to another adapter".to_owned(),
            })?;
        terminal_mode::apply(io::stdin().as_fd(), &token.attributes).map_err(|error| {
            PlatformError::Unavailable {
                capability: Capability::TerminalInfo,
                reason: format!("applying terminal attributes failed: {error}"),
            }
        })
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

    fn create_process_group(&self) -> Result<Box<dyn OwnedProcessGroup>, ProcessGroupError> {
        self.require(Capability::ProcessGroups)?;
        let (anchor, control, watchdog) =
            process_group_owner::create().map_err(process_group_operation_error)?;
        let group = ProcessGroupId::new(u64::try_from(anchor).map_err(|_| {
            ProcessGroupError::Operation {
                kind: io::ErrorKind::InvalidData,
                message: "the process-group anchor had a negative identifier".to_owned(),
            }
        })?)
        .ok_or_else(|| ProcessGroupError::Operation {
            kind: io::ErrorKind::InvalidData,
            message: "the process-group anchor had the reserved zero identifier".to_owned(),
        })?;
        Ok(Box::new(PosixOwnedProcessGroup {
            group,
            anchor,
            control: Some(control),
            watchdog: Some(watchdog),
            released: false,
        }))
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

    fn prepare_foreground_signals(&self) -> Result<Box<dyn ForegroundSignalGuard>, PlatformError> {
        self.require(Capability::Signals)?;
        foreground_signals::prepare().map(|guard| Box::new(guard) as Box<dyn ForegroundSignalGuard>)
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

    fn standard_directories(
        &self,
        environment: &dyn StandardDirectoryEnvironment,
    ) -> Result<StandardDirectories, PlatformError> {
        self.require(Capability::StandardDirectories)?;
        let home = absolute_environment_path(environment, "HOME").ok_or_else(|| {
            PlatformError::Unavailable {
                capability: Capability::StandardDirectories,
                reason: "HOME does not name an absolute directory".to_owned(),
            }
        })?;
        let config = absolute_environment_path(environment, "XDG_CONFIG_HOME")
            .unwrap_or_else(|| host_config_root(&home));
        let cache = absolute_environment_path(environment, "XDG_CACHE_HOME")
            .unwrap_or_else(|| host_cache_root(&home));
        let state = absolute_environment_path(environment, "XDG_STATE_HOME")
            .unwrap_or_else(|| host_state_root(&home));
        Ok(StandardDirectories::new(home, config, cache, state))
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
        let descriptor = inherited_descriptor::duplicate(descriptor).map_err(file_action_error)?;
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

#[allow(unsafe_code)]
mod inherited_descriptor {
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd};

    pub(super) fn duplicate(descriptor: u32) -> io::Result<OwnedFd> {
        let descriptor = libc::c_int::try_from(descriptor).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited descriptor exceeds the POSIX descriptor range",
            )
        })?;
        // SAFETY: fcntl validates the caller-supplied descriptor before
        // returning a new descriptor. A nonnegative result is a fresh owned
        // descriptor with close-on-exec set; no borrowed Rust descriptor is
        // constructed from an untrusted raw value.
        let duplicated = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful F_DUPFD_CLOEXEC result is a fresh descriptor
        // whose ownership has not been transferred elsewhere.
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
    }
}

fn absolute_environment_path(
    environment: &dyn StandardDirectoryEnvironment,
    name: &str,
) -> Option<PathBuf> {
    environment
        .value(OsStr::new(name))
        .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
        .map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn host_config_root(home: &Path) -> PathBuf {
    home.join("Library/Application Support")
}

#[cfg(not(target_os = "macos"))]
fn host_config_root(home: &Path) -> PathBuf {
    home.join(".config")
}

#[cfg(target_os = "macos")]
fn host_cache_root(home: &Path) -> PathBuf {
    home.join("Library/Caches")
}

#[cfg(not(target_os = "macos"))]
fn host_cache_root(home: &Path) -> PathBuf {
    home.join(".cache")
}

#[cfg(target_os = "macos")]
fn host_state_root(home: &Path) -> PathBuf {
    home.join("Library/Application Support")
}

#[cfg(not(target_os = "macos"))]
fn host_state_root(home: &Path) -> PathBuf {
    home.join(".local/state")
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

fn process_group_operation_error(error: io::Error) -> ProcessGroupError {
    ProcessGroupError::Operation {
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

/// A private anchor process that reserves one foreground process-group
/// identifier until the parent explicitly releases it.
///
/// Fork is used only to create the anchor. The child performs exclusively
/// async-signal-safe syscalls before `_exit`: it allocates nothing, takes no
/// Rust lock, and runs no destructor. The private pipes are close-on-exec in
/// the parent, so later user programs cannot retain the ownership channel.
#[allow(unsafe_code)]
mod process_group_owner {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    const READY: u8 = 1;
    const FAILED: u8 = 0;
    const SIGNAL: u8 = 1;
    const CANCEL: u8 = 2;
    const GRACE: Duration = Duration::from_secs(2);

    // These statics are used only by the forked anchor's signal handler. The
    // child has its own address space, so the parent never changes them.
    static SIGNAL_WRITER: AtomicI32 = AtomicI32::new(-1);
    static SIGNAL_REPORTED: AtomicBool = AtomicBool::new(false);

    pub(super) struct SignalWatchdog {
        cancel: OwnedFd,
        reader_owner: OwnedFd,
        thread: thread::JoinHandle<io::Result<()>>,
    }

    impl std::fmt::Debug for SignalWatchdog {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("SignalWatchdog")
                .finish_non_exhaustive()
        }
    }

    impl SignalWatchdog {
        pub(super) fn stop(self) -> io::Result<()> {
            let cancel = CANCEL;
            // SAFETY: the descriptor and one-byte value remain live for the call.
            let written =
                unsafe { libc::write(self.cancel.as_raw_fd(), (&raw const cancel).cast(), 1) };
            let write_error = (written != 1).then(io::Error::last_os_error);
            drop(self.cancel);
            drop(self.reader_owner);
            let joined = self
                .thread
                .join()
                .map_err(|_| io::Error::other("the process-group signal watchdog panicked"))?;
            if let Some(error) = write_error {
                return Err(error);
            }
            joined
        }
    }

    pub(super) fn create() -> io::Result<(libc::pid_t, OwnedFd, SignalWatchdog)> {
        let control = pipe_cloexec()?;
        let ready = match pipe_cloexec() {
            Ok(pipe) => pipe,
            Err(error) => {
                close_pair(control);
                return Err(error);
            }
        };
        let notification = match pipe_cloexec() {
            Ok(pipe) => pipe,
            Err(error) => {
                close_pair(control);
                close_pair(ready);
                return Err(error);
            }
        };

        // SAFETY: the child branch calls only the async-signal-safe routine
        // below and never returns to Rust. The parent retains ordinary Rust
        // ownership and wraps exactly one still-open descriptor.
        let anchor = unsafe { libc::fork() };
        if anchor == -1 {
            let error = io::Error::last_os_error();
            close_pair(control);
            close_pair(ready);
            close_pair(notification);
            return Err(error);
        }
        if anchor == 0 {
            child_main(control, ready, notification);
        }

        close(control[0]);
        close(ready[1]);
        let readiness = read_byte(ready[0]);
        close(ready[0]);
        match readiness {
            Ok(READY) => {
                // SAFETY: each raw descriptor has exactly one parent owner.
                let reader = unsafe { OwnedFd::from_raw_fd(notification[0]) };
                // SAFETY: this is the distinct writable end of the pipe.
                let cancel = unsafe { OwnedFd::from_raw_fd(notification[1]) };
                let reader_owner = match reader.try_clone() {
                    Ok(owner) => owner,
                    Err(error) => {
                        close(control[1]);
                        let _ = terminate_and_reap(anchor);
                        return Err(error);
                    }
                };
                let watcher = thread::Builder::new()
                    .name("opaal-foreground-signal-watchdog".to_owned())
                    .spawn(move || watch_signals(reader, anchor));
                let watcher = match watcher {
                    Ok(thread) => SignalWatchdog {
                        cancel,
                        reader_owner,
                        thread,
                    },
                    Err(error) => {
                        close(control[1]);
                        let _ = terminate_and_reap(anchor);
                        return Err(error);
                    }
                };
                // SAFETY: this is the sole remaining owner of the descriptor
                // in the parent branch and it is released exactly once.
                let control = unsafe { OwnedFd::from_raw_fd(control[1]) };
                Ok((anchor, control, watcher))
            }
            Ok(_) => {
                close(control[1]);
                close_pair(notification);
                let _ = terminate_and_reap(anchor);
                Err(io::Error::other(
                    "the process-group anchor could not establish its group",
                ))
            }
            Err(error) => {
                close(control[1]);
                close_pair(notification);
                let _ = terminate_and_reap(anchor);
                Err(error)
            }
        }
    }

    fn watch_signals(reader: OwnedFd, anchor: libc::pid_t) -> io::Result<()> {
        let mut deadline = None;
        loop {
            let timeout = deadline.map_or(-1, |end: Instant| {
                let remaining = end.saturating_duration_since(Instant::now());
                i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX)
            });
            let mut event = libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: event is live writable storage for one poll descriptor.
            let polled = unsafe { libc::poll(&raw mut event, 1, timeout) };
            if polled == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if polled == 0 {
                // The anchor remains unreaped until this thread joins, so its
                // group number cannot have been reused by another job.
                // SAFETY: negative anchor targets that reserved process group.
                if unsafe { libc::kill(-anchor, libc::SIGKILL) } == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::NotFound {
                        return Err(error);
                    }
                }
                return Ok(());
            }
            match read_byte(reader.as_raw_fd())? {
                CANCEL | FAILED => return Ok(()),
                SIGNAL if deadline.is_none() => deadline = Some(Instant::now() + GRACE),
                _ => {}
            }
        }
    }

    extern "C" fn report_signal(_signal: libc::c_int) {
        if !SIGNAL_REPORTED.swap(true, Ordering::Relaxed) {
            let writer = SIGNAL_WRITER.load(Ordering::Relaxed);
            if writer >= 0 {
                // SAFETY: write with one static byte is async-signal-safe. The
                // handler writes at most once, so the pipe cannot fill here.
                let signal = SIGNAL;
                // SAFETY: writer is the anchor's live pipe descriptor and signal is live.
                unsafe { libc::write(writer, (&raw const signal).cast(), 1) };
            }
        }
    }

    pub(super) fn terminate_and_reap(anchor: libc::pid_t) -> io::Result<()> {
        // The anchor pid stays allocated until this wait, so the targeted kill
        // cannot reach a reused process identity.
        // SAFETY: kill takes scalar values and the retained positive pid names
        // the unreaped child created by `create`.
        let killed = unsafe { libc::kill(anchor, libc::SIGKILL) };
        let kill_error = (killed == -1).then(io::Error::last_os_error);

        let mut status = 0;
        loop {
            // SAFETY: status points to a live integer and `anchor` is the exact
            // direct child that has not been waited elsewhere.
            let waited = unsafe { libc::waitpid(anchor, &raw mut status, 0) };
            if waited == anchor {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() != Some(libc::ECHILD) {
                return Err(error);
            }
            break;
        }

        if let Some(error) = kill_error
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error);
        }
        Ok(())
    }

    fn pipe_cloexec() -> io::Result<[libc::c_int; 2]> {
        let mut pipe = [-1; 2];
        // SAFETY: pipe points to storage for exactly two descriptors.
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        for descriptor in pipe {
            // SAFETY: fcntl receives one valid descriptor and scalar flags.
            if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                let error = io::Error::last_os_error();
                close_pair(pipe);
                return Err(error);
            }
        }
        Ok(pipe)
    }

    fn read_byte(descriptor: libc::c_int) -> io::Result<u8> {
        let mut byte = 0u8;
        loop {
            // SAFETY: byte is live writable storage for the requested byte.
            let read = unsafe { libc::read(descriptor, (&raw mut byte).cast(), 1) };
            if read == 1 {
                return Ok(byte);
            }
            if read == 0 {
                return Ok(FAILED);
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
    }

    fn close_pair(pair: [libc::c_int; 2]) {
        close(pair[0]);
        close(pair[1]);
    }

    fn close(descriptor: libc::c_int) {
        if descriptor >= 0 {
            // SAFETY: close takes one scalar descriptor; repeated failure is
            // irrelevant during best-effort setup cleanup.
            unsafe {
                libc::close(descriptor);
            }
        }
    }

    fn child_main(
        control: [libc::c_int; 2],
        ready: [libc::c_int; 2],
        notification: [libc::c_int; 2],
    ) -> ! {
        close(control[1]);
        close(ready[0]);
        close(notification[0]);
        SIGNAL_WRITER.store(notification[1], Ordering::Relaxed);

        // Block every catchable signal except the three supported termination
        // signals, which the anchor reports to its parent's watchdog.
        // SAFETY: the zero pattern is a valid base and sigfillset initializes
        // the complete live set before sigprocmask reads it.
        let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: all arguments point to live storage or are scalar values.
        let blocked_ok = unsafe { libc::sigfillset(&raw mut blocked) } != -1
            && unsafe {
                libc::sigprocmask(libc::SIG_SETMASK, &raw const blocked, std::ptr::null_mut())
            } != -1;
        // SAFETY: zero is a valid base for sigaction; sigemptyset fills the mask.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = report_signal as *const () as usize;
        // SAFETY: action contains live writable mask storage.
        let handlers_ok = unsafe { libc::sigemptyset(&raw mut action.sa_mask) } != -1
            && [libc::SIGHUP, libc::SIGINT, libc::SIGTERM]
                .into_iter()
                // SAFETY: action is initialized and remains live for each call.
                .all(|signal| unsafe {
                    libc::sigaction(signal, &raw const action, std::ptr::null_mut()) != -1
                });
        // SAFETY: setpgid with two zeros makes the calling child the leader of
        // a new group named by its own pid.
        let grouped = blocked_ok
            && handlers_ok
            && unsafe { libc::setpgid(0, 0) } != -1
            && unsafe { libc::sigdelset(&raw mut blocked, libc::SIGHUP) } != -1
            && unsafe { libc::sigdelset(&raw mut blocked, libc::SIGINT) } != -1
            && unsafe { libc::sigdelset(&raw mut blocked, libc::SIGTERM) } != -1
            && unsafe {
                libc::sigprocmask(libc::SIG_SETMASK, &raw const blocked, std::ptr::null_mut())
            } != -1;
        let state = if grouped { READY } else { FAILED };
        // SAFETY: state is one initialized byte and ready[1] is the private
        // writable end inherited from the parent.
        unsafe {
            libc::write(ready[1], (&raw const state).cast(), 1);
            libc::close(ready[1]);
        }
        if !grouped {
            // SAFETY: `_exit` terminates without invoking inherited Rust state.
            unsafe { libc::_exit(127) }
        }

        let mut byte = 0u8;
        loop {
            // SAFETY: byte is live and control[0] is the private read end.
            let read = unsafe { libc::read(control[0], (&raw mut byte).cast(), 1) };
            if read == 0 {
                break;
            }
            if read == -1 {
                // The supported-signal handler interrupts this read. Retry;
                // ownership still belongs to the parent until it closes the
                // control channel.
                continue;
            }
        }
        // Closing the parent owner is a last-resort cleanup path. Signal the
        // whole still-owned group, including this anchor, without ever exposing
        // a reusable numeric identifier.
        // SAFETY: target zero names the caller's current process group.
        unsafe {
            libc::kill(0, libc::SIGKILL);
            libc::_exit(127)
        }
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
/// interactive shell ignores the job-control signals for its own survival and
/// installs a foreground termination handler. A child must replace both before
/// a later pre-exec hook can receive a signal. The mask is cleared for the same
/// reason: `fork` copies the calling thread's mask, and an executed program is
/// entitled to a clear one.
#[allow(unsafe_code)]
mod child_signal_dispositions {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// Signal dispositions a spawned program must not inherit from its shell.
    ///
    /// The interactive shell arranges the job-control signals for its own
    /// survival and handles foreground termination. A reserved background-chain
    /// supervisor additionally ignores hang-up so it can wait for its
    /// descendants after the parent signals the group; ordinary external
    /// descendants must restore the default instead.
    const RESET: [libc::c_int; 7] = [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGTERM,
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

    #[cfg(test)]
    #[allow(unsafe_code)]
    mod tests {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        use std::process::Command;

        use super::configure;

        #[test]
        fn resets_termination_before_later_pre_exec_hooks() {
            if std::env::var_os("OPAAL_TEST_PRE_EXEC_TERM").is_none() {
                let status = Command::new(std::env::current_exe().expect("test binary path"))
                    .arg("--exact")
                    .arg("child_signal_dispositions::tests::resets_termination_before_later_pre_exec_hooks")
                    .env("OPAAL_TEST_PRE_EXEC_TERM", "1")
                    .status()
                    .expect("isolated test process starts");
                assert!(status.success(), "isolated test process failed: {status}");
                return;
            }

            // The child must replace an inherited ignore before the later
            // pre-exec hook raises TERM. Exec itself would reset a caught
            // handler, so signalling after exec would miss this interval.
            // SAFETY: the action and mask are initialized before sigaction
            // reads them in this isolated test process.
            unsafe {
                let mut ignored: libc::sigaction = std::mem::zeroed();
                ignored.sa_sigaction = libc::SIG_IGN;
                assert_eq!(libc::sigemptyset(&raw mut ignored.sa_mask), 0);
                assert_eq!(
                    libc::sigaction(libc::SIGTERM, &raw const ignored, std::ptr::null_mut()),
                    0
                );
            }

            let mut command = Command::new("/bin/true");
            configure(&mut command);
            // SAFETY: the child hook calls only the async-signal-safe raise
            // operation before exec and captures no parent state.
            unsafe {
                command.pre_exec(|| {
                    libc::raise(libc::SIGTERM);
                    Ok(())
                });
            }
            let status = command
                .spawn()
                .expect("child starts")
                .wait()
                .expect("child is waitable");
            assert_eq!(status.signal(), Some(libc::SIGTERM));
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

    use opaal_platform::{ProcessGroup, ProcessGroupId, SpawnError};

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

    // SAFETY: these declarations match the POSIX C signatures. Calls remain
    // inside the pre-exec hook below, which supplies only scalar descriptors.
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

    use opaal_platform::ProcessGroupId;

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

    use opaal_platform::{Capability, PlatformError};

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

/// Scoped forwarding of the three termination signals supported by foreground
/// OPAAL execution.
///
/// Preparation blocks the signals on the calling thread before installing the
/// handlers. The runtime activates the guard after the first verified child
/// joins its owned group, at which point the prior mask is restored. A signal
/// handled by another thread before activation is retained for that child.
/// The handler performs only lock-free atomics and POSIX `kill`, which are
/// async-signal-safe on the two qualified targets.
#[allow(unsafe_code)]
mod foreground_signals {
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

    use opaal_platform::{Capability, ForegroundSignalGuard, PlatformError, ProcessGroupId};

    const FORWARDED: [libc::c_int; 3] = [libc::SIGHUP, libc::SIGINT, libc::SIGTERM];

    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static ACTIVE_GROUP: AtomicI32 = AtomicI32::new(0);
    static FORWARDING_HANDLERS: AtomicUsize = AtomicUsize::new(0);
    static PENDING_SIGNALS: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn deactivate_group(group: ProcessGroupId) {
        let Ok(group) = libc::pid_t::try_from(group.get()) else {
            return;
        };
        let _ = ACTIVE_GROUP.compare_exchange(group, 0, Ordering::SeqCst, Ordering::SeqCst);
        // A handler increments before loading ACTIVE_GROUP. Once this reaches
        // zero, every handler that could have loaded `group` has completed its
        // kill call; later handlers observe zero instead. Drain even when a
        // guard restored its actions just before this owner was released.
        while FORWARDING_HANDLERS.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
    }

    pub(super) struct PosixForegroundSignalGuard {
        previous: [libc::sigaction; 3],
        previous_mask: libc::sigset_t,
        blocked_mask: libc::sigset_t,
        activated: bool,
        restored: bool,
    }

    impl std::fmt::Debug for PosixForegroundSignalGuard {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("PosixForegroundSignalGuard")
                .field("activated", &self.activated)
                .field("restored", &self.restored)
                .finish_non_exhaustive()
        }
    }

    pub(super) fn prepare() -> Result<PosixForegroundSignalGuard, PlatformError> {
        if ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(unavailable(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another foreground signal guard is already active",
            )));
        }
        PENDING_SIGNALS.store(0, Ordering::SeqCst);

        let blocked = match signal_set() {
            Ok(blocked) => blocked,
            Err(error) => {
                ACTIVE.store(false, Ordering::Release);
                return Err(error);
            }
        };
        // SAFETY: an all-zero pattern is a valid base for sigset_t, and libc
        // initializes the complete output before any later read.
        let mut previous_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `blocked` and `previous_mask` are live, correctly typed signal
        // sets. pthread_sigmask changes only the calling thread's mask.
        let masked = unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &raw const blocked, &raw mut previous_mask)
        };
        if masked != 0 {
            ACTIVE.store(false, Ordering::Release);
            return Err(unavailable(io::Error::from_raw_os_error(masked)));
        }

        // SAFETY: every slot is overwritten by sigaction before it is read.
        let mut previous: [libc::sigaction; 3] = unsafe { std::mem::zeroed() };
        // SAFETY: the zero pattern is a valid base and sigemptyset initializes
        // the mask field before the action is passed to sigaction.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = forward as *const () as usize;
        // SAFETY: the pointer targets the live mask field of `action`.
        if unsafe { libc::sigemptyset(&raw mut action.sa_mask) } == -1 {
            let _ = restore_mask(&previous_mask);
            ACTIVE.store(false, Ordering::Release);
            return Err(unavailable(io::Error::last_os_error()));
        }

        for index in 0..FORWARDED.len() {
            // SAFETY: both pointers target live initialized values and the
            // index is bounded by the arrays being traversed.
            let installed = unsafe {
                libc::sigaction(
                    FORWARDED[index],
                    &raw const action,
                    &raw mut previous[index],
                )
            };
            if installed == -1 {
                let failure = io::Error::last_os_error();
                undo_actions(&previous, index);
                let _ = restore_mask(&previous_mask);
                ACTIVE.store(false, Ordering::Release);
                return Err(unavailable(failure));
            }
        }

        Ok(PosixForegroundSignalGuard {
            previous,
            previous_mask,
            blocked_mask: blocked,
            activated: false,
            restored: false,
        })
    }

    extern "C" fn forward(signal: libc::c_int) {
        FORWARDING_HANDLERS.fetch_add(1, Ordering::SeqCst);
        let bit = match signal {
            libc::SIGHUP => 1,
            libc::SIGINT => 2,
            libc::SIGTERM => 4,
            _ => 0,
        };
        PENDING_SIGNALS.fetch_or(bit, Ordering::SeqCst);
        let group = ACTIVE_GROUP.load(Ordering::SeqCst);
        if group > 1 && PENDING_SIGNALS.fetch_and(!bit, Ordering::SeqCst) & bit != 0 {
            // SAFETY: kill takes only scalar values. The negative target names
            // the exact active process group and the received signal is one of
            // the three handlers installed above.
            unsafe {
                libc::kill(-group, signal);
            }
        }
        FORWARDING_HANDLERS.fetch_sub(1, Ordering::SeqCst);
    }

    impl PosixForegroundSignalGuard {
        fn restore_now(&mut self) -> Result<(), PlatformError> {
            if self.restored {
                return Ok(());
            }
            self.restored = true;

            // SAFETY: both sets are live. Blocking closes the interval between
            // clearing the destination group and restoring the old handlers.
            let masked = unsafe {
                libc::pthread_sigmask(
                    libc::SIG_BLOCK,
                    &raw const self.blocked_mask,
                    std::ptr::null_mut(),
                )
            };
            let mut first_error = (masked != 0).then(|| io::Error::from_raw_os_error(masked));

            ACTIVE_GROUP.store(0, Ordering::SeqCst);
            PENDING_SIGNALS.store(0, Ordering::SeqCst);
            for (previous, signal) in self.previous.iter().zip(FORWARDED) {
                // SAFETY: each disposition was captured from this exact signal
                // during preparation and the pointer stays live for the call.
                if unsafe { libc::sigaction(signal, &raw const *previous, std::ptr::null_mut()) }
                    == -1
                    && first_error.is_none()
                {
                    first_error = Some(io::Error::last_os_error());
                }
            }
            // Always attempt to restore the caller's complete prior mask.
            let restored_mask = restore_mask(&self.previous_mask);
            if first_error.is_none() {
                first_error = restored_mask.err();
            }
            ACTIVE.store(false, Ordering::Release);

            first_error.map_or(Ok(()), |error| Err(unavailable(error)))
        }
    }

    impl ForegroundSignalGuard for PosixForegroundSignalGuard {
        fn forward_to(&mut self, group: ProcessGroupId) -> Result<(), PlatformError> {
            if self.restored {
                return Err(unavailable(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the foreground signal guard was already restored",
                )));
            }
            if self.activated {
                return Err(unavailable(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the foreground signal guard already has a process group",
                )));
            }
            let group = libc::pid_t::try_from(group.get()).map_err(|_| {
                unavailable(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("process group {group} exceeds the POSIX process identifier range"),
                ))
            })?;
            if group == 1 {
                return Err(unavailable(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "process group 1 is not addressable as a group",
                )));
            }

            ACTIVE_GROUP.store(group, Ordering::SeqCst);
            let pending = PENDING_SIGNALS.swap(0, Ordering::SeqCst);
            for (index, signal) in FORWARDED.iter().enumerate() {
                if pending & (1 << index) != 0 {
                    // SAFETY: the owned group is still anchored, and the
                    // pending signal is one of the installed dispositions.
                    if unsafe { libc::kill(-group, *signal) } == -1 {
                        let error = io::Error::last_os_error();
                        ACTIVE_GROUP.store(0, Ordering::SeqCst);
                        return Err(unavailable(error));
                    }
                }
            }
            if let Err(error) = restore_mask(&self.previous_mask) {
                ACTIVE_GROUP.store(0, Ordering::SeqCst);
                return Err(unavailable(error));
            }
            self.activated = true;
            Ok(())
        }

        fn restore(&mut self) -> Result<(), PlatformError> {
            self.restore_now()
        }
    }

    impl Drop for PosixForegroundSignalGuard {
        fn drop(&mut self) {
            let _ = self.restore_now();
        }
    }

    fn signal_set() -> Result<libc::sigset_t, PlatformError> {
        // SAFETY: the zero pattern is a valid base and sigemptyset initializes
        // the complete set before it is returned.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: the pointer targets the live local signal set.
        if unsafe { libc::sigemptyset(&raw mut set) } == -1 {
            return Err(unavailable(io::Error::last_os_error()));
        }
        for signal in FORWARDED {
            // SAFETY: the pointer remains live and `signal` is a valid POSIX
            // signal constant.
            if unsafe { libc::sigaddset(&raw mut set, signal) } == -1 {
                return Err(unavailable(io::Error::last_os_error()));
            }
        }
        Ok(set)
    }

    fn restore_mask(mask: &libc::sigset_t) -> Result<(), io::Error> {
        // SAFETY: the pointer targets a mask captured or initialized by libc;
        // the null output pointer requests no previous-mask snapshot.
        let restored = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const *mask, std::ptr::null_mut())
        };
        if restored == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(restored))
        }
    }

    fn undo_actions(previous: &[libc::sigaction; 3], installed: usize) {
        for index in 0..installed {
            // SAFETY: every restored slot was initialized by a successful
            // sigaction call earlier in preparation.
            unsafe {
                libc::sigaction(
                    FORWARDED[index],
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

    use opaal_platform::{Capability, JobControlSignalGuard, PlatformError};

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

    use opaal_platform::{JobSignal, ProcessGroupId, SignalError};

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
    #[cfg(target_os = "macos")]
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use opaal_platform::{ProcessStatus, ProcessTransition, WaitError};

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

    pub(super) fn try_observe(
        pid: c_int,
        flags: c_int,
    ) -> Result<Option<ProcessTransition>, WaitError> {
        let mut status: c_int = 0;
        loop {
            // SAFETY: waitpid writes only through the status pointer, which
            // points at a live local for the whole call.
            let result = unsafe { libc::waitpid(pid, &raw mut status, flags | libc::WNOHANG) };
            if result == pid {
                return decode(status).map(Some);
            }
            if result == 0 {
                return Ok(None);
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(WaitError::new(error.kind(), error.to_string()));
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn observe_transition(pid: c_int) -> Result<ProcessTransition, WaitError> {
        if let Some(transition) = observe_nonblocking(pid)? {
            return Ok(transition);
        }

        // Darwin's blocking waitpid can remain asleep when a terminal-stopped
        // child is resumed by another process. EVFILT_PROC supplies the missing
        // wakeup without a signal handler, timer, or polling loop; waitpid still
        // remains the sole consumer and classifier of child status.
        // SAFETY: kqueue takes no arguments and returns a new descriptor or -1.
        let descriptor = unsafe { libc::kqueue() };
        if descriptor == -1 {
            let error = io::Error::last_os_error();
            return Err(WaitError::new(error.kind(), error.to_string()));
        }
        // SAFETY: the descriptor was just created successfully and ownership
        // transfers exactly once into this guard.
        let queue = unsafe { OwnedFd::from_raw_fd(descriptor) };
        // SAFETY: an all-zero kevent is a valid base that is fully initialized
        // below before the kernel reads it.
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = pid as libc::uintptr_t;
        change.filter = libc::EVFILT_PROC;
        change.flags = (libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR) as libc::c_ushort;
        change.fflags = libc::NOTE_SIGNAL | libc::NOTE_EXIT;

        // SAFETY: change points to one initialized event, the output list is
        // absent with count zero, and the queue descriptor remains owned.
        let registered = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &raw const change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if registered == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                // A short-lived child can exit between the nonblocking wait and
                // registration. It still owns a waitable terminal status even
                // though kqueue can no longer attach to the live process.
                return observe(pid, libc::WUNTRACED | libc::WCONTINUED);
            }
            return Err(WaitError::new(error.kind(), error.to_string()));
        }

        loop {
            if let Some(transition) = observe_nonblocking(pid)? {
                return Ok(transition);
            }

            // SAFETY: kevent initializes every returned event field; zeroing
            // the destination first also leaves a valid value on failure.
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            // SAFETY: event points to space for exactly one result, no change
            // list is supplied, and a null timeout requests an event-driven
            // blocking wait.
            let received = unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &raw mut event,
                    1,
                    std::ptr::null(),
                )
            };
            if received == 1 {
                if event.flags & libc::EV_ERROR as libc::c_ushort != 0 {
                    let code = i32::try_from(event.data).unwrap_or(libc::EINVAL);
                    let error = io::Error::from_raw_os_error(code);
                    return Err(WaitError::new(error.kind(), error.to_string()));
                }
                continue;
            }
            if received == 0 {
                return Err(WaitError::new(
                    io::ErrorKind::Other,
                    "kqueue returned no process event without a timeout",
                ));
            }
            let error = io::Error::last_os_error();
            if received == -1 && error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(WaitError::new(error.kind(), error.to_string()));
        }
    }

    #[cfg(target_os = "macos")]
    fn observe_nonblocking(pid: c_int) -> Result<Option<ProcessTransition>, WaitError> {
        let mut status: c_int = 0;
        loop {
            // SAFETY: waitpid writes only through the status pointer, which
            // points at a live local for the whole call.
            let result = unsafe {
                libc::waitpid(
                    pid,
                    &raw mut status,
                    libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
                )
            };
            if result == pid {
                return decode(status).map(Some);
            }
            if result == 0 {
                return Ok(None);
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(WaitError::new(error.kind(), error.to_string()));
        }
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
mod terminal_input {
    use std::io;
    use std::time::Duration;

    #[cfg(target_os = "redox")]
    pub(super) fn read(buffer: &mut [u8], timeout: Duration) -> io::Result<Option<usize>> {
        // Redox PTYs can retain a queued byte without delivering a matching
        // readiness notification. Avoid that lossy seam entirely: raw mode
        // makes the descriptor nonblocking, and this bounded retry preserves
        // the caller's timeout without spinning.
        // SAFETY: F_GETFL only inspects the status flags of standard input.
        let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if flags & libc::O_NONBLOCK == 0 {
            return Err(io::Error::other(
                "Redox terminal input descriptor is not nonblocking",
            ));
        }
        if let Some(read) = read_nonblocking(buffer)? {
            return Ok(Some(read));
        }
        std::thread::sleep(timeout);
        read_nonblocking(buffer)
    }

    #[cfg(not(target_os = "redox"))]
    pub(super) fn read(buffer: &mut [u8], timeout: Duration) -> io::Result<Option<usize>> {
        let milliseconds = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` points to one initialized pollfd for the whole
        // call and the descriptor count exactly matches that allocation.
        let result = unsafe { libc::poll(&raw mut descriptor, 1, milliseconds) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if result == 0 || descriptor.revents & (libc::POLLIN | libc::POLLHUP) == 0 {
            return Ok(None);
        }
        read_nonblocking(buffer)
    }

    fn read_nonblocking(buffer: &mut [u8]) -> io::Result<Option<usize>> {
        // SAFETY: `buffer` is valid for writes of its length and standard
        // input remains owned by the process for the duration of the call.
        let read =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        Ok(Some(read as usize))
    }
}

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

    pub(super) fn status_flags(fd: BorrowedFd<'_>) -> io::Result<libc::c_int> {
        // SAFETY: F_GETFL only inspects the status flags of a live descriptor.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags)
    }

    pub(super) fn apply_status_flags(fd: BorrowedFd<'_>, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: F_SETFL updates only the status flags of a live descriptor.
        let result = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags) };
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
    saved_status: libc::c_int,
    restored: bool,
}

struct PosixTerminalModeToken {
    attributes: libc::termios,
}

impl fmt::Debug for PosixTerminalModeToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PosixTerminalModeToken(..)")
    }
}

impl TerminalModeToken for PosixTerminalModeToken {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl TerminalModeGuard for PosixTerminalModeGuard {
    fn restore(&mut self) -> Result<(), PlatformError> {
        if self.restored {
            return Ok(());
        }
        let stdin = io::stdin();
        let status_result = terminal_mode::apply_status_flags(stdin.as_fd(), self.saved_status);
        let attributes_result = terminal_mode::apply(stdin.as_fd(), &self.saved);
        if let Err(error) = status_result.and(attributes_result) {
            return Err(PlatformError::Unavailable {
                capability: Capability::TerminalInfo,
                reason: format!("restoring terminal mode failed: {error}"),
            });
        }
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
