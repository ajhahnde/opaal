//! Acceptance tests for the POSIX adapter's capability profile and spawn seam.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use opaal_platform::{
    Capability, ChildDescriptor, DirectoryEntry, DirectoryEntryKind, DirectoryReadError,
    DirectoryReadRequest, ExecutableProbe, FileOpenMode, FileOpenRequest, JobSignal, Platform,
    PlatformError, ProcessGroup, ProcessGroupId, ProcessStatus, ProcessTransition, SignalError,
    SpawnError, SpawnRequest, StandardDirectoryEnvironment, TerminalSize, WorkingDirectoryError,
    WorkingDirectoryRequest,
};
use opaal_platform_posix::{OwnedDescriptor, PosixPlatform};
use rustix::fs::{Mode, OFlags, open};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

struct DirectoryEnvironment(Vec<(OsString, OsString)>);

impl StandardDirectoryEnvironment for DirectoryEnvironment {
    fn value(&self, name: &OsStr) -> Option<OsString> {
        self.0
            .iter()
            .find_map(|(key, value)| (key == name).then(|| value.clone()))
    }
}

#[test]
fn posix_platform_supports_every_capability() {
    let platform = PosixPlatform;

    for capability in Capability::ALL {
        assert!(
            platform.capabilities().supports(capability),
            "POSIX adapter should support {capability:?}",
        );
        assert_eq!(platform.require(capability), Ok(()));
    }
}

#[test]
fn the_adapter_names_the_running_executable() {
    let reported = PosixPlatform
        .shell_executable()
        .expect("a POSIX host can name its own executable");
    let expected = std::env::current_exe().expect("the test binary has a path");

    assert_eq!(reported, expected);
}

#[test]
fn executable_probe_accepts_a_regular_executable_file() {
    let temp = TempDir::new("executable-probe-file");
    let target = temp.path().join("program");
    fs::write(&target, b"#!/bin/sh\n").expect("fixture file should be written");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700))
        .expect("fixture should be executable");

    assert!(PosixPlatform.is_executable(target.as_os_str()));
}

#[test]
fn executable_probe_follows_a_symlink_to_an_executable_file() {
    let temp = TempDir::new("executable-probe-symlink");
    let target = temp.path().join("program");
    let link = temp.path().join("program-link");
    fs::write(&target, b"#!/bin/sh\n").expect("fixture file should be written");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700))
        .expect("fixture should be executable");
    symlink("program", &link).expect("fixture symlink should be created");

    assert!(PosixPlatform.is_executable(link.as_os_str()));
}

#[test]
fn executable_probe_rejects_a_directory() {
    let temp = TempDir::new("executable-probe-directory");

    assert!(!PosixPlatform.is_executable(temp.path().as_os_str()));
}

#[test]
fn executable_probe_rejects_a_non_regular_target() {
    let temp = TempDir::new("executable-probe-socket");
    let socket = temp.path().join("socket");
    let _listener = UnixListener::bind(&socket).expect("fixture socket should be bound");

    assert!(!PosixPlatform.is_executable(socket.as_os_str()));
}

#[test]
fn executable_probe_rejects_a_missing_path() {
    let temp = TempDir::new("executable-probe-missing");

    assert!(!PosixPlatform.is_executable(temp.path().join("missing").as_os_str()));
}

#[test]
fn executable_probe_rejects_a_regular_file_without_execute_access() {
    let temp = TempDir::new("executable-probe-permission");
    let target = temp.path().join("program");
    fs::write(&target, b"#!/bin/sh\n").expect("fixture file should be written");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
        .expect("fixture execute bits should be absent");

    assert!(!PosixPlatform.is_executable(target.as_os_str()));
}

#[test]
fn posix_standard_directories_preserve_absolute_native_overrides() {
    let environment = DirectoryEnvironment(vec![
        (OsString::from("HOME"), OsString::from("/users/test")),
        (
            OsString::from("XDG_CONFIG_HOME"),
            OsString::from("/native/config"),
        ),
        (
            OsString::from("XDG_CACHE_HOME"),
            OsString::from("/native/cache"),
        ),
        (
            OsString::from("XDG_STATE_HOME"),
            OsString::from("/native/state"),
        ),
    ]);

    let directories = PosixPlatform
        .standard_directories(&environment)
        .expect("the POSIX host selects all standard directories");

    assert_eq!(directories.home(), Path::new("/users/test"));
    assert_eq!(directories.config(), Path::new("/native/config"));
    assert_eq!(directories.cache(), Path::new("/native/cache"));
    assert_eq!(directories.state(), Path::new("/native/state"));
}

#[test]
fn the_adapter_advertises_both_new_capabilities() {
    let capabilities = PosixPlatform.capabilities();

    assert!(capabilities.supports(Capability::ShellExecutable));
    assert!(capabilities.supports(Capability::HangupDisposition));
}

#[test]
fn a_child_that_installs_the_hangup_ignore_survives_its_own_hangup() {
    const CHILD: &str = "OPAAL_TEST_HANGUP_IGNORE_CHILD";

    if std::env::var_os(CHILD).is_some() {
        PosixPlatform
            .ignore_hangup()
            .expect("the POSIX adapter should install the hang-up ignore");
        // SAFETY: `raise` sends SIGHUP to this disposable subprocess. The
        // disposition was installed immediately above, and no shared parent
        // process state is mutated.
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
        return;
    }

    let status = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args([
            "--exact",
            "a_child_that_installs_the_hangup_ignore_survives_its_own_hangup",
        ])
        .env(CHILD, "1")
        .status()
        .expect("the hang-up probe child should run");

    assert!(
        status.success(),
        "the child should survive after installing the hang-up ignore"
    );
}

