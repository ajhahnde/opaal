//! Acceptance tests for the POSIX adapter's capability profile and spawn seam.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flashshell_platform::{
    Capability, ChildDescriptor, FileOpenMode, FileOpenRequest, Platform, ProcessStatus,
    SpawnError, SpawnRequest, WorkingDirectoryError, WorkingDirectoryRequest,
};
use flashshell_platform_posix::{OwnedDescriptor, PosixPlatform};

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
fn posix_spawn_preserves_native_argv_and_never_invokes_a_shell() {
    let temp = TempDir::new("direct-argv");
    let report = temp.path().join("report.bin");
    let fixture = Path::new(env!("CARGO_BIN_EXE_flashshell-process-observer-fixture"));
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
            OsString::from("FLASH_PROBE_REPORT"),
            report.clone().into_os_string(),
        ),
        (
            OsString::from("FLASH_PROBE_VALUE"),
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
fn owned_descriptors_are_not_inherited_across_exec() {
    let temp = TempDir::new("close-on-exec");
    let report = temp.path().join("report.bin");
    let fixture = Path::new(env!("CARGO_BIN_EXE_flashshell-process-observer-fixture"));
    let (owned_end, _peer) = UnixStream::pair().expect("socket pair should open");
    let descriptor = OwnedDescriptor::adopt(OwnedFd::from(owned_end))
        .expect("descriptor adoption should succeed");
    let descriptor_number = descriptor.as_fd().as_raw_fd();
    let argv = [OsString::from("probe")];
    let environment = [
        (
            OsString::from("FLASH_PROBE_REPORT"),
            report.clone().into_os_string(),
        ),
        (
            OsString::from("FLASH_PROBE_FD"),
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
    let fixture = Path::new(env!("CARGO_BIN_EXE_flashshell-process-observer-fixture"));
    let (owned_end, _peer) = UnixStream::pair().expect("socket pair should open");
    let descriptor = OwnedDescriptor::adopt(OwnedFd::from(owned_end))
        .expect("descriptor adoption should succeed");
    let argv = [OsString::from("probe")];
    let environment = [
        (
            OsString::from("FLASH_PROBE_REPORT"),
            report.clone().into_os_string(),
        ),
        (OsString::from("FLASH_PROBE_FD"), OsString::from("3")),
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
            std::env::temp_dir().join(format!("flashshell-{label}-{}-{nonce}", std::process::id()));
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
