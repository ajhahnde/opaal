#![forbid(unsafe_code)]

//! FlashOS platform adapter for Flash.
//!
//! The target's existing Rust and `relibc` routes stay inside the concrete
//! adapter. Portable runtime code depends only on [`flash_platform::Platform`]
//! and never learns the Unix-like implementation details used below.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use flash_platform::{
    Capabilities, Capability, ChildProcess, DescriptorEndpoint, DescriptorReadError,
    DirectoryReadError, DirectoryReadRequest, DirectoryStream, FileActionError, FileIoEndpoint,
    FileOpenRequest, ForegroundTerminalGuard, JobControlSignalGuard, JobSignal, PipeEndpoints,
    PipeError, Platform, PlatformError, ProcessGroupId, SignalError, SpawnError, SpawnRequest,
    StandardDirectories, StandardDirectoryEnvironment, TerminalModeGuard, TerminalModeToken,
    TerminalSize, WorkingDirectoryError, WorkingDirectoryRequest,
};
use flash_platform_posix::PosixPlatform;

/// The capabilities whose complete target behavior has been qualified.
///
/// Target runtime bring-up qualifies every classified group except signals.
///
/// FlashOS has not produced a stopped-child transition through the configured
/// target wait route, so the indivisible signals group remains absent rather
/// than advertising group delivery without its required transition vocabulary.
/// The durable capability report and exhaustive target matrix remain separate
/// gates. New groups likewise default to absent until their complete behavior
/// is explicitly brought up and added here.
const QUALIFIED_CAPABILITIES: Capabilities = Capabilities::full_without(Capability::Signals);

/// The concrete FlashOS platform adapter.
#[derive(Clone, Copy, Debug)]
pub struct FlashOsPlatform {
    capabilities: Capabilities,
}

impl Default for FlashOsPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl FlashOsPlatform {
    /// Build the current adapter with only target-qualified capabilities.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            capabilities: QUALIFIED_CAPABILITIES,
        }
    }

    #[cfg(test)]
    const fn with_capabilities(capabilities: Capabilities) -> Self {
        Self { capabilities }
    }
}

/// FlashOS-owned selection of per-user directory roots.
#[derive(Clone, Copy, Debug, Default)]
pub struct FlashOsDirectoryPolicy;

impl FlashOsDirectoryPolicy {
    /// Select native home, configuration, cache, and state paths.
    ///
    /// Absolute `HOME` and XDG roots are preserved byte-for-byte. A missing or
    /// relative home falls back to `/root` for the root user, `/home/<user>` for
    /// a single native user-name component, and `/home/user` otherwise.
    #[must_use]
    pub fn select(environment: &dyn StandardDirectoryEnvironment) -> StandardDirectories {
        let home = absolute_environment_path(environment, "HOME")
            .unwrap_or_else(|| fallback_home(environment));
        let config = absolute_environment_path(environment, "XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"));
        let cache = absolute_environment_path(environment, "XDG_CACHE_HOME")
            .unwrap_or_else(|| home.join(".cache"));
        let state = absolute_environment_path(environment, "XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local/state"));
        StandardDirectories::new(home, config, cache, state)
    }
}

impl Platform for FlashOsPlatform {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn is_terminal(&self) -> bool {
        self.capabilities.supports(Capability::TerminalInfo) && PosixPlatform.is_terminal()
    }

    fn is_output_terminal(&self) -> bool {
        self.capabilities.supports(Capability::TerminalInfo) && PosixPlatform.is_output_terminal()
    }