#[test]
fn posix_working_directory_resolution_canonicalizes_and_rejects_files() {
    let temp = TempDir::new("working-directory");
    fs::create_dir(temp.path().join("child")).expect("child directory should be created");
    fs::write(temp.path().join("file"), b"not a directory").expect("file should be created");

    let resolved = PosixPlatform
        .resolve_working_directory(WorkingDirectoryRequest::new(
            Path::new("child/.."),
            temp.path(),
        ))
        .expect("directory should resolve through the host");
    assert_eq!(
        resolved,
        fs::canonicalize(temp.path()).expect("temporary directory canonicalizes")
    );

    let error = PosixPlatform
        .resolve_working_directory(WorkingDirectoryRequest::new(Path::new("file"), temp.path()))
        .expect_err("a regular file is not a working directory");
    assert!(matches!(
        error,
        WorkingDirectoryError::Operation {
            kind: std::io::ErrorKind::NotADirectory,
            ..
        }
    ));
}

#[test]
fn owned_descriptors_close_on_drop_and_clones_keep_the_resource_alive() {
    let (owned_end, mut peer) = UnixStream::pair().expect("socket pair should open");
    let descriptor = OwnedDescriptor::adopt(OwnedFd::from(owned_end))
        .expect("descriptor adoption should duplicate with close-on-exec");
    let clone = descriptor
        .try_clone()
        .expect("descriptor cloning should succeed");

    drop(descriptor);
    clone
        .as_fd()
        .try_clone_to_owned()
        .expect("the clone should still be open");
    drop(clone);

    let mut byte = [0_u8; 1];
    assert_eq!(peer.read(&mut byte).expect("peer read should succeed"), 0);
}

#[test]
fn posix_pipe_returns_connected_owned_endpoints_with_prompt_eof() {
    let (reader, writer) = PosixPlatform
        .pipe()
        .expect("pipe creation should succeed")
        .into_parts();
    let reader_clone = reader
        .as_any()
        .downcast_ref::<OwnedDescriptor>()
        .expect("POSIX returns POSIX descriptors")
        .try_clone()
        .expect("the read endpoint should clone")
        .into_owned_fd();
    let writer_clone = writer
        .as_any()
        .downcast_ref::<OwnedDescriptor>()
        .expect("POSIX returns POSIX descriptors")
        .try_clone()
        .expect("the write endpoint should clone")
        .into_owned_fd();
    drop(reader);
    drop(writer);

    let mut reader = fs::File::from(reader_clone);
    let mut writer = fs::File::from(writer_clone);
    writer.write_all(b"pipeline bytes").expect("write succeeds");
    drop(writer);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).expect("read reaches EOF");

    assert_eq!(bytes, b"pipeline bytes");
}

#[test]
fn posix_platform_reads_an_owned_pipe_endpoint_to_eof() {
    let (reader, writer) = PosixPlatform
        .pipe()
        .expect("POSIX creates a connected pipe")
        .into_parts();
    let writer_clone = writer
        .as_any()
        .downcast_ref::<OwnedDescriptor>()
        .expect("POSIX returns POSIX descriptors")
        .try_clone()
        .expect("the write endpoint should clone")
        .into_owned_fd();
    drop(writer);
    let mut file = fs::File::from(writer_clone);
    file.write_all(b"drained bytes").expect("write succeeds");
    drop(file);

    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4];
    loop {
        let amount = PosixPlatform
            .read_descriptor(reader.as_ref(), &mut buffer)
            .expect("pipe read should succeed");
        if amount == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..amount]);
    }

    assert_eq!(bytes, b"drained bytes");
}

#[test]
fn posix_owned_pipe_endpoints_transfer_bytes_in_process() {
    let (mut reader, mut writer) = PosixPlatform
        .pipe()
        .expect("POSIX creates a connected pipe")
        .into_parts();

    assert_eq!(
        writer
            .write(b"bridge bytes")
            .expect("owned pipe write should succeed"),
        12
    );
    drop(writer);

    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4];
    loop {
        let amount = reader
            .read(&mut buffer)
            .expect("owned pipe read should succeed");
        if amount == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..amount]);
    }
    assert_eq!(bytes, b"bridge bytes");
}

#[test]
fn posix_file_actions_preserve_relative_cwd_and_open_modes() {
    let temp = TempDir::new("file-actions");
    fs::write(temp.path().join("target"), b"old").expect("seed file should be written");

    let truncate = PosixPlatform
        .open_file(FileOpenRequest::new(
            Path::new("target"),
            temp.path(),
            FileOpenMode::WriteTruncate,
        ))
        .expect("truncate open should succeed");
    let mut file = fs::File::from(
        truncate
            .as_any()
            .downcast_ref::<OwnedDescriptor>()
            .expect("POSIX returns POSIX descriptors")
            .try_clone()
            .expect("the descriptor should clone")
            .into_owned_fd(),
    );
    file.write_all(b"new").expect("truncate target is writable");
    drop((file, truncate));

    let append = PosixPlatform
        .open_file(FileOpenRequest::new(
            Path::new("target"),
            temp.path(),
            FileOpenMode::WriteAppend,
        ))
        .expect("append open should succeed");
    let mut file = fs::File::from(
        append
            .as_any()
            .downcast_ref::<OwnedDescriptor>()
            .expect("POSIX returns POSIX descriptors")
            .try_clone()
            .expect("the descriptor should clone")
            .into_owned_fd(),
    );
    file.write_all(b"+").expect("append target is writable");
    drop((file, append));

    let input = PosixPlatform
        .open_file(FileOpenRequest::new(
            Path::new("target"),
            temp.path(),
            FileOpenMode::Read,
        ))
        .expect("read open should succeed");
    let mut file = fs::File::from(
        input
            .as_any()
            .downcast_ref::<OwnedDescriptor>()
            .expect("POSIX returns POSIX descriptors")
            .try_clone()
            .expect("the descriptor should clone")
            .into_owned_fd(),
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).expect("input is readable");

    assert_eq!(bytes, b"new+");
}

#[test]
fn posix_in_process_file_endpoints_read_and_write_owned_files() {
    let temp = TempDir::new("file-io");
    fs::write(temp.path().join("input"), [0, 0xff, 7]).expect("seed file should be written");

    let mut input = PosixPlatform
        .open_file_io(FileOpenRequest::new(
            Path::new("input"),
            temp.path(),
            FileOpenMode::Read,
        ))
        .expect("in-process read open should succeed");
    let mut buffer = [0; 8];
    let amount = input
        .read(&mut buffer)
        .expect("owned file read should work");
    assert_eq!(&buffer[..amount], &[0, 0xff, 7]);
    assert_eq!(input.read(&mut buffer).unwrap(), 0);

    let mut output = PosixPlatform
        .open_file_io(FileOpenRequest::new(
            Path::new("output"),
            temp.path(),
            FileOpenMode::WriteTruncate,
        ))
        .expect("in-process write open should succeed");
    assert_eq!(output.write(&[9, 8, 7]).unwrap(), 3);
    drop(output);
    assert_eq!(fs::read(temp.path().join("output")).unwrap(), [9, 8, 7]);
}

#[test]
fn posix_spawn_preserves_native_argv_and_never_invokes_a_shell() {
    let temp = TempDir::new("direct-argv");
    let report = temp.path().join("report.bin");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    symlink(fixture, temp.path().join("argv-probe")).expect("fixture symlink should be created");
    let argv = [
        OsString::from("deliberate-argv-zero"),
        OsString::from("two words"),
        OsString::from("$(must-not-run) ; *"),
        OsString::new(),
        OsString::from_vec(vec![b'n', b'a', 0x80, b't', b'i', b'v', b'e']),
    ];
    let environment = [
        (
            OsString::from("OPAAL_PROBE_REPORT"),
            report.clone().into_os_string(),
        ),
        (
            OsString::from("OPAAL_PROBE_VALUE"),
            OsString::from("exact value"),
        ),
    ];
    let request = SpawnRequest::new(Path::new("./argv-probe"), &argv, &environment, temp.path())
        .expect("the spawn request is valid");

    let mut child = PosixPlatform
        .spawn(&request)
        .expect("the fixture should spawn directly");

    assert!(child.id() > 0);
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));

    let bytes = fs::read(&report).expect("the fixture should write its report");
    let child_cwd = fs::canonicalize(temp.path()).expect("temporary cwd should canonicalize");
    let expected = encode_report(
        &argv,
        &child_cwd,
        OsStr::new("exact value"),
        OsStr::new(""),
        false,
    );
    assert_eq!(bytes, expected);
}

#[test]
fn posix_spawn_preserves_native_environment_names_and_values() {
    let temp = TempDir::new("native-environment");
    let report = temp.path().join("report.bin");
    let native_report = temp.path().join("native.bin");
    let native_name = OsString::from_vec(b"OPAAL_NATIVE_\x80".to_vec());
    let native_value = OsString::from_vec(b"value_\xfe".to_vec());
    let environment = [
        (
            OsString::from("OPAAL_PROBE_REPORT"),
            report.into_os_string(),
        ),
        (
            OsString::from("OPAAL_PROBE_NATIVE_NAME"),
            native_name.clone(),
        ),
        (
            OsString::from("OPAAL_PROBE_NATIVE_REPORT"),
            native_report.clone().into_os_string(),
        ),
        (native_name, native_value.clone()),
    ];
    let argv = [OsString::from("native-environment")];
    let request = SpawnRequest::new(
        Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture")),
        &argv,
        &environment,
        temp.path(),
    )
    .expect("the spawn request is valid");

    let mut child = PosixPlatform
        .spawn(&request)
        .expect("the fixture should spawn");
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));
    assert_eq!(fs::read(native_report).unwrap(), native_value.as_bytes());
}

#[test]
fn posix_nonblocking_wait_preserves_a_live_child_and_caches_completion() {
    let temp = TempDir::new("nonblocking-wait");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let release = temp.path().join("release");
    let argv = [OsString::from("holder")];
    let mut environment = probe_environment(temp.path(), "holder");
    environment.push((
        OsString::from("OPAAL_PROBE_HOLD_UNTIL"),
        release.clone().into_os_string(),
    ));
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid");
    let mut child = PosixPlatform.spawn(&request).expect("the fixture spawns");

    assert_eq!(child.try_wait_for_transition(), Ok(None));
    fs::write(release, b"go").expect("the release marker is writable");

    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(transition) = child
            .try_wait_for_transition()
            .expect("nonblocking observation should succeed")
        {
            break transition;
        }
        assert!(Instant::now() < deadline, "the released child should exit");
        std::thread::yield_now();
    };
    assert_eq!(
        status,
        ProcessTransition::Completed(ProcessStatus::Exited(0))
    );
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));
}