    fn terminal_size(&self) -> Result<TerminalSize, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        PosixPlatform.terminal_size()
    }

    fn enter_raw_mode(&self) -> Result<Box<dyn TerminalModeGuard>, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        PosixPlatform.enter_raw_mode()
    }

    fn read_terminal_input(
        &self,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> Result<Option<usize>, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        PosixPlatform.read_terminal_input(buffer, timeout)
    }

    fn snapshot_terminal_mode(&self) -> Result<Box<dyn TerminalModeToken>, PlatformError> {
        self.require(Capability::TerminalInfo)?;
        PosixPlatform.snapshot_terminal_mode()
    }

    fn apply_terminal_mode(&self, token: &dyn TerminalModeToken) -> Result<(), PlatformError> {
        self.require(Capability::TerminalInfo)?;
        PosixPlatform.apply_terminal_mode(token)
    }

    fn foreground_process_group(&self) -> Result<Option<ProcessGroupId>, PlatformError> {
        self.require(Capability::ForegroundTerminal)?;
        PosixPlatform.foreground_process_group()
    }

    fn enter_foreground(
        &self,
        group: ProcessGroupId,
    ) -> Result<Box<dyn ForegroundTerminalGuard>, PlatformError> {
        self.require(Capability::ForegroundTerminal)?;
        PosixPlatform.enter_foreground(group)
    }

    fn signal_process_group(
        &self,
        group: ProcessGroupId,
        signal: JobSignal,
    ) -> Result<(), SignalError> {
        self.require(Capability::Signals)?;
        PosixPlatform.signal_process_group(group, signal)
    }

    fn install_job_control_signals(&self) -> Result<Box<dyn JobControlSignalGuard>, PlatformError> {
        self.require(Capability::Signals)?;
        PosixPlatform.install_job_control_signals()
    }

    fn shell_executable(&self) -> Result<PathBuf, PlatformError> {
        self.require(Capability::ShellExecutable)?;
        PosixPlatform.shell_executable()
    }

    fn ignore_hangup(&self) -> Result<(), PlatformError> {
        self.require(Capability::HangupDisposition)?;
        PosixPlatform.ignore_hangup()
    }

    fn standard_directories(
        &self,
        environment: &dyn StandardDirectoryEnvironment,
    ) -> Result<StandardDirectories, PlatformError> {
        self.require(Capability::StandardDirectories)?;
        Ok(FlashOsDirectoryPolicy::select(environment))
    }

    fn resolve_working_directory(
        &self,
        request: WorkingDirectoryRequest<'_>,
    ) -> Result<PathBuf, WorkingDirectoryError> {
        self.require(Capability::WorkingDirectory)?;
        PosixPlatform.resolve_working_directory(request)
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        self.require(Capability::Pipes)?;
        PosixPlatform.pipe()
    }

    fn open_file(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        PosixPlatform.open_file(request)
    }

    fn open_file_io(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn FileIoEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        PosixPlatform.open_file_io(request)
    }

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.require(Capability::FileActions)?;
        PosixPlatform.inherit_descriptor(descriptor)
    }

    fn read_directory(
        &self,
        request: DirectoryReadRequest<'_>,
    ) -> Result<Box<dyn DirectoryStream>, DirectoryReadError> {
        self.require(Capability::DirectoryRead)?;
        PosixPlatform.read_directory(request)
    }

    fn read_descriptor(
        &self,
        endpoint: &dyn DescriptorEndpoint,
        buffer: &mut [u8],
    ) -> Result<usize, DescriptorReadError> {
        self.require(Capability::Pipes)?;
        PosixPlatform.read_descriptor(endpoint, buffer)
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        self.require(Capability::ProcessSpawn)?;
        if request.process_group().requires_capability() {
            self.require(Capability::ProcessGroups)?;
        }
        PosixPlatform.spawn(request)
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

fn fallback_home(environment: &dyn StandardDirectoryEnvironment) -> PathBuf {
    let Some(user) = environment
        .value(OsStr::new("USER"))
        .filter(is_single_normal_component)
    else {
        return PathBuf::from("/home/user");
    };
    if user == OsStr::new("root") {
        PathBuf::from("/root")
    } else {
        Path::new("/home").join(user)
    }
}

fn is_single_normal_component(value: &OsString) -> bool {
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EmptyEnvironment;

    impl StandardDirectoryEnvironment for EmptyEnvironment {
        fn value(&self, _name: &OsStr) -> Option<OsString> {
            None
        }
    }

    #[test]
    fn a_qualified_directory_route_uses_flashos_policy() {
        let platform = FlashOsPlatform::with_capabilities(
            Capabilities::empty().with(Capability::StandardDirectories),
        );

        let directories = platform
            .standard_directories(&EmptyEnvironment)
            .expect("the qualified route selects directories");

        assert_eq!(directories.home(), Path::new("/home/user"));
    }
}