#[test]
fn owned_descriptors_are_not_inherited_across_exec() {
    let temp = TempDir::new("close-on-exec");
    let report = temp.path().join("report.bin");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let (owned_end, _peer) = UnixStream::pair().expect("socket pair should open");
    let descriptor = OwnedDescriptor::adopt(OwnedFd::from(owned_end))
        .expect("descriptor adoption should succeed");
    let descriptor_number = descriptor.as_fd().as_raw_fd();
    let argv = [OsString::from("probe")];
    let environment = [
        (
            OsString::from("OPAAL_PROBE_REPORT"),
            report.clone().into_os_string(),
        ),
        (
            OsString::from("OPAAL_PROBE_FD"),
            OsString::from(descriptor_number.to_string()),
        ),
    ];
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid");

    let mut child = PosixPlatform
        .spawn(&request)
        .expect("the fixture should spawn");
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));

    let bytes = fs::read(&report).expect("the fixture should write its report");
    let child_cwd = fs::canonicalize(temp.path()).expect("temporary cwd should canonicalize");
    assert_eq!(
        bytes,
        encode_report(&argv, &child_cwd, OsStr::new(""), OsStr::new(""), false,)
    );
    drop(descriptor);
}

#[test]
fn posix_spawn_installs_a_deliberate_arbitrary_child_descriptor() {
    let temp = TempDir::new("mapped-fd");
    let report = temp.path().join("report.bin");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let (owned_end, _peer) = UnixStream::pair().expect("socket pair should open");
    let descriptor = OwnedDescriptor::adopt(OwnedFd::from(owned_end))
        .expect("descriptor adoption should succeed");
    let argv = [OsString::from("probe")];
    let environment = [
        (
            OsString::from("OPAAL_PROBE_REPORT"),
            report.clone().into_os_string(),
        ),
        (OsString::from("OPAAL_PROBE_FD"), OsString::from("3")),
    ];
    let mappings = [ChildDescriptor::new(3, &descriptor)];
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid")
        .with_descriptors(&mappings)
        .expect("descriptor 3 has one mapping");

    let mut child = PosixPlatform
        .spawn(&request)
        .expect("the fixture should spawn with descriptor 3");
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));

    let bytes = fs::read(&report).expect("the fixture should write its report");
    let child_cwd = fs::canonicalize(temp.path()).expect("temporary cwd should canonicalize");
    assert_eq!(
        bytes,
        encode_report(&argv, &child_cwd, OsStr::new(""), OsStr::new(""), true)
    );
}

#[test]
fn posix_spawn_surfaces_a_structured_host_error() {
    let temp = TempDir::new("spawn-error");
    let argv = [OsString::from("missing")];
    let environment = [];
    let request = SpawnRequest::new(
        Path::new("./definitely-missing"),
        &argv,
        &environment,
        temp.path(),
    )
    .expect("the spawn request is valid");

    let error = PosixPlatform
        .spawn(&request)
        .expect_err("the missing executable must not spawn");

    assert!(matches!(
        error,
        SpawnError::Operation {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    ));
}

fn encode_report(
    argv: &[OsString],
    cwd: &Path,
    value: &OsStr,
    path: &OsStr,
    fd_open: bool,
) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    let mut report = Vec::new();
    write_field(&mut report, cwd.as_os_str().as_bytes());
    write_field(&mut report, value.as_bytes());
    write_field(&mut report, path.as_bytes());
    report.push(u8::from(fd_open));
    report.extend_from_slice(&(argv.len() as u32).to_le_bytes());
    for argument in argv {
        write_field(&mut report, argument.as_os_str().as_bytes());
    }
    report
}

fn write_field(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u32).to_le_bytes());
    output.extend_from_slice(value);
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("opaal-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("temporary directory should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).expect("temporary directory should be removed");
    }
}

#[test]
fn terminal_size_is_positive_or_falls_back_to_the_default() {
    let platform = PosixPlatform;

    let size = platform
        .terminal_size()
        .expect("terminal info is supported");

    // Under `cargo test` stdin is usually not a tty, so the documented 80x24
    // fallback is the expected answer there; on a tty any real size is valid.
    assert!(size.columns() > 0);
    assert!(size.rows() > 0);
    if !platform.is_terminal() {
        assert_eq!(size, TerminalSize::new(80, 24));
    }
}

#[test]
fn entering_raw_mode_without_a_terminal_reports_unavailable() {
    let platform = PosixPlatform;

    if platform.is_terminal() {
        // A developer running the suite from a terminal: acquisition must
        // succeed and restoring must be idempotent.
        let mut guard = platform.enter_raw_mode().expect("a tty grants raw mode");
        assert!(guard.restore().is_ok());
        assert!(guard.restore().is_ok());
    } else {
        let error = platform.enter_raw_mode().expect_err("stdin is not a tty");
        assert!(
            format!("{error}").contains("terminal"),
            "the diagnostic names the terminal: {error}"
        );
    }
}

#[test]
fn reading_a_directory_reports_each_entry_with_its_kind_and_size() {
    let temp = TempDir::new("read-directory");
    fs::write(temp.path().join("notes.txt"), b"0123456789").expect("file should be written");
    fs::create_dir(temp.path().join("src")).expect("directory should be created");
    symlink("notes.txt", temp.path().join("link")).expect("symlink should be created");
    let platform = PosixPlatform;

    let mut stream = platform
        .read_directory(DirectoryReadRequest::new(temp.path(), Path::new(".")))
        .expect("the directory is readable");
    let mut entries = Vec::new();
    while let Some(entry) = stream.next_entry().expect("the walk should not fail") {
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.name().cmp(right.name()));

    let names: Vec<&OsStr> = entries.iter().map(DirectoryEntry::name).collect();
    assert_eq!(
        names,
        vec![
            OsStr::new("link"),
            OsStr::new("notes.txt"),
            OsStr::new("src")
        ],
    );
    // The link reports itself, not its target: enumeration never follows.
    assert_eq!(entries[0].kind(), DirectoryEntryKind::Symlink);
    assert_eq!(entries[0].size(), None);
    assert_eq!(entries[1].kind(), DirectoryEntryKind::File);
    assert_eq!(entries[1].size(), Some(10));
    assert_eq!(entries[2].kind(), DirectoryEntryKind::Directory);
    assert_eq!(entries[2].size(), None);
}

#[test]
fn a_relative_directory_target_resolves_against_the_stage_working_directory() {
    let temp = TempDir::new("read-directory-relative");
    fs::create_dir(temp.path().join("inner")).expect("directory should be created");
    fs::write(temp.path().join("inner").join("only.txt"), b"x").expect("file should be written");
    let platform = PosixPlatform;

    let mut stream = platform
        .read_directory(DirectoryReadRequest::new(Path::new("inner"), temp.path()))
        .expect("the directory is readable");

    let entry = stream
        .next_entry()
        .expect("the walk should not fail")
        .expect("the directory holds one entry");
    assert_eq!(entry.name(), OsStr::new("only.txt"));
    assert_eq!(stream.next_entry(), Ok(None));
}

#[test]
fn reading_a_missing_directory_reports_the_host_error_category() {
    let temp = TempDir::new("read-directory-missing");
    let platform = PosixPlatform;

    let error = platform
        .read_directory(DirectoryReadRequest::new(
            &temp.path().join("absent"),
            Path::new("."),
        ))
        .expect_err("the directory does not exist");

    assert!(
        matches!(
            error,
            DirectoryReadError::Operation {
                kind: std::io::ErrorKind::NotFound,
                ..
            }
        ),
        "expected a NotFound operation error, got {error:?}",
    );
}

#[test]
fn reading_a_file_as_a_directory_is_a_per_operation_failure() {
    // The capability is present, so this is the method's own error rather than
    // a capability gap.
    let temp = TempDir::new("read-directory-not-a-directory");
    let file = temp.path().join("plain.txt");
    fs::write(&file, b"x").expect("file should be written");
    let platform = PosixPlatform;

    let error = platform
        .read_directory(DirectoryReadRequest::new(&file, Path::new(".")))
        .expect_err("a regular file cannot be enumerated");

    assert!(matches!(error, DirectoryReadError::Operation { .. }));
}

#[test]
fn posix_spawn_places_a_new_leader_in_a_group_named_after_itself() {
    let temp = TempDir::new("group-leader");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let argv = [OsString::from("leader")];
    let environment = probe_environment(temp.path(), "leader");
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid")
        .in_process_group(ProcessGroup::New);

    let mut child = PosixPlatform
        .spawn(&request)
        .expect("the fixture should spawn as a group leader");
    let identifier = child.id();
    let reported = child.process_group().expect("a leader reports its group");
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));

    // The observed group comes from the child's own getpgrp, so this asserts
    // where the process actually landed rather than what the adapter intended.
    assert_eq!(reported.get(), identifier);
    assert_eq!(read_group(temp.path(), identifier), identifier);
    // A freshly created group is named after a live child, so it cannot be the
    // group this test process already belonged to.
    assert_ne!(identifier, u64::from(std::process::id()));
}

#[test]
fn posix_spawn_puts_every_pipeline_member_in_one_group() {
    let temp = TempDir::new("group-members");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let argv = [OsString::from("member")];

    let leader_environment = probe_environment(temp.path(), "leader");
    let leader_request = SpawnRequest::new(fixture, &argv, &leader_environment, temp.path())
        .expect("the spawn request is valid")
        .in_process_group(ProcessGroup::New);
    let mut leader = PosixPlatform
        .spawn(&leader_request)
        .expect("the leader should spawn");
    let group = leader.process_group().expect("a leader reports its group");

    let follower_environment = probe_environment(temp.path(), "follower");
    let follower_request = SpawnRequest::new(fixture, &argv, &follower_environment, temp.path())
        .expect("the spawn request is valid")
        .in_process_group(ProcessGroup::Join(group));
    let mut follower = PosixPlatform
        .spawn(&follower_request)
        .expect("the follower should join the leader's group");

    assert_eq!(leader.wait(), Ok(ProcessStatus::Exited(0)));
    assert_eq!(follower.wait(), Ok(ProcessStatus::Exited(0)));

    assert_eq!(follower.process_group(), Some(group));
    assert_eq!(read_group(temp.path(), leader.id()), group.get());
    assert_eq!(read_group(temp.path(), follower.id()), group.get());
    assert_ne!(follower.id(), leader.id());
}

#[test]
fn posix_spawn_inherits_the_shell_group_when_no_placement_is_asked_for() {
    let temp = TempDir::new("group-inherit");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let argv = [OsString::from("inheritor")];
    let environment = probe_environment(temp.path(), "inheritor");
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid");

    let mut child = PosixPlatform.spawn(&request).expect("the fixture spawns");
    assert_eq!(child.wait(), Ok(ProcessStatus::Exited(0)));

    // An unplaced child stays where fork left it, which is never its own pid.
    assert_eq!(child.process_group(), None);
    assert_ne!(read_group(temp.path(), child.id()), child.id());
}

#[test]
fn posix_spawn_refuses_a_group_outside_the_process_identifier_range() {
    let temp = TempDir::new("group-range");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let argv = [OsString::from("out-of-range")];
    let environment = [];
    let group = ProcessGroupId::new(u64::from(u32::MAX) + 1).expect("the value is nonzero");
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid")
        .in_process_group(ProcessGroup::Join(group));

    match PosixPlatform.spawn(&request) {
        Err(SpawnError::Operation { kind, message }) => {
            assert_eq!(kind, std::io::ErrorKind::InvalidInput);
            assert!(message.contains("process identifier range"), "{message}");
        }
        other => panic!("expected an out-of-range rejection, got {other:?}"),
    }
}

#[test]
fn posix_spawn_reports_a_child_that_cannot_join_its_requested_group() {
    let temp = TempDir::new("group-missing");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let argv = [OsString::from("orphan")];
    let environment = [];
    // A group is named after a live leader, so a very high identifier belongs
    // to no group this process may join.
    let group = ProcessGroupId::new(0x7fff_fffe).expect("the value is nonzero");
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid")
        .in_process_group(ProcessGroup::Join(group));

    assert!(
        PosixPlatform.spawn(&request).is_err(),
        "a child that cannot reach its group must fail its spawn, not run ungrouped",
    );
}

fn probe_environment(directory: &Path, label: &str) -> Vec<(OsString, OsString)> {
    vec![
        (
            OsString::from("OPAAL_PROBE_REPORT"),
            directory
                .join(format!("{label}-report.bin"))
                .into_os_string(),
        ),
        (
            OsString::from("OPAAL_PROBE_GROUP_REPORT"),
            directory.to_path_buf().into_os_string(),
        ),
    ]
}

fn read_group(directory: &Path, process: u64) -> u64 {
    fs::read_to_string(directory.join(format!("{process}.group")))
        .expect("the fixture should report its process group")
        .trim()
        .parse()
        .expect("a process group is an integer")
}

#[test]
fn posix_spawn_gives_a_child_default_signal_dispositions() {
    let temp = TempDir::new("child-dispositions");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let argv = [OsString::from("raiser")];
    let mut environment = probe_environment(temp.path(), "raiser");
    environment.push((OsString::from("OPAAL_PROBE_RAISE"), OsString::from("2")));
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid");

    let mut child = PosixPlatform.spawn(&request).expect("the fixture spawns");
    let identifier = child.id();

    // SIGINT is 2 on every supported host. A child that inherited an ignore or
    // a blocked mask would survive the raise and leave the survival marker.
    assert_eq!(child.wait(), Ok(ProcessStatus::Signaled(2)));
    assert!(
        !temp.path().join(format!("{identifier}.survived")).exists(),
        "a child with default dispositions must not survive raising SIGINT",
    );
}

#[test]
fn an_interactive_shell_survives_the_signals_its_children_still_answer() {
    let temp = TempDir::new("signal-guard");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-signal-guard-fixture"));
    let observer = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let report = temp.path().join("guard-report.txt");
    let argv = [OsString::from("guard")];
    let environment = [
        (
            OsString::from("OPAAL_GUARD_REPORT"),
            report.clone().into_os_string(),
        ),
        (
            OsString::from("OPAAL_GUARD_OBSERVER"),
            observer.to_path_buf().into_os_string(),
        ),
        (
            OsString::from("OPAAL_GUARD_WORKSPACE"),
            temp.path().to_path_buf().into_os_string(),
        ),
    ];
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid");

    let mut child = PosixPlatform.spawn(&request).expect("the fixture spawns");

    // The fixture ends by raising SIGINT with the arrangement already restored,
    // so a clean exit would mean the restore silently did nothing.
    assert_eq!(child.wait(), Ok(ProcessStatus::Signaled(2)));

    let findings = std::fs::read_to_string(&report).expect("the fixture wrote its report");
    assert!(
        findings.contains("shell-survived-interrupt"),
        "an arranged shell must survive its own interrupt: {findings}",
    );
    assert!(
        findings.contains("child-status:Signaled(2)"),
        "a child must not inherit the shell's ignore: {findings}",
    );
}

#[test]
fn foreground_signals_are_forwarded_to_the_owned_group_and_every_child_is_reaped() {
    let temp = TempDir::new("foreground-signals");
    let report = temp.path().join("signal-report.txt");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-foreground-signal-fixture"));
    let observer = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));

    let status = Command::new(fixture)
        .env("OPAAL_SIGNAL_REPORT", &report)
        .env("OPAAL_SIGNAL_OBSERVER", observer)
        .env("OPAAL_SIGNAL_WORKSPACE", temp.path())
        .status()
        .expect("the foreground signal fixture should run");

    assert!(
        status.success(),
        "the forwarding fixture failed: {status:?}"
    );
    assert_eq!(
        fs::read_to_string(report).expect("the fixture should write its report"),
        format!(
            concat!(
                "{}:forwarded-and-reaped\n{}:caught-and-reaped\n{}:ignored-and-reaped\n",
                "{}:forwarded-and-reaped\n{}:caught-and-reaped\n{}:ignored-and-reaped\n",
                "{}:forwarded-and-reaped\n{}:caught-and-reaped\n{}:ignored-and-reaped\n",
                "preactivation-signal-forwarded\nno-signal-has-no-deadline\n",
                "prior-dispositions-restored\n",
                "owned-group-descendant-terminated\n",
                "ignored-same-group-descendant-escalated\n",
                "concurrent-handler-release-safe"
            ),
            libc::SIGHUP,
            libc::SIGHUP,
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGINT,
            libc::SIGINT,
            libc::SIGTERM,
            libc::SIGTERM,
            libc::SIGTERM,
        )
    );
}

#[test]
fn terminal_generated_interrupt_reaps_the_job_and_returns_the_pty() {
    run_terminal_interrupt_case(false);
}

#[test]
fn terminal_generated_interrupt_escalates_an_ignored_child_and_returns_the_pty() {
    run_terminal_interrupt_case(true);
}

fn run_terminal_interrupt_case(ignored: bool) {
    let temp = TempDir::new("foreground-pty");
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open PTY master");
    // Keep terminal output draining while the session leader exits; a PTY
    // close can otherwise wait for an echoed interrupt byte to be consumed.
    // SAFETY: fcntl changes only the flags of this test's live PTY master.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        assert!(flags >= 0);
        assert_eq!(
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
            0
        );
    }
    grantpt(&master).expect("grant PTY slave");
    unlockpt(&master).expect("unlock PTY slave");
    let name = ptsname(&master, Vec::new()).expect("name PTY slave");
    let slave = std::fs::File::from(
        open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        )
        .expect("open PTY slave"),
    );
    // A newly opened PTY may inherit a host-specific terminal mode. Make the
    // interrupt byte an actual terminal-generated signal on both hosts.
    // SAFETY: the termios pointer is live and the slave descriptor is valid.
    unsafe {
        let mut attributes: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &raw mut attributes), 0);
        attributes.c_lflag |= libc::ISIG | libc::ICANON;
        attributes.c_cc[libc::VINTR] = 3;
        assert_eq!(
            libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &raw const attributes),
            0
        );
    }
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-foreground-signal-fixture"));
    let observer = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let mut command = Command::new(fixture);
    command
        .env("OPAAL_PTY_WORKSPACE", temp.path())
        .env("OPAAL_SIGNAL_OBSERVER", observer)
        .stdin(Stdio::from(slave.try_clone().expect("clone PTY slave")))
        .stdout(Stdio::from(slave.try_clone().expect("clone PTY slave")))
        .stderr(Stdio::from(slave));
    if ignored {
        command.env("OPAAL_PTY_IGNORE", "1");
    }
    // SAFETY: only async-signal-safe POSIX operations run in the child before
    // exec. Standard input already refers to the PTY slave.
    unsafe {
        command.pre_exec(|| {
            #[cfg(target_os = "linux")]
            let controlling_terminal_request = libc::TIOCSCTTY;
            #[cfg(target_os = "macos")]
            let controlling_terminal_request = libc::c_ulong::from(libc::TIOCSCTTY);
            if libc::setsid() == -1 || libc::ioctl(0, controlling_terminal_request, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("start the isolated PTY session");
    let shell_group = i32::try_from(child.id()).expect("the child PID fits a process group");
    let deadline = Instant::now() + Duration::from_secs(10);
    let transferred = loop {
        // SAFETY: tcgetpgrp reads the foreground group of the live PTY master.
        let group = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
        if group > 0 && group != shell_group {
            break group;
        }
        if let Some(status) = child.try_wait().expect("observe the PTY fixture") {
            panic!("PTY fixture exited before terminal handoff: {status}");
        }
        assert!(Instant::now() < deadline, "PTY handoff timed out");
        std::thread::sleep(Duration::from_millis(5));
    };
    while !temp.path().join("pty-child-report").exists() {
        assert!(
            Instant::now() < deadline,
            "PTY observer readiness timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let reported_group = fs::read_dir(temp.path())
        .expect("read PTY observer reports")
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "group"))
        .expect("observer should report its process group");
    assert_eq!(
        fs::read_to_string(reported_group.path()).expect("read observer group"),
        transferred.to_string(),
    );
    // SAFETY: write addresses the live PTY master with one initialized byte.
    let sent = unsafe { libc::write(master.as_raw_fd(), b"\x03".as_ptr().cast(), 1) };
    assert_eq!(sent, 1);
    let signaled_at = Instant::now();
    let status = loop {
        let mut output = [0u8; 4096];
        // SAFETY: read writes at most the size of the live output buffer.
        let _ = unsafe { libc::read(master.as_raw_fd(), output.as_mut_ptr().cast(), output.len()) };
        if let Some(status) = child.try_wait().expect("observe the PTY fixture") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("terminate a stalled PTY fixture");
            panic!("PTY interrupt timed out for group {transferred}");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success(), "PTY fixture failed: {status}");
    if ignored {
        assert!(signaled_at.elapsed() >= Duration::from_millis(1800));
        assert!(signaled_at.elapsed() < Duration::from_secs(5));
    }
    assert_eq!(
        fs::read_to_string(temp.path().join("pty-result")).expect("read the PTY result"),
        if ignored {
            "ignored-interrupt-escalated-and-restored"
        } else {
            "interrupted-and-restored"
        }
    );
}

#[test]
fn posix_observes_a_stopped_child_and_resumes_it_through_its_group() {
    let temp = TempDir::new("group-stop");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    let release = temp.path().join("release");
    let argv = [OsString::from("holder")];
    let mut environment = probe_environment(temp.path(), "holder");
    environment.push((
        OsString::from("OPAAL_PROBE_HOLD_UNTIL"),
        release.clone().into_os_string(),
    ));
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid")
        .in_process_group(ProcessGroup::New);

    let mut child = PosixPlatform.spawn(&request).expect("the fixture spawns");
    let group = child.process_group().expect("a leader reports its group");

    PosixPlatform
        .signal_process_group(group, JobSignal::Stop)
        .expect("a POSIX host stops a group");
    assert_eq!(
        child.wait_for_transition(),
        Ok(ProcessTransition::Stopped {
            signal: libc::SIGSTOP
        }),
        "the stop is reported with the host's SIGSTOP number",
    );

    PosixPlatform
        .signal_process_group(group, JobSignal::Continue)
        .expect("a POSIX host resumes a group");
    assert_eq!(
        child.wait_for_transition(),
        Ok(ProcessTransition::Continued),
        "the continuation is reported before the held child can complete",
    );
    std::fs::write(&release, b"go").expect("the release marker is writable");

    assert_eq!(
        child.wait_for_transition(),
        Ok(ProcessTransition::Completed(ProcessStatus::Exited(0))),
    );
}

#[test]
fn posix_hangs_up_a_group_and_observes_the_termination() {
    let temp = TempDir::new("group-hangup");
    let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
    // The release marker is never written: the fixture holds until the hang-up
    // ends it, which is what makes the observed status attributable to the
    // signal rather than to the fixture finishing on its own.
    let release = temp.path().join("release");
    let argv = [OsString::from("holder")];
    let mut environment = probe_environment(temp.path(), "holder");
    environment.push((
        OsString::from("OPAAL_PROBE_HOLD_UNTIL"),
        release.into_os_string(),
    ));
    let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
        .expect("the spawn request is valid")
        .in_process_group(ProcessGroup::New);

    let mut child = PosixPlatform.spawn(&request).expect("the fixture spawns");
    let group = child.process_group().expect("a leader reports its group");

    PosixPlatform
        .signal_process_group(group, JobSignal::Hangup)
        .expect("a POSIX host hangs up a group");

    assert_eq!(
        child.wait_for_transition(),
        Ok(ProcessTransition::Completed(ProcessStatus::Signaled(
            libc::SIGHUP
        ))),
        "a child with the default disposition dies from the hang-up",
    );
}

#[test]
fn posix_maps_portable_termination_signals_to_the_exact_host_signals() {
    for (signal, expected, label) in [
        (JobSignal::Interrupt, libc::SIGINT, "interrupt"),
        (JobSignal::Terminate, libc::SIGTERM, "terminate"),
        (JobSignal::Kill, libc::SIGKILL, "kill"),
    ] {
        let temp = TempDir::new(label);
        let fixture = Path::new(env!("CARGO_BIN_EXE_opaal-process-observer-fixture"));
        let release = temp.path().join("release");
        let argv = [OsString::from("holder")];
        let mut environment = probe_environment(temp.path(), label);
        environment.push((
            OsString::from("OPAAL_PROBE_HOLD_UNTIL"),
            release.into_os_string(),
        ));
        let request = SpawnRequest::new(fixture, &argv, &environment, temp.path())
            .expect("the spawn request is valid")
            .in_process_group(ProcessGroup::New);

        let mut child = PosixPlatform.spawn(&request).expect("the fixture spawns");
        let group = child.process_group().expect("a leader reports its group");

        PosixPlatform
            .signal_process_group(group, signal)
            .expect("a POSIX host signals the disposable group");

        assert_eq!(
            child.wait_for_transition(),
            Ok(ProcessTransition::Completed(ProcessStatus::Signaled(
                expected
            ))),
            "{signal:?} must map to host signal {expected}",
        );
    }
}

#[test]
fn posix_reports_a_signal_to_a_group_that_does_not_exist() {
    // A group is named after a live leader, so a very high identifier belongs
    // to no group on this host.
    let group = ProcessGroupId::new(0x7fff_fffe).expect("the value is nonzero");

    match PosixPlatform.signal_process_group(group, JobSignal::Continue) {
        // The host rejects delivery to a group with no members. The errno for
        // that is ESRCH, which the standard library maps to its unnameable
        // catch-all category rather than to a distinct kind, so the failure is
        // matched by shape: it is an operation error, not a capability gap.
        Err(SignalError::Operation { .. }) => {}
        other => panic!("expected a missing-group failure, got {other:?}"),
    }
}

#[test]
fn posix_refuses_the_group_whose_target_means_every_process() {
    // Group 1 negates to the target the host reads as "every process the caller
    // may signal". A stop delivered there would freeze the session, so the
    // adapter must refuse it instead of handing it to the host.
    let group = ProcessGroupId::new(1).expect("the value is nonzero");

    match PosixPlatform.signal_process_group(group, JobSignal::Stop) {
        Err(SignalError::Operation { kind, .. }) => {
            assert_eq!(kind, std::io::ErrorKind::InvalidInput);
        }
        other => panic!("expected the broadcast target to be refused, got {other:?}"),
    }
}

#[test]
fn posix_reports_no_terminal_owner_when_standard_input_is_not_a_terminal() {
    // The test harness runs with a redirected standard input, which is exactly
    // the non-interactive shape a script or a pipeline sees.
    assert_eq!(PosixPlatform.foreground_process_group(), Ok(None));
}

#[test]
fn posix_refuses_a_terminal_handover_without_a_terminal() {
    let group = ProcessGroupId::new(1).expect("the group is usable");

    match PosixPlatform.enter_foreground(group) {
        Err(PlatformError::Unavailable { capability, reason }) => {
            // Unavailable, not Unsupported: the adapter has the capability and
            // this session simply has no terminal to hand over.
            assert_eq!(capability, Capability::ForegroundTerminal);
            assert!(reason.contains("not a terminal"), "{reason}");
        }
        other => panic!("expected an unavailable terminal, got {other:?}"),
    }
}
